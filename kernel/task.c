#include <nanox/diag.h>
#include <nanox/elf_plan.h>
#include <nanox/printf.h>
#include <nanox/string.h>
#include <nanox/syscall.h>

#include "arch/x86_64/cpu.h"
#include "arch/x86_64/timer.h"
#include "arch/x86_64/trap.h"
#include "kernel.h"
#include "mm/mm.h"
#include "obj/ipc.h"
#include "obj/vmo.h"
#include "task.h"

struct nx_task *nx_current;
uint64_t nx_sched_switches;
uint64_t nx_sched_preemptions;

static struct nx_task tasks[NX_TASK_MAX];
static struct nx_task *idle;
static uint32_t next_id = 1;
static int preempt_enabled;
static uint64_t watchdog_deadline;
static const char *watchdog_what;

/* ---- helpers ------------------------------------------------------------ */

static uint64_t irq_save(void)
{
    uint64_t f = nx_read_rflags();
    nx_cli();
    return f;
}

static void irq_restore(uint64_t f)
{
    if (f & NX_RFLAGS_IF)
        nx_sti();
}

static void release_slot(struct nx_object *o)
{
    struct nx_task *t = (struct nx_task *)o;
    t->state = NX_TASK_FREE;
}

static struct nx_task *alloc_task(const char *name)
{
    for (uint32_t s = 0; s < NX_TASK_MAX; s++) {
        struct nx_task *t = &tasks[s];
        if (t->state != NX_TASK_FREE)
            continue;
        memset(t, 0, sizeof(*t));
        t->base.type = NX_OBJ_TASK;
        t->base.refs = 1; /* held until the task is reaped */
        t->base.destroy = release_slot;
        t->id = next_id++;
        t->slot = s;
        uint32_t i = 0;
        for (; name[i] && i + 1 < NX_TASK_NAME; i++)
            t->name[i] = name[i];
        t->name[i] = 0;
        t->state = NX_TASK_BLOCKED; /* created, not started */
        nx_ht_init(&t->handles);
        return t;
    }
    return 0;
}

static uint64_t kstack_bottom(const struct nx_task *t)
{
    return NX_KSTACK_BASE + t->slot * NX_KSTACK_STRIDE + NX_PAGE_4K; /* above the guard */
}

static void kstack_free(struct nx_task *t)
{
    for (uint32_t i = 0; i < NX_KSTACK_PAGES; i++) {
        uint64_t pa;
        if (nx_vmm_unmap_page(kstack_bottom(t) + i * NX_PAGE_4K, &pa) == NX_PT_OK)
            nx_page_free(pa);
    }
    t->kstack_top = 0;
}

static int kstack_alloc(struct nx_task *t)
{
    for (uint32_t i = 0; i < NX_KSTACK_PAGES; i++) {
        uint64_t pa = nx_pmm_alloc(&nx_pmm);
        if (!pa || nx_vmm_map_page(kstack_bottom(t) + i * NX_PAGE_4K, pa,
                                   NX_PTE_W | NX_PTE_NX | NX_PTE_G) != NX_PT_OK) {
            if (pa)
                nx_page_free(pa);
            t->kstack_top = 1; /* mark partially allocated for kstack_free */
            kstack_free(t);
            return NX_ENOMEM;
        }
        memset(nx_phys_to_virt(pa), 0, NX_PAGE_4K);
    }
    t->kstack_top = kstack_bottom(t) + NX_KSTACK_PAGES * NX_PAGE_4K;
    return NX_OK;
}

int nx_task_stack_readable(uint64_t rbp)
{
    if (!nx_kernel_root || rbp < NX_KSTACK_BASE ||
        rbp >= NX_KSTACK_BASE + NX_TASK_MAX * NX_KSTACK_STRIDE)
        return 0;
    uint64_t pa, flags, size;
    return nx_vmm_query(rbp, &pa, &flags, &size) == NX_PT_OK &&
           nx_vmm_query(rbp + 15, &pa, &flags, &size) == NX_PT_OK;
}

const char *nx_task_guard_of(uint64_t addr)
{
    if (addr < NX_KSTACK_BASE || addr >= NX_KSTACK_BASE + NX_TASK_MAX * NX_KSTACK_STRIDE)
        return 0;
    uint64_t off = (addr - NX_KSTACK_BASE) % NX_KSTACK_STRIDE;
    struct nx_task *t = &tasks[(addr - NX_KSTACK_BASE) / NX_KSTACK_STRIDE];
    return off < NX_PAGE_4K && t->state != NX_TASK_FREE ? t->name : 0;
}

/* ---- scheduler ---------------------------------------------------------- */

static struct nx_task *pick_next(void)
{
    uint32_t start = nx_current->slot;
    for (uint32_t i = 1; i <= NX_TASK_MAX; i++) {
        struct nx_task *t = &tasks[(start + i) % NX_TASK_MAX];
        if (t->state == NX_TASK_READY && t != idle)
            return t;
    }
    if (nx_current->state == NX_TASK_RUNNING && nx_current != idle)
        return nx_current;
    return idle;
}

void nx_schedule(void)
{
    struct nx_task *prev = nx_current, *next = pick_next();
    if (next == prev) {
        prev->slice = NX_SCHED_SLICE;
        return;
    }
    if (prev->state == NX_TASK_RUNNING)
        prev->state = NX_TASK_READY;
    next->state = NX_TASK_RUNNING;
    next->slice = NX_SCHED_SLICE;
    nx_current = next;
    nx_tss_set_rsp0(next->kstack_top);
    if (next->cr3 != prev->cr3)
        nx_write_cr3(next->cr3);
    nx_sched_switches++;
    nx_ctx_switch(&prev->saved_rsp, next->saved_rsp);
}

static int other_ready(void)
{
    for (uint32_t s = 0; s < NX_TASK_MAX; s++)
        if (tasks[s].state == NX_TASK_READY && &tasks[s] != idle && &tasks[s] != nx_current)
            return 1;
    return 0;
}

void nx_sched_watchdog(uint64_t ticks, const char *what)
{
    watchdog_what = what;
    watchdog_deadline = ticks ? nx_timer_ticks + ticks : 0;
}

/* Timer interrupt (EOI already sent, IF = 0). */
static void sched_tick(struct nx_trap_frame *f)
{
    (void)f;
    nx_current->ticks++;
    if (nx_current->slice)
        nx_current->slice--;
    if (watchdog_deadline && nx_timer_ticks >= watchdog_deadline) {
        nx_printf("NANOX: TEST FAIL watchdog: %s not finished in time (running %s#%u,"
                  " preemption %s)\n",
                  watchdog_what, nx_current->name, nx_current->id, preempt_enabled ? "on" : "off");
        nx_debug_exit(NX_EXIT_TEST_FAIL);
    }
    if (!preempt_enabled && nx_current != idle)
        return;
    if ((nx_current == idle || nx_current->slice == 0) && other_ready()) {
        if (nx_current != idle) {
            nx_current->preempted++;
            nx_sched_preemptions++;
        }
        nx_schedule();
    }
}

void nx_yield(void)
{
    uint64_t f = irq_save();
    nx_current->slice = 0;
    nx_schedule();
    irq_restore(f);
}

void nx_task_block(void *on)
{
    nx_current->state = NX_TASK_BLOCKED;
    nx_current->blocked_on = on;
    nx_schedule();
}

void nx_task_wake(struct nx_task *t)
{
    if (t->state == NX_TASK_BLOCKED) {
        t->state = NX_TASK_READY;
        t->blocked_on = 0;
    }
}

static int64_t idle_main(void *arg)
{
    (void)arg;
    for (;;) {
        __asm__ volatile("sti; hlt; cli" : : : "memory");
        nx_schedule();
    }
}

static void prepare_kthread(struct nx_task *t, int64_t (*fn)(void *), void *arg)
{
    extern char nx_kthread_trampoline[];
    uint64_t *sp = (uint64_t *)(uintptr_t)t->kstack_top;
    *--sp = (uint64_t)(uintptr_t)nx_kthread_trampoline;
    *--sp = 0x2;                       /* RFLAGS: IF = 0 (kernel threads are not preempted) */
    *--sp = 0;                         /* rbp */
    *--sp = 0;                         /* rbx */
    *--sp = (uint64_t)(uintptr_t)fn;   /* r12 */
    *--sp = (uint64_t)(uintptr_t)arg;  /* r13 */
    *--sp = 0;                         /* r14 */
    *--sp = 0;                         /* r15 */
    t->saved_rsp = (uint64_t)(uintptr_t)sp;
}

struct nx_task *nx_kthread_create(const char *name, int64_t (*fn)(void *), void *arg)
{
    uint64_t f = irq_save();
    struct nx_task *t = alloc_task(name);
    if (t && kstack_alloc(t) != NX_OK) {
        t->state = NX_TASK_FREE;
        t = 0;
    }
    irq_restore(f);
    if (!t)
        return 0;
    t->cr3 = nx_kernel_root;
    prepare_kthread(t, fn, arg);
    return t;
}

void nx_sched_init(int preempt)
{
    nx_cli(); /* kernel code runs with interrupts disabled from here on */
    struct nx_task *k = alloc_task("kmain");
    k->state = NX_TASK_RUNNING;
    k->cr3 = nx_kernel_root;
    k->kstack_top = (uint64_t)(uintptr_t)__boot_stack_top;
    k->slice = NX_SCHED_SLICE;
    nx_current = k;
    nx_tss_set_rsp0(k->kstack_top);
    idle = nx_kthread_create("idle", idle_main, 0);
    if (!idle)
        nx_panic("sched: cannot create the idle thread");
    idle->state = NX_TASK_READY;
    /* The kernel does not save FPU/SSE state on a switch, so user code must
     * not have any: x87 instructions raise #NM (CR0.EM) and SSE/AVX raise
     * #UD (CR0.EM, CR4.OSFXSR/OSXSAVE clear).  The kernel is built with
     * -mgeneral-regs-only. */
    nx_write_cr0((nx_read_cr0() | NX_CR0_EM | NX_CR0_MP) & ~NX_CR0_TS);
    nx_write_cr4(nx_read_cr4() & ~(NX_CR4_OSFXSR | NX_CR4_OSXMMEXCPT | NX_CR4_OSXSAVE));
    preempt_enabled = preempt;
    nx_timer_start_periodic(sched_tick);
    nx_printf("NANOX: sched ok tasks=kmain#%u,idle#%u preempt=%s slice_ticks=%u hz=%u"
              " user_fpu=off gdt_user=0x1b/0x23 syscall_vector=0x%x\n",
              k->id, idle->id, preempt ? "on" : "off", NX_SCHED_SLICE, NX_TIMER_HZ,
              NX_VEC_SYSCALL);
}

/* ---- user tasks --------------------------------------------------------- */

static void free_owned_leaf(void *ctx, uint64_t pa, uint64_t flags, uint64_t size)
{
    (void)ctx;
    if ((flags & NX_PTE_OWNED) && size == NX_PAGE_4K)
        nx_page_free(pa);
}

static int map_new_page(uint64_t root, uint64_t va, uint64_t flags, uint64_t *pa_out)
{
    uint64_t pa = nx_pmm_alloc(&nx_pmm);
    if (!pa)
        return NX_ENOMEM;
    memset(nx_phys_to_virt(pa), 0, NX_PAGE_4K);
    int st = nx_vmm_map_page_in(root, va, pa, flags | NX_PTE_U | NX_PTE_OWNED);
    if (st != NX_PT_OK) {
        nx_page_free(pa);
        return st == NX_PT_E_NOMEM ? NX_ENOMEM : NX_EINVAL;
    }
    *pa_out = pa;
    return NX_OK;
}

static int load_elf(uint64_t root, const uint8_t *elf, uint64_t size, uint64_t *entry)
{
    static struct nx_elf_plan plan; /* single CPU, interrupts off: no reentry */
    if (nx_elf_plan_user(elf, size, &plan) != NX_ELF_OK)
        return NX_EINVAL;
    for (uint32_t i = 0; i < plan.segment_count; i++) {
        const struct nx_elf_segment *s = &plan.segments[i];
        uint64_t flags = (s->flags & 2 ? NX_PTE_W : 0) | (s->flags & 1 ? 0 : NX_PTE_NX);
        uint64_t first = s->addr & ~(NX_PAGE_4K - 1);
        uint64_t end = (s->addr + s->mem_size + NX_PAGE_4K - 1) & ~(NX_PAGE_4K - 1);
        for (uint64_t va = first; va < end; va += NX_PAGE_4K) {
            uint64_t pa;
            int st = map_new_page(root, va, flags, &pa);
            if (st != NX_OK)
                return st;
            uint64_t lo = va > s->addr ? va : s->addr;
            uint64_t hi = s->addr + s->file_size;
            if (hi > va + NX_PAGE_4K)
                hi = va + NX_PAGE_4K;
            if (lo < hi)
                memcpy((uint8_t *)nx_phys_to_virt(pa) + (lo - va),
                       elf + s->file_offset + (lo - s->addr), hi - lo);
        }
    }
    *entry = plan.entry;
    return NX_OK;
}

static struct nx_trap_frame *user_frame(struct nx_task *t)
{
    return (struct nx_trap_frame *)(uintptr_t)(t->kstack_top - sizeof(struct nx_trap_frame));
}

struct nx_task *nx_utask_create(const char *name, const uint8_t *elf, uint64_t size,
                                const uint64_t args[4], int *err)
{
    uint64_t f = irq_save();
    struct nx_task *t = alloc_task(name);
    if (!t) {
        irq_restore(f);
        *err = NX_ENOMEM;
        return 0;
    }
    int st = kstack_alloc(t);
    irq_restore(f);
    if (st != NX_OK) {
        t->state = NX_TASK_FREE;
        *err = st;
        return 0;
    }
    t->user = 1;
    t->cr3 = nx_as_create();
    uint64_t entry = 0;
    st = t->cr3 ? load_elf(t->cr3, elf, size, &entry) : NX_ENOMEM;
    for (uint32_t i = 0; st == NX_OK && i < NX_USER_STACK_PAGES; i++) {
        uint64_t pa;
        st = map_new_page(t->cr3, NX_USER_STACK_TOP - (i + 1) * NX_PAGE_4K, NX_PTE_W | NX_PTE_NX,
                          &pa);
    }
    if (st != NX_OK) {
        if (t->cr3)
            nx_as_destroy(t->cr3, free_owned_leaf, 0);
        kstack_free(t);
        t->state = NX_TASK_FREE;
        *err = st;
        return 0;
    }
    t->entry = entry;
    struct nx_trap_frame *fr = user_frame(t);
    memset(fr, 0, sizeof(*fr));
    fr->rdi = args[0];
    fr->rsi = args[1];
    fr->rdx = args[2];
    fr->rcx = args[3];
    fr->rip = entry;
    fr->cs = NX_SEL_UCODE;
    fr->rflags = 0x202; /* IF = 1, IOPL = 0 */
    fr->rsp = NX_USER_STACK_TOP;
    fr->ss = NX_SEL_UDATA;
    uint64_t *sp = (uint64_t *)fr;
    *--sp = (uint64_t)(uintptr_t)nx_trap_return;
    *--sp = 0x2; /* RFLAGS for nx_ctx_switch; iretq restores the user RFLAGS */
    for (int i = 0; i < 6; i++)
        *--sp = 0;
    t->saved_rsp = (uint64_t)(uintptr_t)sp;
    return t;
}

void nx_task_set_arg(struct nx_task *t, unsigned index, uint64_t value)
{
    struct nx_trap_frame *fr = user_frame(t);
    if (index == 0)
        fr->rdi = value;
    else if (index == 1)
        fr->rsi = value;
    else if (index == 2)
        fr->rdx = value;
    else if (index == 3)
        fr->rcx = value;
}

void nx_task_start(struct nx_task *t)
{
    uint64_t f = irq_save();
    if (t->state == NX_TASK_BLOCKED && !t->blocked_on)
        t->state = NX_TASK_READY;
    irq_restore(f);
}

/* ---- ending and reaping ------------------------------------------------- */

static void end_task(struct nx_task *t, int end)
{
    struct nx_endpoint *ep = t->blocked_on;
    if (t->state == NX_TASK_BLOCKED && ep && ep->waiter == t)
        ep->waiter = 0;
    t->blocked_on = 0;
    t->end = end;
    t->state = NX_TASK_DEAD;
    if (t->waiter)
        nx_task_wake(t->waiter);
}

void nx_task_exit_current(int64_t code)
{
    nx_cli();
    nx_current->exit_code = code;
    end_task(nx_current, NX_END_EXIT);
    nx_schedule();
    nx_panic("sched: dead task %s resumed", nx_current->name);
}

void nx_task_terminate(struct nx_task *t, int end, uint32_t killer_id)
{
    uint64_t f = irq_save();
    if (t->state == NX_TASK_FREE || t->state == NX_TASK_DEAD || t->state == NX_TASK_REAPED) {
        irq_restore(f);
        return;
    }
    t->killer_id = killer_id;
    end_task(t, end);
    if (t == nx_current) {
        nx_schedule();
        nx_panic("sched: terminated task %s resumed", t->name);
    }
    irq_restore(f);
}

void nx_user_trap(struct nx_trap_frame *f)
{
    struct nx_task *t = nx_current;
    t->fault_vector = f->vector;
    t->fault_error = f->error;
    t->fault_rip = f->rip;
    t->fault_cr2 = nx_read_cr2();
    nx_task_terminate(t, NX_END_FAULT, 0);
    nx_panic("sched: faulted task resumed");
}

static void print_end(const struct nx_task *t)
{
    switch (t->end) {
    case NX_END_EXIT:
        nx_printf("NANOX: task %s#%u exited code=%" NX_PRIu64 " ticks=%" NX_PRIu64
                  " preempted=%" NX_PRIu64 " syscalls=%" NX_PRIu64 "\n",
                  t->name, t->id, (uint64_t)t->exit_code, t->ticks, t->preempted, t->syscalls);
        break;
    case NX_END_FAULT:
        nx_printf("NANOX: task %s#%u killed: user fault %s vector=%" NX_PRIu64
                  " error=0x%" NX_PRIx64 " rip=0x%016" NX_PRIx64 " cr2=0x%016" NX_PRIx64 "\n",
                  t->name, t->id, nx_exception_mnemonic(t->fault_vector), t->fault_vector,
                  t->fault_error, t->fault_rip, t->fault_cr2);
        break;
    case NX_END_KILLED:
        nx_printf("NANOX: task %s#%u killed: by task #%u through a task handle\n", t->name,
                  t->id, t->killer_id);
        break;
    default:
        nx_printf("NANOX: task %s#%u terminated by the kernel\n", t->name, t->id);
        break;
    }
}

static void reap(struct nx_task *t)
{
    print_end(t);
    nx_ht_clear(&t->handles);
    /* Unmap first, then drop the memory objects the mappings referenced. */
    if (t->user)
        nx_as_destroy(t->cr3, free_owned_leaf, 0);
    for (uint32_t i = 0; i < NX_TASK_MAPS; i++) {
        if (!t->maps[i].vmo)
            continue;
        nx_obj_unref(&t->maps[i].vmo->base);
        t->maps[i].vmo = 0;
    }
    kstack_free(t);
    t->state = NX_TASK_REAPED;
    nx_obj_unref(&t->base);
}

int64_t nx_task_wait_reap(struct nx_task *t)
{
    uint64_t f = irq_save();
    while (t->state != NX_TASK_DEAD && t->state != NX_TASK_REAPED) {
        t->waiter = nx_current;
        nx_task_block(0);
    }
    t->waiter = 0;
    int64_t code = t->end == NX_END_FAULT ? -(int64_t)t->fault_vector : t->exit_code;
    if (t->state == NX_TASK_DEAD)
        reap(t);
    irq_restore(f);
    return code;
}

struct nx_task *nx_task_by_id(uint32_t id)
{
    for (uint32_t s = 0; s < NX_TASK_MAX; s++)
        if (tasks[s].state != NX_TASK_FREE && tasks[s].id == id)
            return &tasks[s];
    return 0;
}

uint32_t nx_task_live_count(void)
{
    uint32_t n = 0;
    for (uint32_t s = 0; s < NX_TASK_MAX; s++)
        n += tasks[s].state != NX_TASK_FREE && tasks[s].state != NX_TASK_REAPED;
    return n;
}
