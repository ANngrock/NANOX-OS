/*
 * Tasks and the preemptive scheduler (M2).  Single CPU.  Design and rules:
 * docs/m2-kernel.md.
 *
 * A task is a kernel object (handles of type NX_OBJ_TASK refer to it).  A
 * kernel thread runs in ring 0 on the kernel address space; a user task has
 * its own address space (PML4 slots 1..255) and enters ring 3 through
 * nx_trap_return.  Every task has its own kernel stack with an unmapped
 * guard page.  Scheduling is round-robin over READY tasks with a time slice
 * of NX_SCHED_SLICE timer ticks; the timer preempts user code.  The kernel
 * itself is not preemptible: kernel code (system calls, kernel threads)
 * runs with interrupts disabled and switches only when it blocks, yields or
 * exits; the idle thread enables interrupts only around `hlt`.
 */
#ifndef NANOX_KERNEL_TASK_H
#define NANOX_KERNEL_TASK_H

#include <stdint.h>

#include "obj/handle.h"

#define NX_TASK_MAX 32u
#define NX_TASK_NAME 16u
#define NX_TASK_MAPS 8u
#define NX_SCHED_SLICE 2u /* timer ticks (10 ms each) */

enum nx_task_state {
    NX_TASK_FREE = 0,
    NX_TASK_READY,
    NX_TASK_RUNNING,
    NX_TASK_BLOCKED,
    NX_TASK_DEAD,   /* exited or killed, resources not yet released */
    NX_TASK_REAPED, /* resources released; slot kept while handles refer to it */
};

enum nx_task_end {
    NX_END_NONE = 0,
    NX_END_EXIT,      /* NX_SYS_EXIT or kernel thread returned */
    NX_END_FAULT,     /* CPU exception in user mode */
    NX_END_KILLED,    /* NX_SYS_TASK_KILL through a task handle */
    NX_END_KERNEL,    /* terminated by the kernel (test controller) */
};

struct nx_vmo;

struct nx_task_map {
    struct nx_vmo *vmo; /* reference held while mapped */
    uint64_t va;
    uint32_t pages;
};

struct nx_task {
    struct nx_object base; /* type NX_OBJ_TASK */
    uint32_t id;           /* sequential, never reused */
    uint32_t slot;
    char name[NX_TASK_NAME];
    int state;
    int user;
    uint64_t cr3;
    uint64_t entry; /* user tasks: ELF entry point */
    uint64_t kstack_top;
    uint64_t saved_rsp;
    uint32_t slice;
    uint64_t ticks;       /* timer ticks while running */
    uint64_t preempted;   /* involuntary switches away from this task */
    uint64_t syscalls;
    uint64_t yields;      /* NX_SYS_YIELD calls */
    /* NX_SYS_DEBUG_WRITE calls and their positions in the global sequence
     * (nx_debug_write_seq): used to check that tasks ran interleaved. */
    uint64_t writes, first_write_seq, last_write_seq;
    struct nx_task *waiter;
    void *blocked_on;     /* endpoint while blocked in receive */
    struct nx_handle_table handles;
    struct nx_task_map maps[NX_TASK_MAPS];
    /* How the task ended. */
    int end;
    int64_t exit_code;
    uint64_t fault_vector, fault_error, fault_rip, fault_cr2;
    uint32_t killer_id;
};

extern struct nx_task *nx_current;

/* Turns the running boot context into kernel thread "kmain", creates the
 * idle thread and starts preemption (unless `preempt` is 0). */
void nx_sched_init(int preempt);
struct nx_task *nx_kthread_create(const char *name, int64_t (*fn)(void *), void *arg);
/* Loads an ELF program into a new address space; arguments arrive in
 * RDI, RSI, RDX, RCX.  Returns NULL and sets *err (NX_E*) on failure. */
struct nx_task *nx_utask_create(const char *name, const uint8_t *elf, uint64_t size,
                                const uint64_t args[4], int *err);
/* Replaces argument `index` (0..3) of a created, not yet started user task. */
void nx_task_set_arg(struct nx_task *t, unsigned index, uint64_t value);
/* Makes a created task runnable. */
void nx_task_start(struct nx_task *t);
/* Test watchdog: if it is still armed `ticks` timer ticks from now, the
 * timer interrupt reports TEST FAIL naming `what`.  0 disarms it. */
void nx_sched_watchdog(uint64_t ticks, const char *what);

void nx_schedule(void);
void nx_yield(void);
/* Blocks the current task until nx_task_wake; interrupts must be disabled. */
void nx_task_block(void *on);
void nx_task_wake(struct nx_task *t);

__attribute__((noreturn)) void nx_task_exit_current(int64_t code);
/* Ends task t (any state except FREE/REAPED); returns if t is not current. */
void nx_task_terminate(struct nx_task *t, int end, uint32_t killer_id);
/* Waits until t is DEAD, then releases its resources (handles, address
 * space, mappings, kernel stack) and prints its end record.  Returns the
 * exit code (or -vector for faults).  The slot itself stays allocated
 * (state REAPED) while references to the task object remain. */
int64_t nx_task_wait_reap(struct nx_task *t);
struct nx_task *nx_task_by_id(uint32_t id);
uint32_t nx_task_live_count(void);

/* User-mode CPU exception: records it and terminates the current task. */
struct nx_trap_frame;
__attribute__((noreturn)) void nx_user_trap(struct nx_trap_frame *f);
void nx_syscall(struct nx_trap_frame *f);

/* Scheduler statistics. */
extern uint64_t nx_sched_switches;
extern uint64_t nx_sched_preemptions;
/* Global count of NX_SYS_DEBUG_WRITE calls (kernel/syscall.c). */
extern uint64_t nx_debug_write_seq;

/* True when [rbp, rbp+16) lies in a mapped task kernel stack. */
int nx_task_stack_readable(uint64_t rbp);
/* Name of the task whose kernel-stack guard page contains addr, or NULL. */
const char *nx_task_guard_of(uint64_t addr);

#endif
