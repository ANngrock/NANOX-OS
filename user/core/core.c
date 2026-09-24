/*
 * bin/core — the Cognitive Core executor of M3 (docs/m3-core.md).
 *
 * Serves NCI requests (nci.h) that the host bridge sends over the bridge
 * channel and executes them through the kernel's control interface with
 * the Sovereign handle it received from the kernel.  Every request becomes
 * a task-engine record (engine.h) that goes through OBSERVING (read the
 * current state, check references and preconditions), PLANNED (the kernel
 * call is chosen), RUNNING (the call), VERIFYING (the end state is read
 * back from the kernel: task state, the kernel's state events, handle
 * release) and ends SUCCEEDED or FAILED.  A successful system call alone
 * never makes an action SUCCEEDED.  Every transition is written to the
 * serial log (NX_SYS_DEBUG_WRITE) as the guest-side trace.
 *
 * a0 = Sovereign handle, a1 = channel handle (0: none), a2 = NX_M3_CORE_*
 * and NX_M4_CORE_* flags, a3 = block device handle of the data disk (0:
 * no store, as in M3).  Exit codes: abi/nanox/m3.h, abi/nanox/m4.h.
 *
 * M4: with a data disk the executor keeps configuration, blobs, the commit
 * history and its own task-engine records in the store (persist.c,
 * docs/m4-store.md); requests of an earlier boot are answered from the
 * stored records, interrupted ones as OUTCOME_UNKNOWN.
 */
#include <nanox/m3.h>
#include <nanox/m4.h>
#include <nanox/string.h>

#include "core.h"
#include "engine.h"
#include "nanox_user.h"
#include "nci.h"

#define READ_TIMEOUT_TICKS 500u /* one channel read */
#define IDLE_LIMIT_READS 24u    /* 24 x 5 s without a request: the host is gone */
#define REAP_WAIT_TICKS 200u    /* terminate: time allowed for the end state */
#define RUN_WAIT_TICKS 100u     /* spawn: time allowed until the task has run */
#define MEASURE_MAX_MS 5000u
#define KNOWN_MAX 24u
#define SUBS_MAX 4u
#define LIST_MAX 32u

static uint64_t sov, chan, hz;
uint64_t boot_id;
static uint32_t self_id;
static int nodedup;
struct engine eng;
uint64_t verify_failures;
static char self_ref[NCI_REF_MAX];

/* Tasks this executor started: kernel handle per task id. */
static struct {
    uint32_t id;
    uint32_t h;
} known[KNOWN_MAX];
static uint64_t subs[SUBS_MAX];
static uint32_t nsubs;

static struct nx_task_info list[LIST_MAX];
static struct nx_event evbuf[32];

/* ---- channel -------------------------------------------------------------- */

static char rx[1024];
static uint32_t rx_len;
static int discarding; /* inside an overlong line */

/* Without a bridge (M4 workload and check modes) the response lines go to
 * the serial log instead ("out <line>", RES lines only). */
static void write_all(const char *p, uint32_t len)
{
    if (!chan) {
        while (len) {
            uint32_t n = 0;
            while (n < len && p[n] != '\n')
                n++;
            if (n >= 3 && p[0] == 'R' && p[1] == 'E' && p[2] == 'S') {
                char line[208];
                uint32_t k = n < 200 ? n : 200;
                memcpy(line, p, k);
                line[k] = 0;
                u_printf("out %s\n", line);
            }
            p += n < len ? n + 1 : n;
            len -= n < len ? n + 1 : n;
        }
        return;
    }
    while (len) {
        uint32_t n = len > NX_CHAN_IO_MAX ? NX_CHAN_IO_MAX : len;
        if (nx_chan_write(chan, p, n) < 0)
            return;
        p += n;
        len -= n;
    }
}

/* 1: a line in `line` (without '\n'), 0: timeout, -1: an overlong line was
 * dropped, -2: channel error. */
static int read_line(char *line, uint32_t cap, uint32_t *len)
{
    for (;;) {
        for (uint32_t i = 0; i < rx_len; i++) {
            if (rx[i] != '\n')
                continue;
            int was_discarding = discarding;
            discarding = 0;
            int ok = !was_discarding && i < cap;
            if (ok) {
                memcpy(line, rx, i);
                *len = i;
            }
            memmove(rx, rx + i + 1, rx_len - i - 1);
            rx_len -= i + 1;
            return ok ? 1 : -1;
        }
        if (rx_len == sizeof(rx)) { /* no newline in a full buffer */
            discarding = 1;
            rx_len = 0;
        }
        uint32_t space = (uint32_t)sizeof(rx) - rx_len;
        int64_t n = nx_chan_read(chan, rx + rx_len, space > NX_CHAN_IO_MAX ? NX_CHAN_IO_MAX : space,
                                 READ_TIMEOUT_TICKS);
        if (n < 0)
            return -2;
        if (n == 0)
            return 0;
        rx_len += (uint32_t)n;
    }
}

/* ---- response helpers ------------------------------------------------------ */

static char outmem[ENG_RESULT_MAX];

void res_reset(struct nci_buf *b)
{
    nb_init(b, outmem, sizeof(outmem));
}

void res_begin(struct nci_buf *b, const char *id, const char *state)
{
    nb_init(b, outmem, sizeof(outmem));
    nb_str(b, "RES ");
    nb_str(b, id);
    nb_char(b, ' ');
    nb_str(b, state);
}

void item_begin(struct nci_buf *b, const char *id)
{
    nb_str(b, "\nITEM ");
    nb_str(b, id);
}

void res_end(struct nci_buf *b, const char *id)
{
    nb_str(b, "\nEND ");
    nb_str(b, id);
    nb_char(b, '\n');
}

/* ---- trace ------------------------------------------------------------------ */

void step(struct eng_action *a, int to, const char *detail)
{
    int from = a->state;
    if (eng_advance(a, to) != 0) {
        u_printf("act %s %s ILLEGAL %s -> %s\n", a->id, a->op, eng_state_name(from),
                 eng_state_name(to));
        verify_failures++; /* an executor bug must not pass silently */
        return;
    }
    u_printf("act %s %s %s%s%s\n", a->id, a->op, eng_state_name(to), detail[0] ? " " : "",
             detail);
}

/* Ends the action FAILED with `code` (and an optional detail value). */
void fail_action(struct eng_action *a, struct nci_buf *b, const char *code,
                 const char *detail, const char *effects)
{
    char t[160];
    struct nci_buf tb;
    nb_init(&tb, t, sizeof(t));
    nb_str(&tb, "code=");
    nb_str(&tb, code);
    if (detail) {
        nb_str(&tb, " detail=");
        nb_str(&tb, detail);
    }
    nb_str(&tb, " effects=");
    nb_str(&tb, effects);
    step(a, ACT_FAILED, t);
    res_begin(b, a->id, "FAILED");
    nb_kv(b, "code", code);
    if (detail)
        nb_kv(b, "detail", detail);
    nb_kv(b, "effects", effects);
    res_end(b, a->id);
}

/* ---- kernel state ------------------------------------------------------------ */

static const char *state_name(uint32_t s)
{
    switch (s) {
    case NX_TS_READY: return "ready";
    case NX_TS_RUNNING: return "running";
    case NX_TS_BLOCKED: return "blocked";
    case NX_TS_DEAD: return "dead";
    case NX_TS_REAPED: return "reaped";
    default: return "unknown";
    }
}

static const char *end_name(uint32_t e)
{
    switch (e) {
    case NX_TE_NONE: return "none";
    case NX_TE_EXIT: return "exit";
    case NX_TE_FAULT: return "fault";
    case NX_TE_KILLED: return "killed";
    case NX_TE_KERNEL: return "kernel";
    default: return "unknown";
    }
}

static const char *event_name(uint32_t t)
{
    switch (t) {
    case NX_EV_TASK_CREATED: return "created";
    case NX_EV_TASK_STARTED: return "started";
    case NX_EV_TASK_EXITED: return "exited";
    case NX_EV_TASK_KILLED: return "killed";
    case NX_EV_TASK_FAULTED: return "faulted";
    case NX_EV_TASK_REAPED: return "reaped";
    default: return "unknown";
    }
}

static int alive(uint32_t state)
{
    return state == NX_TS_READY || state == NX_TS_RUNNING || state == NX_TS_BLOCKED;
}

static uint64_t events_next(void)
{
    struct nx_sys_info si;
    nx_sys_info(&si);
    return si.events_next;
}

/* True if the kernel recorded an event of `type` for `task` at or after `since`. */
static int event_seen(uint64_t since, uint32_t task, uint32_t type)
{
    for (;;) {
        int64_t n = nx_sov_event_read(sov, since, evbuf, 32);
        if (n <= 0)
            return 0;
        for (int64_t i = 0; i < n; i++)
            if (evbuf[i].task == task && evbuf[i].type == type)
                return 1;
        since = evbuf[n - 1].seq + 1;
    }
}

/* Finds task `id` in the kernel's task list. */
static int find_task(uint32_t id, struct nx_task_info *out)
{
    int64_t n = nx_sov_task_list(sov, list, LIST_MAX);
    for (int64_t i = 0; i < n && i < LIST_MAX; i++)
        if (list[i].id == id) {
            *out = list[i];
            return 1;
        }
    return 0;
}

enum { REF_OK, REF_BAD, REF_STALE, REF_GONE, REF_NOT_FOUND };

static const char *const REF_CODES[] = {"OK", "BAD_REQUEST", "STALE_REF", "GONE", "NOT_FOUND"};

/* Resolves the task reference in argument `key`.  REF_GONE: the task ended
 * (info filled if it is still listed); ids below next_task_id that are no
 * longer listed ended as well, since ids are never reused within a boot. */
static int resolve(const struct nci_req *r, const char *key, uint32_t *id,
                   struct nx_task_info *info, int *listed)
{
    const char *v = nci_get(r, key);
    uint64_t b;
    *listed = 0;
    if (!v || !nci_ref_parse(v, &b, id))
        return REF_BAD;
    if (b != boot_id)
        return REF_STALE;
    if (find_task(*id, info)) {
        *listed = 1;
        return alive(info->state) ? REF_OK : REF_GONE;
    }
    struct nx_sys_info si;
    nx_sys_info(&si);
    return *id < si.next_task_id ? REF_GONE : REF_NOT_FOUND;
}

static int known_find(uint32_t id)
{
    for (uint32_t i = 0; i < KNOWN_MAX; i++)
        if (known[i].id == id)
            return (int)i;
    return -1;
}

static void describe_ref(struct nci_buf *b, const char *key, uint32_t id)
{
    char ref[NCI_REF_MAX];
    nci_ref_format(ref, boot_id, id);
    nb_kv(b, key, ref);
}

/* ---- operations ------------------------------------------------------------------ */

static const char OPS[] = "system.describe,task.list,task.inspect,task.spawn,task.measure,"
                          "task.terminate,memory.stats,event.subscribe,event.poll,"
                          "action.status,session.close,";

static void op_describe(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    (void)r;
    struct nx_sys_info si;
    step(a, ACT_OBSERVING, "");
    step(a, ACT_PLANNED, "call=SYS_INFO");
    step(a, ACT_RUNNING, "");
    int64_t st = nx_sys_info(&si);
    if (st < 0) {
        fail_action(a, b, "KERNEL_ERROR", u_err(st), "none");
        return;
    }
    step(a, ACT_VERIFYING, "read-only");
    step(a, ACT_SUCCEEDED, "");
    res_begin(b, a->id, "SUCCEEDED");
    nb_kv_u64(b, "nci", 1);
    nb_str(b, " boot=");
    nb_hex(b, si.boot_id, 16);
    nb_kv(b, "core", self_ref);
    nb_kv_u64(b, "hz", si.hz);
    nb_kv_u64(b, "ticks", si.ticks);
    nb_kv_u64(b, "tasks", si.tasks);
    nb_kv_u64(b, "next_task_id", si.next_task_id);
    nb_kv_u64(b, "events_next", si.events_next);
    nb_kv(b, "dedup", nodedup ? "off" : "on");
    nb_kv_u64(b, "actions", eng.started);
    nb_str(b, " ops=");
    nb_str(b, OPS);
    nb_str(b, PS_OPS);
    if (ps_state != PS_ABSENT) {
        nb_kv(b, "store", ps_state_name());
        nb_kv_u64(b, "store_gen", ps_gen());
    }
    nb_kv(b, "verify", "n/a");
    res_end(b, a->id);
}

static void task_fields(struct nci_buf *b, const struct nx_task_info *t)
{
    describe_ref(b, "ref", t->id);
    nb_kv(b, "name", t->name);
    nb_kv(b, "kind", t->flags & NX_TASK_INFO_USER ? "user" : "kernel");
    nb_kv(b, "state", state_name(t->state));
    nb_kv_u64(b, "ticks", t->ticks);
    nb_kv_u64(b, "rev", t->rev);
}

static void op_list(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    (void)r;
    step(a, ACT_OBSERVING, "");
    step(a, ACT_PLANNED, "call=SOV_TASK_LIST");
    step(a, ACT_RUNNING, "");
    int64_t n = nx_sov_task_list(sov, list, LIST_MAX);
    if (n < 0) {
        fail_action(a, b, "KERNEL_ERROR", u_err(n), "none");
        return;
    }
    /* Consistency of the observation: the executor must see itself running. */
    int self_seen = 0;
    for (int64_t i = 0; i < n && i < LIST_MAX; i++)
        self_seen |= list[i].id == self_id && list[i].state == NX_TS_RUNNING;
    if (!self_seen) {
        verify_failures++;
        step(a, ACT_VERIFYING, "");
        fail_action(a, b, "VERIFY_FAILED", "executor_not_listed", "none");
        return;
    }
    step(a, ACT_VERIFYING, "self_listed=yes");
    step(a, ACT_SUCCEEDED, "");
    res_begin(b, a->id, "SUCCEEDED");
    nb_kv_u64(b, "count", (uint64_t)n);
    nb_kv(b, "verify", "ok");
    for (int64_t i = 0; i < n && i < LIST_MAX; i++) {
        item_begin(b, a->id);
        task_fields(b, &list[i]);
    }
    res_end(b, a->id);
}

static void op_inspect(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    uint32_t id;
    int listed;
    struct nx_task_info t;
    step(a, ACT_OBSERVING, "");
    int rs = resolve(r, "target", &id, &t, &listed);
    if (rs != REF_OK && !(rs == REF_GONE && listed)) {
        fail_action(a, b, REF_CODES[rs], 0, "none");
        return;
    }
    step(a, ACT_PLANNED, "call=SOV_TASK_LIST");
    step(a, ACT_RUNNING, "");
    step(a, ACT_VERIFYING, "read-only");
    step(a, ACT_SUCCEEDED, "");
    res_begin(b, a->id, "SUCCEEDED");
    task_fields(b, &t);
    nb_kv_u64(b, "syscalls", t.syscalls);
    nb_kv(b, "end", end_name(t.end));
    nb_kv_i64(b, "exit_code", t.exit_code);
    nb_kv_u64(b, "created_tick", t.created_tick);
    nb_kv_u64(b, "ended_tick", t.ended_tick);
    nb_kv(b, "verify", "n/a");
    res_end(b, a->id);
}

static void op_spawn(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    char path[NX_SPAWN_PATH_MAX + 1], detail[96];
    step(a, ACT_OBSERVING, "");
    const char *prog = nci_get(r, "program");
    uint64_t plen = prog ? u_strlen(prog) : 0;
    if (!prog || plen == 0 || plen + 4 > NX_SPAWN_PATH_MAX) {
        fail_action(a, b, "BAD_REQUEST", "program", "none");
        return;
    }
    int slot = known_find(0);
    if (slot < 0) {
        fail_action(a, b, "NO_RESOURCES", "known_table", "none");
        return;
    }
    memcpy(path, "bin/", 4);
    memcpy(path + 4, prog, plen + 1);
    uint64_t e0 = events_next();
    struct nci_buf db;
    nb_init(&db, detail, sizeof(detail));
    nb_str(&db, "call=SOV_TASK_SPAWN path=");
    nb_str(&db, path);
    step(a, ACT_PLANNED, detail);
    if (ps_intent(a) != 0) {
        fail_action(a, b, "STORE_ERROR", "intent_not_saved", "none");
        return;
    }
    step(a, ACT_RUNNING, "");
    int64_t h = nx_sov_task_spawn(sov, path, plen + 4, 0);
    if (h < 0) {
        fail_action(a, b, h == -NX_ENOENT ? "NOT_FOUND" : "KERNEL_ERROR", u_err(h), "none");
        return;
    }
    /* End state: a user task of that name exists, has actually run, and the
     * kernel recorded its creation and start. */
    struct nx_task_info t;
    int64_t st = nx_task_info((uint64_t)h, &t);
    step(a, ACT_VERIFYING, "");
    for (uint32_t w = 0; st == 0 && alive(t.state) && t.ticks == 0 && t.syscalls == 0 &&
                         w < RUN_WAIT_TICKS;
         w++) {
        nx_sleep(1);
        st = nx_task_info((uint64_t)h, &t);
    }
    known[slot].id = st == 0 ? t.id : 0;
    known[slot].h = (uint32_t)h;
    const char *why = 0;
    if (st != 0)
        why = "task_info_failed";
    else if (!(t.flags & NX_TASK_INFO_USER) || !nci_streq(t.name, prog))
        why = "wrong_task";
    else if (!alive(t.state))
        why = "not_alive";
    else if (t.ticks == 0 && t.syscalls == 0)
        why = "never_ran";
    else if (!event_seen(e0, t.id, NX_EV_TASK_CREATED))
        why = "no_created_event";
    else if (!event_seen(e0, t.id, NX_EV_TASK_STARTED))
        why = "no_started_event";
    if (why) {
        verify_failures++;
        fail_action(a, b, "VERIFY_FAILED", why, "applied");
        return;
    }
    nb_init(&db, detail, sizeof(detail));
    nb_str(&db, "task=");
    nb_u64(&db, t.id);
    nb_str(&db, " state=");
    nb_str(&db, state_name(t.state));
    nb_str(&db, " ran=yes events=created,started");
    step(a, ACT_SUCCEEDED, detail);
    res_begin(b, a->id, "SUCCEEDED");
    task_fields(b, &t);
    nb_kv(b, "verify", "ok");
    nb_kv(b, "checks", "user_task,name,alive,ran,event_created,event_started");
    res_end(b, a->id);
}

static void op_measure(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    uint32_t id;
    int listed;
    struct nx_task_info t0, t1;
    struct nx_sys_info s0, s1;
    char detail[64];
    step(a, ACT_OBSERVING, "");
    uint64_t ms = 0;
    if (nci_get_u64(r, "window_ms", &ms) != 1 || ms == 0 || ms > MEASURE_MAX_MS) {
        fail_action(a, b, "BAD_REQUEST", "window_ms", "none");
        return;
    }
    int rs = resolve(r, "target", &id, &t0, &listed);
    if (rs != REF_OK) {
        fail_action(a, b, REF_CODES[rs], 0, "none");
        return;
    }
    int k = known_find(id), temp = k < 0;
    int64_t h = temp ? nx_sov_task_open(sov, id) : known[k].h;
    if (h < 0) {
        fail_action(a, b, "KERNEL_ERROR", u_err(h), "none");
        return;
    }
    uint64_t window = (ms * hz + 999u) / 1000u;
    struct nci_buf db;
    nb_init(&db, detail, sizeof(detail));
    nb_str(&db, "call=TASK_INFO,SLEEP window_ticks=");
    nb_u64(&db, window);
    step(a, ACT_PLANNED, detail);
    step(a, ACT_RUNNING, "");
    int64_t e = nx_task_info((uint64_t)h, &t0);
    nx_sys_info(&s0);
    nx_sleep(window);
    if (e == 0)
        e = nx_task_info((uint64_t)h, &t1);
    nx_sys_info(&s1);
    if (temp)
        nx_handle_close((uint64_t)h);
    step(a, ACT_VERIFYING, "");
    if (e != 0) {
        fail_action(a, b, "KERNEL_ERROR", u_err(e), "none");
        return;
    }
    if (!alive(t1.state)) {
        fail_action(a, b, "TARGET_ENDED", state_name(t1.state), "none");
        return;
    }
    uint64_t w = s1.ticks - s0.ticks, d = t1.ticks - t0.ticks;
    if (t1.id != id || t1.ticks < t0.ticks || w < window || d > w) {
        verify_failures++;
        fail_action(a, b, "VERIFY_FAILED", "inconsistent_counters", "none");
        return;
    }
    nb_init(&db, detail, sizeof(detail));
    nb_str(&db, "cpu_ticks=");
    nb_u64(&db, d);
    nb_str(&db, " window_ticks=");
    nb_u64(&db, w);
    step(a, ACT_SUCCEEDED, detail);
    res_begin(b, a->id, "SUCCEEDED");
    describe_ref(b, "ref", id);
    nb_kv(b, "state", state_name(t1.state));
    nb_kv_u64(b, "window_ticks", w);
    nb_kv_u64(b, "cpu_ticks", d);
    nb_kv_u64(b, "share_pct", w ? d * 100 / w : 0);
    nb_kv_u64(b, "ticks_before", t0.ticks);
    nb_kv_u64(b, "ticks_after", t1.ticks);
    nb_kv_u64(b, "syscalls", t1.syscalls - t0.syscalls);
    nb_kv(b, "verify", "ok");
    res_end(b, a->id);
}

static void op_terminate(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    uint32_t id;
    int listed;
    struct nx_task_info t;
    char detail[96];
    step(a, ACT_OBSERVING, "");
    int rs = resolve(r, "target", &id, &t, &listed);
    if (rs != REF_OK) {
        fail_action(a, b, REF_CODES[rs], 0, "none");
        return;
    }
    if (!(t.flags & NX_TASK_INFO_USER) || id == self_id) {
        fail_action(a, b, "DENIED", id == self_id ? "self" : "kernel_thread", "none");
        return;
    }
    uint64_t want_rev;
    int has_rev = nci_get_u64(r, "expect_rev", &want_rev);
    if (has_rev < 0) {
        fail_action(a, b, "BAD_REQUEST", "expect_rev", "none");
        return;
    }
    if (has_rev && want_rev != t.rev) {
        char rv[24];
        struct nci_buf rb;
        nb_init(&rb, rv, sizeof(rv));
        nb_str(&rb, "rev_");
        nb_u64(&rb, t.rev);
        fail_action(a, b, "CONFLICT", rv, "none");
        return;
    }
    int k = known_find(id);
    int64_t h = k >= 0 ? known[k].h : nx_sov_task_open(sov, id);
    if (h < 0) {
        fail_action(a, b, "KERNEL_ERROR", u_err(h), "none");
        return;
    }
    uint64_t e0 = events_next();
    struct nci_buf db;
    nb_init(&db, detail, sizeof(detail));
    nb_str(&db, "call=TASK_KILL h=0x");
    nb_hex(&db, (uint64_t)h, 0);
    nb_str(&db, " rev=");
    nb_u64(&db, t.rev);
    step(a, ACT_PLANNED, detail);
    if (ps_intent(a) != 0) {
        if (k < 0)
            nx_handle_close((uint64_t)h);
        fail_action(a, b, "STORE_ERROR", "intent_not_saved", "none");
        return;
    }
    step(a, ACT_RUNNING, "");
    int64_t kr = nx_task_kill((uint64_t)h);
    if (kr < 0) {
        /* The kernel refused before changing anything (see syscall.c). */
        if (k < 0)
            nx_handle_close((uint64_t)h);
        fail_action(a, b, "KERNEL_ERROR", u_err(kr), "none");
        return;
    }
    step(a, ACT_VERIFYING, "result=ok");
    /* End state: the task ended by our kill, its resources were released
     * (REAPED), the kernel recorded both, and our handle is gone. */
    int64_t st = nx_task_info((uint64_t)h, &t);
    for (uint32_t w = 0; st == 0 && t.state != NX_TS_REAPED && w < REAP_WAIT_TICKS; w++) {
        nx_sleep(1);
        st = nx_task_info((uint64_t)h, &t);
    }
    const char *why = 0;
    if (st != 0)
        why = "task_info_failed";
    else if (alive(t.state))
        why = "still_running";
    else if (t.state != NX_TS_REAPED)
        why = "not_reaped";
    else if (t.end != NX_TE_KILLED || t.killer_id != self_id)
        why = "wrong_end";
    else if (!event_seen(e0, id, NX_EV_TASK_KILLED))
        why = "no_killed_event";
    else if (!event_seen(e0, id, NX_EV_TASK_REAPED))
        why = "no_reaped_event";
    if (why) {
        /* Keep the handle: the task may still be running and must stay
         * reachable for a later request. */
        if (k < 0)
            nx_handle_close((uint64_t)h);
        verify_failures++;
        fail_action(a, b, "VERIFY_FAILED", why, "unknown");
        return;
    }
    nx_handle_close((uint64_t)h);
    struct nx_task_info gone;
    int released = nx_task_info((uint64_t)h, &gone) == -NX_EBADHANDLE;
    struct nx_task_info again;
    int still_listed = find_task(id, &again) && alive(again.state);
    if (k >= 0)
        known[k].id = 0;
    if (!released || still_listed) {
        verify_failures++;
        fail_action(a, b, "VERIFY_FAILED", released ? "still_listed" : "handle_not_released",
                    "applied");
        return;
    }
    step(a, ACT_SUCCEEDED, "state=reaped end=killed events=killed,reaped handle=released");
    res_begin(b, a->id, "SUCCEEDED");
    describe_ref(b, "ref", id);
    nb_kv(b, "state", "reaped");
    nb_kv(b, "end", "killed");
    nb_kv_u64(b, "rev", t.rev);
    nb_kv_u64(b, "ticks", t.ticks);
    nb_kv(b, "verify", "ok");
    nb_kv(b, "checks", "not_alive,reaped,end_killed_by_core,event_killed,event_reaped,"
                       "handle_released,not_listed");
    res_end(b, a->id);
}

static void op_memory(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    (void)r;
    struct nx_sys_info si;
    step(a, ACT_OBSERVING, "");
    step(a, ACT_PLANNED, "call=SYS_INFO");
    step(a, ACT_RUNNING, "");
    int64_t st = nx_sys_info(&si);
    if (st < 0) {
        fail_action(a, b, "KERNEL_ERROR", u_err(st), "none");
        return;
    }
    step(a, ACT_VERIFYING, "read-only");
    step(a, ACT_SUCCEEDED, "");
    res_begin(b, a->id, "SUCCEEDED");
    nb_kv_u64(b, "free_pages", si.free_pages);
    nb_kv_u64(b, "managed_pages", si.managed_pages);
    nb_kv_u64(b, "page_tables", si.page_tables);
    nb_kv_u64(b, "free_kib", si.free_pages * 4);
    nb_kv_u64(b, "tasks", si.tasks);
    nb_kv(b, "verify", "n/a");
    res_end(b, a->id);
}

static void op_subscribe(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    (void)r;
    step(a, ACT_OBSERVING, "");
    if (nsubs == SUBS_MAX) {
        fail_action(a, b, "NO_RESOURCES", "subscriptions", "none");
        return;
    }
    step(a, ACT_PLANNED, "call=SYS_INFO");
    step(a, ACT_RUNNING, "");
    subs[nsubs] = events_next();
    step(a, ACT_VERIFYING, "");
    step(a, ACT_SUCCEEDED, "");
    res_begin(b, a->id, "SUCCEEDED");
    nb_kv_u64(b, "sub", nsubs + 1);
    nb_kv_u64(b, "next", subs[nsubs]);
    nb_kv(b, "verify", "n/a");
    res_end(b, a->id);
    nsubs++;
}

static void op_poll(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    uint64_t sub;
    step(a, ACT_OBSERVING, "");
    if (nci_get_u64(r, "sub", &sub) != 1 || sub == 0 || sub > nsubs) {
        fail_action(a, b, "BAD_REQUEST", "sub", "none");
        return;
    }
    step(a, ACT_PLANNED, "call=SOV_EVENT_READ");
    step(a, ACT_RUNNING, "");
    uint64_t from = subs[sub - 1];
    int64_t n = nx_sov_event_read(sov, from, evbuf, 32);
    if (n < 0) {
        fail_action(a, b, "KERNEL_ERROR", u_err(n), "none");
        return;
    }
    uint64_t lost = n > 0 && evbuf[0].seq > from ? evbuf[0].seq - from : 0;
    uint64_t next = n > 0 ? evbuf[n - 1].seq + 1 : from;
    step(a, ACT_VERIFYING, "");
    /* Sequence numbers must be consecutive. */
    for (int64_t i = 1; i < n; i++)
        if (evbuf[i].seq != evbuf[i - 1].seq + 1) {
            verify_failures++;
            fail_action(a, b, "VERIFY_FAILED", "event_gap", "none");
            return;
        }
    subs[sub - 1] = next;
    step(a, ACT_SUCCEEDED, "");
    res_begin(b, a->id, "SUCCEEDED");
    nb_kv_u64(b, "count", (uint64_t)n);
    nb_kv_u64(b, "next", next);
    nb_kv_u64(b, "lost", lost);
    nb_kv(b, "verify", "ok");
    for (int64_t i = 0; i < n; i++) {
        item_begin(b, a->id);
        nb_kv_u64(b, "seq", evbuf[i].seq);
        nb_kv_u64(b, "tick", evbuf[i].tick);
        nb_kv(b, "type", event_name(evbuf[i].type));
        describe_ref(b, "task", evbuf[i].task);
        nb_kv_i64(b, "arg", evbuf[i].arg);
    }
    res_end(b, a->id);
}

static const struct {
    const char *name;
    op_fn fn;
} OPTAB[] = {
    {"system.describe", op_describe}, {"task.list", op_list},
    {"task.inspect", op_inspect},     {"task.spawn", op_spawn},
    {"task.measure", op_measure},     {"task.terminate", op_terminate},
    {"memory.stats", op_memory},      {"event.subscribe", op_subscribe},
    {"event.poll", op_poll},
};

/* ---- request loop ---------------------------------------------------------------- */

static void reply_simple(const char *id, const char *state, const char *k1, const char *v1,
                         const char *k2, const char *v2)
{
    struct nci_buf b;
    res_begin(&b, id, state);
    if (k1)
        nb_kv(&b, k1, v1);
    if (k2)
        nb_kv(&b, k2, v2);
    res_end(&b, id);
    write_all(b.p, b.len);
}

/* Sends a stored response again with " replayed=1" on its first line. */
static void replay(const struct eng_action *a)
{
    uint32_t i = 0;
    while (i < a->result_len && a->result[i] != '\n')
        i++;
    write_all(a->result, i);
    write_all(" replayed=1", 11);
    write_all(a->result + i, a->result_len - i);
    u_printf("act %s %s REPLAYED stored_state=%s (not executed again)\n", a->id, a->op,
             eng_state_name(a->state));
}

static void action_status(const struct nci_req *r)
{
    const char *rid = nci_get(r, "request");
    if (!rid) {
        reply_simple(r->id, "REJECTED", "code", "BAD_REQUEST", "detail", "request");
        return;
    }
    struct eng_action *a = eng_find(&eng, rid);
    struct nci_buf b;
    res_begin(&b, r->id, "SUCCEEDED");
    nb_kv(&b, "request", rid);
    nb_kv(&b, "known", a ? "yes" : "no");
    if (a) {
        nb_kv(&b, "state", eng_state_name(a->state));
        nb_kv(&b, "op", a->op);
        ps_status_fields(&b, a);
    }
    res_end(&b, r->id);
    write_all(b.p, b.len);
    u_printf("status %s request=%s known=%s state=%s\n", r->id, rid, a ? "yes" : "no",
             a ? eng_state_name(a->state) : "-");
}

/* Returns the exit code when the session is closed, else -1. */
int64_t core_handle(const char *line, uint32_t len)
{
    static struct nci_req req;
    int ps = nci_parse(line, len, &req);
    if (ps != NCI_OK) {
        u_printf("reject id=%s: %s\n", req.id, nci_strerror(ps));
        reply_simple(req.id, "REJECTED", "code", "BAD_REQUEST", "detail", nci_strerror(ps));
        return -1;
    }
    if (nci_streq(req.op, "action.status")) {
        action_status(&req);
        return -1;
    }
    if (nci_streq(req.op, "session.close")) {
        const char *hc = nci_get(&req, "host_checks");
        const char *why = nci_get(&req, "reason");
        int host_ok = hc && nci_streq(hc, "ok");
        int64_t code = verify_failures    ? NX_M3_CORE_VERIFY_FAILED
                       : !host_ok         ? NX_M3_CORE_HOST_FAILED
                                          : NX_M3_CORE_OK;
        struct nci_buf b;
        res_begin(&b, req.id, "SUCCEEDED");
        nb_kv_u64(&b, "actions", eng.started);
        nb_kv_u64(&b, "replayed", eng.replayed);
        nb_kv_u64(&b, "verify_failures", verify_failures);
        nb_kv_i64(&b, "exit", code);
        res_end(&b, req.id);
        write_all(b.p, b.len);
        u_printf("session closed host_checks=%s reason=%s actions=%lu replayed=%lu"
                 " verify_failures=%lu exit=%ld\n",
                 hc ? hc : "missing", why ? why : "-", eng.started, eng.replayed,
                 verify_failures, code);
        return code;
    }
    op_fn fn = ps_op(req.op);
    for (uint32_t i = 0; i < sizeof(OPTAB) / sizeof(OPTAB[0]); i++)
        if (nci_streq(req.op, OPTAB[i].name))
            fn = OPTAB[i].fn;
    if (!fn) {
        u_printf("reject id=%s: unknown operation %s\n", req.id, req.op);
        reply_simple(req.id, "REJECTED", "code", "UNKNOWN_OP", 0, 0);
        return -1;
    }
    struct eng_action *a;
    int br = eng_begin(&eng, req.id, req.op, nci_fingerprint(&req), &a);
    if (br == ENG_REPLAY) {
        replay(a);
        return -1;
    }
    if (br != ENG_NEW) {
        const char *code = br == ENG_ID_REUSED ? "ID_REUSED" : "BUSY";
        u_printf("reject id=%s: %s\n", req.id, code);
        reply_simple(req.id, "REJECTED", "code", code, 0, 0);
        return -1;
    }
    u_printf("act %s %s CREATED request=\"%s\"\n", a->id, a->op, line);
    struct nci_buf b;
    fn(a, &req, &b);
    if (b.overflow) {
        a->state = ACT_VERIFYING; /* result is known, only the text did not fit */
        fail_action(a, &b, "RESPONSE_TOO_LARGE", 0, "unknown");
    }
    if (a->persist == PS_INTENT)
        ps_final(a, &b); /* write-ahead record of task.spawn / task.terminate */
    if (eng_store(a, b.p, b.len) != 0)
        u_printf("act %s: response not stored (%u bytes)\n", a->id, b.len);
    write_all(b.p, b.len);
    return -1;
}

int64_t umain(uint64_t a0, uint64_t a1, uint64_t flags, uint64_t blk)
{
    sov = a0;
    chan = a1;
    nodedup = (flags & NX_M3_CORE_NODEDUP) != 0;
    eng_init(&eng, nodedup);
    struct nx_sys_info si;
    if (nx_sys_info(&si) < 0)
        return NX_M3_CORE_BAD_ARGS;
    boot_id = si.boot_id;
    hz = si.hz;
    self_id = (uint32_t)nx_task_self();
    nci_ref_format(self_ref, boot_id, self_id);
    /* The handles must be what the kernel promised. */
    if (nx_sov_task_list(sov, list, LIST_MAX) < 0 || (chan && nx_chan_write(chan, "", 0) < 0)) {
        u_printf("bad handles: sovereign=0x%lx channel=0x%lx\n", sov, chan);
        return NX_M3_CORE_BAD_ARGS;
    }
    ps_init(blk);
    if (flags & NX_M4_CORE_WORKLOAD)
        return ps_run_workload();
    if (flags & NX_M4_CORE_CHECK)
        return ps_run_check();
    if (!chan) {
        u_printf("no bridge channel and no M4 mode flag\n");
        return NX_M3_CORE_BAD_ARGS;
    }
    char hello[128];
    struct nci_buf hb;
    nb_init(&hb, hello, sizeof(hello));
    nb_str(&hb, "HELLO nci=1");
    nb_kv(&hb, "core", self_ref);
    nb_str(&hb, " boot=");
    nb_hex(&hb, boot_id, 16);
    nb_kv(&hb, "dedup", nodedup ? "off" : "on");
    nb_char(&hb, '\n');
    write_all(hb.p, hb.len);
    u_printf("ready %s dedup=%s: waiting for NCI requests on the bridge\n", self_ref,
             nodedup ? "off" : "on");

    static char line[NCI_LINE_MAX + 2];
    uint32_t idle = 0;
    for (;;) {
        uint32_t len = 0;
        int rl = read_line(line, NCI_LINE_MAX + 1, &len);
        if (rl == 0) {
            if (++idle >= IDLE_LIMIT_READS) {
                u_printf("no request for %u s: bridge lost\n",
                         IDLE_LIMIT_READS * READ_TIMEOUT_TICKS / 100);
                return NX_M3_CORE_BRIDGE_LOST;
            }
            continue;
        }
        idle = 0;
        if (rl == -2) {
            u_printf("channel read failed\n");
            return NX_M3_CORE_BRIDGE_LOST;
        }
        if (rl == -1) {
            u_printf("reject: line longer than %u bytes\n", NCI_LINE_MAX);
            reply_simple("-", "REJECTED", "code", "BAD_REQUEST", "detail", "too_long");
            continue;
        }
        line[len] = 0;
        int64_t code = core_handle(line, len);
        if (code >= 0)
            return code;
    }
}
