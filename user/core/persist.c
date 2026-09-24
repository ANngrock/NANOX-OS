/*
 * bin/core, M4: the persistent state of the Cognitive Core executor
 * (docs/m4-store.md).
 *
 * The data disk holds an nxstore (lib/store.c) with
 *   cfg/<key>     configuration values
 *   blob/<name>   test data of a given size (full-disk and crash scenarios)
 *   core/tasks    the task-engine records of actions with effects
 * and, in every generation, the commit log (history) of the store.
 *
 * Rules:
 *   - An operation on the store is one transaction together with its own
 *     task record: after a crash either both the change and the record
 *     exist or neither does.  Its response carries saved=yes only after
 *     st_commit returned ST_OK, i.e. after the second flush.
 *   - task.spawn / task.terminate change kernel state, which no store can
 *     make atomic with a record: a write-ahead record (RUNNING) is
 *     committed before the kernel call and the final record after it.  A
 *     record found in a non-final state at start is reported as
 *     OUTCOME_UNKNOWN (ARCHITECTURE.md §7, §8.2) and that is committed.
 *   - Requests of an earlier boot are answered from the stored record
 *     (replayed=1): the host can ask action.status and repeat a request
 *     across a reboot without executing it twice.
 */
#include <nanox/m3.h>
#include <nanox/m4.h>
#include <nanox/store.h>
#include <nanox/string.h>

#include "core.h"
#include "nanox_user.h"

int ps_state = PS_ABSENT;
static struct store store;
static uint64_t blk_h;
static int mount_err;
static uint32_t restored, recovered_unknown;
static uint64_t commits_ok, commits_failed;

const char PS_OPS[] = "store.status,store.check,store.prune,config.set,config.get,"
                      "config.delete,config.list,blob.put,blob.delete,blob.check,"
                      "history.list,history.pin,history.unpin";

/* ---- block device ------------------------------------------------------------ */

static int d_read(void *ctx, uint64_t blk, uint32_t n, void *buf)
{
    (void)ctx;
    while (n) {
        uint32_t k = n > NX_BLK_IO_MAX ? NX_BLK_IO_MAX : n;
        if (nx_blk_read(blk_h, blk, k, buf) != (int64_t)k)
            return -1;
        blk += k;
        n -= k;
        buf = (uint8_t *)buf + (uint64_t)k * NX_BLK_SIZE;
    }
    return 0;
}

static int d_write(void *ctx, uint64_t blk, uint32_t n, const void *buf)
{
    (void)ctx;
    while (n) {
        uint32_t k = n > NX_BLK_IO_MAX ? NX_BLK_IO_MAX : n;
        if (nx_blk_write(blk_h, blk, k, buf) != (int64_t)k)
            return -1;
        blk += k;
        n -= k;
        buf = (const uint8_t *)buf + (uint64_t)k * NX_BLK_SIZE;
    }
    return 0;
}

static int d_flush(void *ctx)
{
    (void)ctx;
    return nx_blk_flush(blk_h) == 0 ? 0 : -1;
}

/* ---- small helpers ----------------------------------------------------------------- */

static void copy_str(char *dst, uint32_t cap, const char *src)
{
    uint32_t i = 0;
    for (; i + 1 < cap && src[i]; i++)
        dst[i] = src[i];
    dst[i] = 0;
}

static uint32_t first_line_len(const char *p, uint32_t len)
{
    uint32_t n = 0;
    while (n < len && p[n] != '\n')
        n++;
    return n;
}

/* Commit label: up to 13 characters of the request id and a suffix. */
static void make_label(char out[16], const char *id, const char *suffix)
{
    uint32_t n = 0;
    for (; id[n] && n < 13; n++)
        out[n] = id[n];
    for (uint32_t i = 0; suffix[i] && n < 15; i++)
        out[n++] = suffix[i];
    out[n] = 0;
}

static int key_ok(const char *k, uint32_t max)
{
    uint32_t n = 0;
    for (; k[n]; n++) {
        char c = k[n];
        if (n >= max ||
            !((c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '.' || c == '_' || c == '-'))
            return 0;
    }
    return n > 0;
}

const char *ps_state_name(void)
{
    return ps_state == PS_MOUNTED ? "mounted" : ps_state == PS_UNMOUNTABLE ? "unmountable" : "absent";
}

uint64_t ps_gen(void)
{
    return ps_state == PS_MOUNTED ? store.cur.gen : 0;
}

/* ---- the task table ------------------------------------------------------------------- */

static uint8_t tasks_buf[sizeof(struct nx_m4_tasks_hdr) +
                         NX_M4_TASKS_MAX * sizeof(struct nx_m4_task_rec)];

static void fill_rec(struct nx_m4_task_rec *r, const struct eng_action *a, int state,
                     const char *text, uint32_t len)
{
    memset(r, 0, sizeof(*r));
    copy_str(r->id, sizeof(r->id), a->id);
    copy_str(r->op, sizeof(r->op), a->op);
    r->state = (uint8_t)state;
    r->fp = a->fp;
    r->boot_id = a->boot_id;
    uint32_t n = first_line_len(text, len);
    if (n > NX_M4_RES_MAX) {
        n = NX_M4_RES_MAX;
        r->flags |= NX_M4_REC_TRUNCATED;
    }
    memcpy(r->res, text, n);
    r->res_len = n;
}

/* Serialises every persisted engine record in creation order; `over`
 * (if not NULL) is written with state/text instead of its current values. */
static uint32_t build_tasks(const struct eng_action *over, int state, const char *text,
                            uint32_t len)
{
    struct nx_m4_tasks_hdr *h = (struct nx_m4_tasks_hdr *)(void *)tasks_buf;
    struct nx_m4_task_rec *recs = (struct nx_m4_task_rec *)(void *)(tasks_buf + sizeof(*h));
    memset(h, 0, sizeof(*h));
    memcpy(h->magic, NX_M4_TASKS_MAGIC, 8);
    h->rec_size = sizeof(struct nx_m4_task_rec);
    uint32_t last = 0;
    for (;;) { /* ascending seq */
        const struct eng_action *next = 0;
        for (uint32_t i = 0; i < ENG_SLOTS; i++) {
            const struct eng_action *a = &eng.a[i];
            if (a->state == ACT_NONE || !(a->persist || a == over) || a->seq <= last)
                continue;
            if (!next || a->seq < next->seq)
                next = a;
        }
        if (!next || h->count == NX_M4_TASKS_MAX)
            break;
        last = next->seq;
        if (next == over)
            fill_rec(&recs[h->count++], next, state, text, len);
        else
            fill_rec(&recs[h->count++], next, next->state, next->result, next->result_len);
    }
    return (uint32_t)sizeof(*h) + h->count * (uint32_t)sizeof(struct nx_m4_task_rec);
}

static int put_tasks(const struct eng_action *over, int state, const char *text, uint32_t len)
{
    uint32_t n = build_tasks(over, state, text, len);
    return st_put(&store, NX_M4_TASKS_NAME, NX_M4_KIND_TASKS, tasks_buf, n);
}

/* The stored record of `id` (state), or -1. */
static int stored_state(const char *id)
{
    uint32_t len = 0;
    if (st_read(&store, 0, NX_M4_TASKS_NAME, tasks_buf, sizeof(tasks_buf), &len, 0) != ST_OK ||
        len < sizeof(struct nx_m4_tasks_hdr))
        return -1;
    const struct nx_m4_tasks_hdr *h = (const struct nx_m4_tasks_hdr *)(const void *)tasks_buf;
    const struct nx_m4_task_rec *r =
        (const struct nx_m4_task_rec *)(const void *)(tasks_buf + sizeof(*h));
    if (memcmp(h->magic, NX_M4_TASKS_MAGIC, 8) != 0 || h->rec_size != sizeof(*r) ||
        sizeof(*h) + (uint64_t)h->count * sizeof(*r) > len)
        return -1;
    for (uint32_t i = 0; i < h->count; i++)
        if (nci_streq(r[i].id, id))
            return r[i].state;
    return -1;
}

/* ---- commits ---------------------------------------------------------------------------- */

static void remount(void)
{
    struct st_dev dev = store.dev;
    mount_err = st_mount(&store, &dev, boot_id, 0);
    ps_state = mount_err == ST_OK ? PS_MOUNTED : PS_UNMOUNTABLE;
    u_printf("store remounted after an unknown outcome: %s gen=%lu\n", st_strerror(mount_err),
             ps_gen());
}

/* The durability protocol of lib/store.c; "store saved" is printed only
 * after it completed (the crash tests rely on this line). */
static int commit(const char *label, uint64_t *gen)
{
    uint64_t g = st_next_gen(&store);
    u_printf("store commit begin gen=%lu label=%s\n", g, label);
    int r = st_commit(&store, label, gen);
    if (r == ST_OK) {
        commits_ok++;
        u_printf("store saved gen=%lu label=%s free=%u\n", *gen, label, st_free_blocks(&store));
    } else {
        commits_failed++;
        u_printf("store commit failed gen=%lu label=%s: %s\n", g, label, st_strerror(r));
        if (r == ST_E_UNKNOWN)
            remount();
    }
    return r;
}

static const char *st_code(int r, const char **detail)
{
    *detail = st_strerror(r);
    switch (r) {
    case ST_E_NOSPC:
    case ST_E_OBJFULL:
    case ST_E_PINFULL: return "NO_SPACE";
    case ST_E_NOTFOUND: return "NOT_FOUND";
    case ST_E_PRUNED: return "PRUNED";
    case ST_E_EXISTS: return "CONFLICT";
    case ST_E_NAME:
    case ST_E_TOOBIG: return "BAD_REQUEST";
    default: return "STORE_ERROR";
    }
}

/* Ends a store transaction: adds the record of `a` as SUCCEEDED with the
 * response `res` (complete, "RES ...\nEND ...\n") and commits both. */
static int commit_with_record(struct eng_action *a, const char *res, uint32_t len,
                              uint64_t *gen)
{
    a->boot_id = boot_id;
    int r = put_tasks(a, ACT_SUCCEEDED, res, len);
    if (r != ST_OK) {
        st_abort(&store);
        return r;
    }
    char label[16];
    make_label(label, a->id, "");
    r = commit(label, gen);
    a->persist = r == ST_OK ? PS_RECORD : 0;
    return r;
}

static int store_ready(struct eng_action *a, struct nci_buf *b)
{
    if (ps_state == PS_MOUNTED)
        return 1;
    fail_action(a, b, "NO_STORE", ps_state_name(), "none");
    return 0;
}

/* Common failure path of a transaction step before the commit. */
static void tx_failed(struct eng_action *a, struct nci_buf *b, int r)
{
    const char *detail;
    const char *code = st_code(r, &detail);
    if (store.in_tx)
        st_abort(&store);
    if (r == ST_E_UNKNOWN) {
        step(a, ACT_OUTCOME_UNKNOWN, "commit=unknown");
        res_begin(b, a->id, "OUTCOME_UNKNOWN");
        nb_kv(b, "code", "STORE_ERROR");
        nb_kv(b, "detail", detail);
        nb_kv(b, "effects", "unknown");
        res_end(b, a->id);
        return;
    }
    fail_action(a, b, code, detail, "none");
}

/* ---- restoring the task records at start -------------------------------------------------- */

static void restore_tasks(void)
{
    uint32_t len = 0;
    int r = st_read(&store, 0, NX_M4_TASKS_NAME, tasks_buf, sizeof(tasks_buf), &len, 0);
    if (r == ST_E_NOTFOUND) {
        u_printf("store tasks none\n");
        return;
    }
    const struct nx_m4_tasks_hdr *h = (const struct nx_m4_tasks_hdr *)(const void *)tasks_buf;
    if (r != ST_OK || len < sizeof(*h) || memcmp(h->magic, NX_M4_TASKS_MAGIC, 8) != 0 ||
        h->rec_size != sizeof(struct nx_m4_task_rec) ||
        sizeof(*h) + (uint64_t)h->count * h->rec_size > len) {
        u_printf("store tasks unreadable: %s\n", st_strerror(r));
        return;
    }
    static struct nx_m4_task_rec recs[NX_M4_TASKS_MAX];
    uint32_t count = h->count > NX_M4_TASKS_MAX ? NX_M4_TASKS_MAX : h->count;
    memcpy(recs, tasks_buf + sizeof(*h), count * sizeof(recs[0]));
    static char text[NX_M4_RES_MAX + 160];
    for (uint32_t i = 0; i < count; i++) {
        struct nx_m4_task_rec *rec = &recs[i];
        rec->id[32] = 0;
        rec->op[32] = 0;
        int state = rec->state;
        struct nci_buf b;
        nb_init(&b, text, sizeof(text));
        if (!eng_is_final(state)) {
            /* Interrupted between the write-ahead record and the final one:
             * whether the effect happened is not known. */
            state = ACT_OUTCOME_UNKNOWN;
            recovered_unknown++;
            nb_str(&b, "RES ");
            nb_str(&b, rec->id);
            nb_str(&b, " OUTCOME_UNKNOWN code=INTERRUPTED detail=boot_");
            nb_hex(&b, rec->boot_id, 16);
            nb_str(&b, " effects=unknown");
        } else {
            uint32_t n = rec->res_len > NX_M4_RES_MAX ? NX_M4_RES_MAX : rec->res_len;
            for (uint32_t k = 0; k < n; k++)
                nb_char(&b, rec->res[k]);
            if (rec->flags & NX_M4_REC_TRUNCATED)
                nb_str(&b, " truncated=1");
        }
        nb_str(&b, "\nEND ");
        nb_str(&b, rec->id);
        nb_char(&b, '\n');
        struct eng_action *a =
            eng_restore(&eng, rec->id, rec->op, rec->fp, state, b.p, b.len, rec->boot_id);
        if (a)
            restored++;
        u_printf("store task %s %s %s boot=%lx%s\n", rec->id, rec->op, eng_state_name(state),
                 rec->boot_id, state != rec->state ? " (was interrupted)" : "");
    }
    u_printf("store tasks restored=%u outcome_unknown=%u\n", restored, recovered_unknown);
}

void ps_init(uint64_t h)
{
    if (!h)
        return; /* no data disk: M3 behaviour */
    blk_h = h;
    struct nx_blk_info info;
    if (nx_blk_info(h, &info) < 0) {
        ps_state = PS_UNMOUNTABLE;
        mount_err = ST_E_IO;
        u_printf("store: block device handle rejected\n");
        return;
    }
    struct st_dev dev = {0, info.blocks, d_read, d_write, d_flush};
    mount_err = st_mount(&store, &dev, boot_id, 0);
    const struct st_mount_report *rp = &store.rep;
    if (mount_err != ST_OK) {
        ps_state = PS_UNMOUNTABLE;
        u_printf("store unmountable: %s slots=%s,%s max_gen=%lu (serving without the store)\n",
                 st_strerror(mount_err), st_slot_state_name(rp->slot_state[0]),
                 st_slot_state_name(rp->slot_state[1]), rp->max_gen);
        return;
    }
    ps_state = PS_MOUNTED;
    u_printf("store mounted gen=%lu label=%s slot=%d slots=%s,%s fallback=%d rejected_gen=%lu"
             " max_gen=%lu free=%u blocks=%lu retain=%u hist_damaged=%u serial=%s\n",
             store.cur.gen, store.cur.label, rp->slot, st_slot_state_name(rp->slot_state[0]),
             st_slot_state_name(rp->slot_state[1]), rp->fallback, rp->rejected_gen, rp->max_gen,
             st_free_blocks(&store), store.dev.blocks, store.cur.retain, rp->hist_damaged,
             info.serial);
    restore_tasks();
    if (recovered_unknown) {
        uint64_t g;
        int r = st_begin(&store);
        if (r == ST_OK)
            r = put_tasks(0, 0, 0, 0);
        if (r == ST_OK)
            r = commit("recover", &g);
        else
            st_abort(&store);
        u_printf("store recovery commit: %s\n", st_strerror(r));
    }
}

/* ---- write-ahead records of task.spawn / task.terminate ----------------------------------- */

int ps_intent(struct eng_action *a)
{
    if (ps_state != PS_MOUNTED)
        return 0;
    a->boot_id = boot_id;
    uint64_t g;
    char label[16];
    make_label(label, a->id, ":s");
    int r = st_begin(&store);
    if (r == ST_OK)
        r = put_tasks(a, ACT_RUNNING, "", 0);
    if (r == ST_OK)
        r = commit(label, &g);
    else
        st_abort(&store);
    if (r != ST_OK)
        return -1;
    a->persist = PS_INTENT;
    return 0;
}

static void insert_saved(struct nci_buf *b, int saved)
{
    const char *ins = saved ? " saved=yes" : " saved=no";
    uint32_t n = (uint32_t)u_strlen(ins), at = first_line_len(b->p, b->len);
    if (b->len + n >= b->cap)
        return;
    memmove(b->p + at + n, b->p + at, b->len - at);
    memcpy(b->p + at, ins, n);
    b->len += n;
}

void ps_final(struct eng_action *a, struct nci_buf *b)
{
    if (ps_state != PS_MOUNTED)
        return;
    insert_saved(b, 1);
    uint64_t g;
    char label[16];
    make_label(label, a->id, ":d");
    int r = st_begin(&store);
    if (r == ST_OK)
        r = put_tasks(a, a->state, b->p, b->len);
    if (r == ST_OK)
        r = commit(label, &g);
    else
        st_abort(&store);
    if (r == ST_OK)
        a->persist = PS_RECORD;
    if (r != ST_OK) {
        /* " saved=yes" -> " saved=no" (same position, one character shorter) */
        uint32_t at = first_line_len(b->p, b->len) - 10;
        memmove(b->p + at, b->p + at + 10, b->len - at - 10);
        b->len -= 10;
        insert_saved(b, 0);
    }
}

void ps_status_fields(struct nci_buf *b, const struct eng_action *a)
{
    if (ps_state == PS_ABSENT)
        return;
    nb_str(b, " boot=");
    nb_hex(b, a->boot_id ? a->boot_id : boot_id, 16);
    nb_kv(b, "persisted", a->persist ? "yes" : "no");
    nb_kv(b, "restored", a->restored ? "yes" : "no");
}

/* ---- operations ------------------------------------------------------------------------------ */

static char resmem[1024];
static uint8_t objbuf[ST_OBJ_MAX_BYTES];

/* Starts a response in resmem (res_begin would use the action's buffer,
 * which must stay free until the commit decided the outcome). */
static void tentative(struct nci_buf *res, const char *id)
{
    nb_init(res, resmem, sizeof(resmem));
    nb_str(res, "RES ");
    nb_str(res, id);
    nb_str(res, " SUCCEEDED");
}

/* Copies the response built in `tmp` into the action's buffer b. */
static void emit(struct nci_buf *b, const struct nci_buf *tmp)
{
    res_reset(b);
    for (uint32_t i = 0; i < tmp->len; i++)
        nb_char(b, tmp->p[i]);
}

/* Parses gen=<n> (0: absent -> current).  -1: malformed. */
static int get_gen(const struct nci_req *r, uint64_t *gen)
{
    *gen = 0;
    int g = nci_get_u64(r, "gen", gen);
    return g < 0 ? -1 : 0;
}

static void op_status(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    (void)r;
    step(a, ACT_OBSERVING, "");
    step(a, ACT_PLANNED, "read=store");
    step(a, ACT_RUNNING, "");
    step(a, ACT_VERIFYING, "read-only");
    step(a, ACT_SUCCEEDED, "");
    const struct st_mount_report *rp = &store.rep;
    res_begin(b, a->id, "SUCCEEDED");
    nb_kv(b, "state", ps_state_name());
    if (ps_state == PS_ABSENT) {
        nb_kv(b, "verify", "n/a");
        res_end(b, a->id);
        return;
    }
    nb_kv(b, "slots", st_slot_state_name(rp->slot_state[0]));
    nb_char(b, ',');
    nb_str(b, st_slot_state_name(rp->slot_state[1]));
    nb_kv_u64(b, "max_gen", rp->max_gen);
    if (ps_state == PS_MOUNTED) {
        nb_kv_u64(b, "gen", store.cur.gen);
        nb_kv(b, "label", store.cur.label);
        nb_kv_u64(b, "slot", (uint64_t)store.cur_slot);
        nb_kv_u64(b, "mounted_gen", rp->gen);
        nb_kv(b, "fallback", rp->fallback ? "yes" : "no");
        if (rp->fallback) {
            nb_kv_u64(b, "rejected_gen", rp->rejected_gen);
            nb_kv(b, "rejected", st_slot_state_name(rp->rejected_state));
        }
        nb_kv_u64(b, "blocks", store.dev.blocks);
        nb_kv_u64(b, "free", st_free_blocks(&store));
        nb_kv_u64(b, "retain", store.cur.retain);
        nb_kv_u64(b, "objects", store.cur.nobj);
        nb_kv_u64(b, "pins", store.cur.npin);
        nb_kv_u64(b, "history", store.cur.nhist);
        nb_kv_u64(b, "commits", commits_ok);
        nb_kv_u64(b, "commit_failures", commits_failed);
        nb_kv_u64(b, "restored_tasks", restored);
        nb_kv_u64(b, "outcome_unknown", recovered_unknown);
    } else {
        nb_kv(b, "error", st_strerror(mount_err));
    }
    nb_kv(b, "verify", "n/a");
    res_end(b, a->id);
}

static void op_check(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    (void)r;
    step(a, ACT_OBSERVING, "");
    if (!store_ready(a, b))
        return;
    step(a, ACT_PLANNED, "call=st_check");
    step(a, ACT_RUNNING, "");
    struct st_check_report cr;
    int st = st_check(&store, &cr);
    step(a, ACT_VERIFYING, "");
    if (st != ST_OK) {
        verify_failures++;
        fail_action(a, b, "VERIFY_FAILED", cr.first, "none");
        return;
    }
    step(a, ACT_SUCCEEDED, "");
    res_begin(b, a->id, "SUCCEEDED");
    nb_kv_u64(b, "gen", store.cur.gen);
    nb_kv_u64(b, "roots", cr.roots);
    nb_kv_u64(b, "objects", cr.objects);
    nb_kv_u64(b, "extents", cr.extents);
    nb_kv_u64(b, "used", cr.used_blocks);
    nb_kv_u64(b, "free", cr.free_blocks);
    nb_kv(b, "verify", "ok");
    nb_kv(b, "checks", "superblock,roots,extent_crc,overlap,allocation");
    res_end(b, a->id);
}

/* Readback of an object after the commit: exists in generation `gen` with
 * the expected bytes, and the task record of the action is stored. */
static const char *readback(struct eng_action *a, const char *name, const uint8_t *want,
                            uint32_t want_len, uint64_t gen)
{
    struct st_obj meta;
    uint32_t len = 0;
    if (store.cur.gen != gen)
        return "generation_not_current";
    if (name) {
        if (st_read(&store, 0, name, objbuf, sizeof(objbuf), &len, &meta) != ST_OK)
            return "object_unreadable";
        if (len != want_len || memcmp(objbuf, want, len) != 0 || meta.mod_gen != gen)
            return "object_differs";
    }
    if (stored_state(a->id) != ACT_SUCCEEDED)
        return "record_missing";
    return 0;
}

/* Shared tail of every store transaction: record + commit + readback. */
static void finish_tx(struct eng_action *a, struct nci_buf *b, struct nci_buf *res,
                      const char *name, const uint8_t *want, uint32_t want_len)
{
    uint64_t gen = 0;
    int st = commit_with_record(a, res->p, res->len, &gen);
    if (st != ST_OK) {
        tx_failed(a, b, st);
        return;
    }
    step(a, ACT_VERIFYING, "saved=yes");
    const char *why = readback(a, name, want, want_len, gen);
    if (why) {
        verify_failures++;
        fail_action(a, b, "VERIFY_FAILED", why, "applied");
        return;
    }
    step(a, ACT_SUCCEEDED, "readback=ok");
    emit(b, res);
}

static void op_config_set(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    step(a, ACT_OBSERVING, "");
    if (!store_ready(a, b))
        return;
    const char *key = nci_get(r, "key"), *val = nci_get(r, "value");
    if (!key || !key_ok(key, ST_NAME_MAX - 4)) {
        fail_action(a, b, "BAD_REQUEST", "key", "none");
        return;
    }
    if (!val) {
        fail_action(a, b, "BAD_REQUEST", "value", "none");
        return;
    }
    char name[32] = "cfg/";
    copy_str(name + 4, sizeof(name) - 4, key);
    const struct st_obj *prev = st_find(&store.cur, name);
    uint32_t version = prev ? prev->version + 1 : 1;
    uint64_t gen = st_next_gen(&store);
    step(a, ACT_PLANNED, "tx=put");
    step(a, ACT_RUNNING, "");
    uint32_t vlen = (uint32_t)u_strlen(val);
    int st = st_begin(&store);
    if (st == ST_OK)
        st = st_put(&store, name, NX_M4_KIND_CONFIG, val, vlen);
    if (st != ST_OK) {
        tx_failed(a, b, st);
        return;
    }
    struct nci_buf res;
    tentative(&res, a->id);
    nb_kv(&res, "key", key);
    nb_kv(&res, "value", val);
    nb_kv_u64(&res, "version", version);
    nb_kv_u64(&res, "gen", gen);
    nb_kv(&res, "saved", "yes");
    nb_kv(&res, "verify", "ok");
    nb_kv(&res, "checks", "committed,readback,record");
    res_end(&res, a->id);
    finish_tx(a, b, &res, name, (const uint8_t *)val, vlen);
}

static void op_config_delete(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    step(a, ACT_OBSERVING, "");
    if (!store_ready(a, b))
        return;
    const char *key = nci_get(r, "key");
    if (!key || !key_ok(key, ST_NAME_MAX - 4)) {
        fail_action(a, b, "BAD_REQUEST", "key", "none");
        return;
    }
    char name[32] = "cfg/";
    copy_str(name + 4, sizeof(name) - 4, key);
    if (!st_find(&store.cur, name)) {
        fail_action(a, b, "NOT_FOUND", key, "none");
        return;
    }
    uint64_t gen = st_next_gen(&store);
    step(a, ACT_PLANNED, "tx=delete");
    step(a, ACT_RUNNING, "");
    int st = st_begin(&store);
    st_allow_reserve(&store); /* releases space: may use the reserve (full store) */
    if (st == ST_OK)
        st = st_del(&store, name);
    if (st != ST_OK) {
        tx_failed(a, b, st);
        return;
    }
    struct nci_buf res;
    tentative(&res, a->id);
    nb_kv(&res, "key", key);
    nb_kv_u64(&res, "gen", gen);
    nb_kv(&res, "saved", "yes");
    nb_kv(&res, "verify", "ok");
    nb_kv(&res, "checks", "committed,absent,record");
    res_end(&res, a->id);
    uint64_t g = 0;
    st = commit_with_record(a, res.p, res.len, &g);
    if (st != ST_OK) {
        tx_failed(a, b, st);
        return;
    }
    step(a, ACT_VERIFYING, "saved=yes");
    const char *why = readback(a, 0, 0, 0, g);
    if (!why && st_find(&store.cur, name))
        why = "still_present";
    if (why) {
        verify_failures++;
        fail_action(a, b, "VERIFY_FAILED", why, "applied");
        return;
    }
    step(a, ACT_SUCCEEDED, "readback=ok");
    emit(b, &res);
}

static void op_config_get(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    step(a, ACT_OBSERVING, "");
    if (!store_ready(a, b))
        return;
    const char *key = nci_get(r, "key");
    uint64_t gen;
    if (!key || !key_ok(key, ST_NAME_MAX - 4) || get_gen(r, &gen) < 0) {
        fail_action(a, b, "BAD_REQUEST", key ? "gen" : "key", "none");
        return;
    }
    char name[32] = "cfg/";
    copy_str(name + 4, sizeof(name) - 4, key);
    step(a, ACT_PLANNED, "read=cfg");
    step(a, ACT_RUNNING, "");
    struct st_obj meta;
    uint32_t len = 0;
    int st = st_read(&store, gen, name, objbuf, NCI_VAL_MAX, &len, &meta);
    if (st != ST_OK) {
        const char *detail;
        fail_action(a, b, st_code(st, &detail), detail, "none");
        return;
    }
    objbuf[len] = 0;
    step(a, ACT_VERIFYING, "read-only crc=ok");
    step(a, ACT_SUCCEEDED, "");
    res_begin(b, a->id, "SUCCEEDED");
    nb_kv(b, "key", key);
    nb_kv(b, "value", (const char *)objbuf);
    nb_kv_u64(b, "version", meta.version);
    nb_kv_u64(b, "modified_gen", meta.mod_gen);
    nb_kv_u64(b, "gen", gen ? gen : store.cur.gen);
    nb_kv(b, "verify", "n/a");
    res_end(b, a->id);
}

static void op_config_list(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    (void)r;
    step(a, ACT_OBSERVING, "");
    if (!store_ready(a, b))
        return;
    step(a, ACT_PLANNED, "read=cfg/*");
    step(a, ACT_RUNNING, "");
    step(a, ACT_VERIFYING, "read-only");
    step(a, ACT_SUCCEEDED, "");
    res_begin(b, a->id, "SUCCEEDED");
    nb_kv_u64(b, "gen", store.cur.gen);
    uint32_t n = 0;
    for (uint32_t i = 0; i < store.cur.nobj; i++)
        n += store.cur.obj[i].kind == NX_M4_KIND_CONFIG;
    nb_kv_u64(b, "count", n);
    nb_kv(b, "verify", "n/a");
    for (uint32_t i = 0; i < store.cur.nobj; i++) {
        struct st_obj o = store.cur.obj[i];
        if (o.kind != NX_M4_KIND_CONFIG)
            continue;
        uint32_t len = 0;
        if (st_read(&store, 0, o.name, objbuf, NCI_VAL_MAX, &len, 0) != ST_OK)
            len = 0;
        objbuf[len] = 0;
        item_begin(b, a->id);
        nb_kv(b, "key", o.name + 4);
        nb_kv(b, "value", len ? (const char *)objbuf : "-");
        nb_kv_u64(b, "version", o.version);
        nb_kv_u64(b, "modified_gen", o.mod_gen);
    }
    res_end(b, a->id);
}

static uint32_t make_blob(const char *name, uint32_t size)
{
    uint32_t h = nx_m4_name_hash(name);
    for (uint32_t i = 0; i < size; i++)
        objbuf[i] = nx_m4_blob_byte(h, size, i);
    return size;
}

static void op_blob_put(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    step(a, ACT_OBSERVING, "");
    if (!store_ready(a, b))
        return;
    const char *nm = nci_get(r, "name");
    uint64_t size = 0;
    if (!nm || !key_ok(nm, ST_NAME_MAX - 5)) {
        fail_action(a, b, "BAD_REQUEST", "name", "none");
        return;
    }
    if (nci_get_u64(r, "size", &size) != 1 || size == 0 || size > ST_OBJ_MAX_BYTES) {
        fail_action(a, b, "BAD_REQUEST", "size", "none");
        return;
    }
    char name[32] = "blob/";
    copy_str(name + 5, sizeof(name) - 5, nm);
    uint64_t gen = st_next_gen(&store);
    step(a, ACT_PLANNED, "tx=put");
    step(a, ACT_RUNNING, "");
    make_blob(nm, (uint32_t)size);
    int st = st_begin(&store);
    if (st == ST_OK)
        st = st_put(&store, name, NX_M4_KIND_BLOB, objbuf, (uint32_t)size);
    if (st != ST_OK) {
        tx_failed(a, b, st);
        return;
    }
    const struct st_obj *o = st_find(&store.work, name);
    struct nci_buf res;
    tentative(&res, a->id);
    nb_kv(&res, "name", nm);
    nb_kv_u64(&res, "size", size);
    nb_kv_u64(&res, "blocks", o ? o->nblk : 0);
    nb_kv_u64(&res, "version", o ? o->version : 0);
    nb_kv_u64(&res, "gen", gen);
    nb_kv(&res, "saved", "yes");
    nb_kv(&res, "verify", "ok");
    nb_kv(&res, "checks", "committed,readback,record");
    res_end(&res, a->id);
    /* The readback compares with a freshly generated copy (objbuf is reused). */
    static uint8_t want[ST_OBJ_MAX_BYTES];
    memcpy(want, objbuf, size);
    finish_tx(a, b, &res, name, want, (uint32_t)size);
}

static void op_blob_delete(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    step(a, ACT_OBSERVING, "");
    if (!store_ready(a, b))
        return;
    const char *nm = nci_get(r, "name");
    if (!nm || !key_ok(nm, ST_NAME_MAX - 5)) {
        fail_action(a, b, "BAD_REQUEST", "name", "none");
        return;
    }
    char name[32] = "blob/";
    copy_str(name + 5, sizeof(name) - 5, nm);
    if (!st_find(&store.cur, name)) {
        fail_action(a, b, "NOT_FOUND", nm, "none");
        return;
    }
    uint64_t gen = st_next_gen(&store);
    step(a, ACT_PLANNED, "tx=delete");
    step(a, ACT_RUNNING, "");
    int st = st_begin(&store);
    st_allow_reserve(&store); /* releases space: may use the reserve (full store) */
    if (st == ST_OK)
        st = st_del(&store, name);
    if (st != ST_OK) {
        tx_failed(a, b, st);
        return;
    }
    struct nci_buf res;
    tentative(&res, a->id);
    nb_kv(&res, "name", nm);
    nb_kv_u64(&res, "gen", gen);
    nb_kv(&res, "saved", "yes");
    nb_kv(&res, "verify", "ok");
    nb_kv(&res, "checks", "committed,absent,record");
    res_end(&res, a->id);
    uint64_t g = 0;
    st = commit_with_record(a, res.p, res.len, &g);
    if (st != ST_OK) {
        tx_failed(a, b, st);
        return;
    }
    step(a, ACT_VERIFYING, "saved=yes");
    const char *why = readback(a, 0, 0, 0, g);
    if (!why && st_find(&store.cur, name))
        why = "still_present";
    if (why) {
        verify_failures++;
        fail_action(a, b, "VERIFY_FAILED", why, "applied");
        return;
    }
    step(a, ACT_SUCCEEDED, "readback=ok");
    emit(b, &res);
}

static void op_blob_check(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    step(a, ACT_OBSERVING, "");
    if (!store_ready(a, b))
        return;
    const char *nm = nci_get(r, "name");
    uint64_t gen;
    if (!nm || !key_ok(nm, ST_NAME_MAX - 5) || get_gen(r, &gen) < 0) {
        fail_action(a, b, "BAD_REQUEST", nm ? "gen" : "name", "none");
        return;
    }
    char name[32] = "blob/";
    copy_str(name + 5, sizeof(name) - 5, nm);
    step(a, ACT_PLANNED, "read=blob");
    step(a, ACT_RUNNING, "");
    struct st_obj meta;
    uint32_t len = 0;
    int st = st_read(&store, gen, name, objbuf, sizeof(objbuf), &len, &meta);
    if (st != ST_OK) {
        const char *detail;
        fail_action(a, b, st_code(st, &detail), detail, "none");
        return;
    }
    step(a, ACT_VERIFYING, "crc=ok");
    uint32_t h = nx_m4_name_hash(nm);
    for (uint32_t i = 0; i < len; i++)
        if (objbuf[i] != nx_m4_blob_byte(h, len, i)) {
            verify_failures++;
            fail_action(a, b, "VERIFY_FAILED", "content", "none");
            return;
        }
    step(a, ACT_SUCCEEDED, "content=ok");
    res_begin(b, a->id, "SUCCEEDED");
    nb_kv(b, "name", nm);
    nb_kv_u64(b, "size", len);
    nb_kv_u64(b, "version", meta.version);
    nb_kv_u64(b, "modified_gen", meta.mod_gen);
    nb_kv_u64(b, "gen", gen ? gen : store.cur.gen);
    nb_kv(b, "verify", "ok");
    nb_kv(b, "checks", "crc,content");
    res_end(b, a->id);
}

static void op_history_list(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    (void)r;
    step(a, ACT_OBSERVING, "");
    if (!store_ready(a, b))
        return;
    step(a, ACT_PLANNED, "read=log");
    step(a, ACT_RUNNING, "");
    step(a, ACT_VERIFYING, "read-only");
    step(a, ACT_SUCCEEDED, "");
    res_begin(b, a->id, "SUCCEEDED");
    nb_kv_u64(b, "current", store.cur.gen);
    nb_kv_u64(b, "retain", store.cur.retain);
    nb_kv_u64(b, "count", store.cur.nlog);
    nb_kv_u64(b, "pins", store.cur.npin);
    nb_kv(b, "verify", "n/a");
    for (uint32_t i = 0; i < store.cur.nlog; i++) {
        const struct st_log *l = &store.cur.log[i];
        item_begin(b, a->id);
        nb_kv_u64(b, "gen", l->gen);
        nb_str(b, " boot=");
        nb_hex(b, l->boot_id, 16);
        nb_kv(b, "label", l->label);
        nb_kv(b, "content", st_is_retained(&store, l->gen) ? "kept" : "pruned");
        const char *pin = "-";
        for (uint32_t k = 0; k < store.cur.npin; k++)
            if (store.cur.pin[k].ref.gen == l->gen)
                pin = store.cur.pin[k].name;
        nb_kv(b, "pin", pin);
    }
    res_end(b, a->id);
}

static void pin_or_unpin(struct eng_action *a, const struct nci_req *r, struct nci_buf *b,
                         int pin)
{
    step(a, ACT_OBSERVING, "");
    if (!store_ready(a, b))
        return;
    const char *nm = nci_get(r, "name");
    uint64_t gen = 0;
    if (!nm || !st_pin_name_ok(nm)) {
        fail_action(a, b, "BAD_REQUEST", "name", "none");
        return;
    }
    if (pin && (nci_get_u64(r, "gen", &gen) != 1 || gen == 0)) {
        fail_action(a, b, "BAD_REQUEST", "gen", "none");
        return;
    }
    uint64_t next = st_next_gen(&store);
    step(a, ACT_PLANNED, pin ? "tx=pin" : "tx=unpin");
    step(a, ACT_RUNNING, "");
    int st = st_begin(&store);
    if (!pin)
        st_allow_reserve(&store);
    if (st == ST_OK)
        st = pin ? st_pin(&store, nm, gen) : st_unpin(&store, nm);
    if (st != ST_OK) {
        tx_failed(a, b, st);
        return;
    }
    struct nci_buf res;
    tentative(&res, a->id);
    nb_kv(&res, "name", nm);
    if (pin)
        nb_kv_u64(&res, "pinned_gen", gen);
    nb_kv_u64(&res, "gen", next);
    nb_kv(&res, "saved", "yes");
    nb_kv(&res, "verify", "ok");
    nb_kv(&res, "checks", pin ? "committed,pinned,record" : "committed,unpinned,record");
    res_end(&res, a->id);
    uint64_t g = 0;
    st = commit_with_record(a, res.p, res.len, &g);
    if (st != ST_OK) {
        tx_failed(a, b, st);
        return;
    }
    step(a, ACT_VERIFYING, "saved=yes");
    const char *why = readback(a, 0, 0, 0, g);
    int found = 0;
    for (uint32_t k = 0; k < store.cur.npin; k++)
        found |= nci_streq(store.cur.pin[k].name, nm) && (!pin || store.cur.pin[k].ref.gen == gen);
    if (!why && found != pin)
        why = pin ? "pin_missing" : "pin_present";
    if (!why && pin && !st_is_retained(&store, gen))
        why = "not_retained";
    if (why) {
        verify_failures++;
        fail_action(a, b, "VERIFY_FAILED", why, "applied");
        return;
    }
    step(a, ACT_SUCCEEDED, "readback=ok");
    emit(b, &res);
}

static void op_pin(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    pin_or_unpin(a, r, b, 1);
}

static void op_unpin(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    pin_or_unpin(a, r, b, 0);
}

static void op_prune(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    (void)r;
    step(a, ACT_OBSERVING, "");
    if (!store_ready(a, b))
        return;
    uint32_t free_before = st_free_blocks(&store);
    uint64_t next = st_next_gen(&store);
    step(a, ACT_PLANNED, "tx=prune");
    step(a, ACT_RUNNING, "");
    int st = st_begin(&store);
    if (st != ST_OK) {
        tx_failed(a, b, st);
        return;
    }
    st_prune(&store);
    st_allow_reserve(&store);
    struct nci_buf res;
    tentative(&res, a->id);
    nb_kv_u64(&res, "gen", next);
    nb_kv(&res, "saved", "yes");
    nb_kv(&res, "verify", "ok");
    nb_kv(&res, "checks", "committed,history_min,record");
    res_end(&res, a->id);
    uint64_t g = 0;
    st = commit_with_record(a, res.p, res.len, &g);
    if (st != ST_OK) {
        tx_failed(a, b, st);
        return;
    }
    step(a, ACT_VERIFYING, "saved=yes");
    const char *why = readback(a, 0, 0, 0, g);
    if (!why && store.cur.nhist != ST_RETAIN_MIN - 1)
        why = "history_not_pruned";
    if (why) {
        verify_failures++;
        fail_action(a, b, "VERIFY_FAILED", why, "applied");
        return;
    }
    u_printf("store prune gen=%lu free_before=%u free_after=%u\n", g, free_before,
             st_free_blocks(&store));
    step(a, ACT_SUCCEEDED, "readback=ok");
    emit(b, &res);
}

static const struct {
    const char *name;
    op_fn fn;
} PS_OPTAB[] = {
    {"store.status", op_status},       {"store.check", op_check},
    {"store.prune", op_prune},         {"config.set", op_config_set},
    {"config.get", op_config_get},     {"config.delete", op_config_delete},
    {"config.list", op_config_list},   {"blob.put", op_blob_put},
    {"blob.delete", op_blob_delete},   {"blob.check", op_blob_check},
    {"history.list", op_history_list}, {"history.pin", op_pin},
    {"history.unpin", op_unpin},
};

op_fn ps_op(const char *op)
{
    for (uint32_t i = 0; i < sizeof(PS_OPTAB) / sizeof(PS_OPTAB[0]); i++)
        if (nci_streq(op, PS_OPTAB[i].name))
            return PS_OPTAB[i].fn;
    return 0;
}

/* ---- workload and check modes (no bridge) ------------------------------------------------------ */

/* The built-in workload of the crash tests (docs/m4-store.md §7): every
 * kind of change the executor makes, enough commits that blocks released
 * by the retention policy are reused.  "@<id>" is replaced by the task
 * reference in the response to request <id>. */
static const char *const WORKLOAD[] = {
    "REQ w1 config.set key=mode value=alpha", /* gen 2 */
    "REQ w2 blob.put name=b1 size=9000",      /* gen 3 */
    "REQ w3 history.pin name=keep gen=3",     /* gen 4 */
    "REQ w4 task.spawn program=load",         /* gens 5 (write-ahead), 6 */
    "REQ w5 task.terminate target=@w4",       /* gens 7, 8 */
    "REQ w6 config.set key=mode value=beta",  /* gen 9 */
    "REQ w7 blob.delete name=b1",             /* gen 10: b1 kept by the pin */
    "REQ w8 history.unpin name=keep",         /* gen 11: b1 released */
    "REQ w9 config.set key=final value=done", /* gen 12: reuses released blocks */
};

/* ref=... of the stored response of request `id` into out. */
static int ref_of(const char *id, char *out, uint32_t cap)
{
    const struct eng_action *a = eng_find(&eng, id);
    if (!a)
        return 0;
    for (uint32_t i = 0; i + 5 < a->result_len; i++) {
        if (a->result[i] != ' ' || memcmp(a->result + i + 1, "ref=", 4) != 0)
            continue;
        uint32_t n = 0, j = i + 5;
        while (j < a->result_len && a->result[j] != ' ' && a->result[j] != '\n' && n + 1 < cap)
            out[n++] = a->result[j++];
        out[n] = 0;
        return n > 0;
    }
    return 0;
}

int64_t ps_run_workload(void)
{
    if (ps_state != PS_MOUNTED)
        return NX_M4_CORE_STORE_FAILED;
    static char line[NCI_LINE_MAX + 1];
    for (uint32_t w = 0; w < sizeof(WORKLOAD) / sizeof(WORKLOAD[0]); w++) {
        const char *src = WORKLOAD[w];
        uint32_t n = 0;
        while (*src && n < NCI_LINE_MAX) {
            if (*src == '@') {
                char id[NCI_ID_MAX + 1], ref[NCI_REF_MAX];
                uint32_t k = 0;
                src++;
                while (*src && *src != ' ' && k < NCI_ID_MAX)
                    id[k++] = *src++;
                id[k] = 0;
                if (!ref_of(id, ref, sizeof(ref))) {
                    u_printf("workload: no reference from %s\n", id);
                    return NX_M3_CORE_VERIFY_FAILED;
                }
                for (k = 0; ref[k] && n < NCI_LINE_MAX; k++)
                    line[n++] = ref[k];
                continue;
            }
            line[n++] = *src++;
        }
        line[n] = 0;
        u_printf("workload step %u: %s\n", w + 1, line);
        core_handle(line, n);
        char id[8];
        uint32_t k = 0;
        for (const char *p = line + 4; *p && *p != ' ' && k < 7; p++)
            id[k++] = *p;
        id[k] = 0;
        const struct eng_action *a = eng_find(&eng, id);
        if (!a || a->state != ACT_SUCCEEDED) {
            u_printf("workload step %u failed: %s\n", w + 1, a ? eng_state_name(a->state) : "?");
            return NX_M3_CORE_VERIFY_FAILED;
        }
    }
    struct st_check_report cr;
    int st = st_check(&store, &cr);
    u_printf("workload done gen=%lu commits=%lu check=%s\n", store.cur.gen, commits_ok,
             st == ST_OK ? "ok" : cr.first);
    return st == ST_OK && !verify_failures ? NX_M3_CORE_OK : NX_M4_CORE_STORE_FAILED;
}

/* Prints the recovered state in a form the harness compares with the
 * independent reader (tools/store/nxstore.py) and runs the full check. */
int64_t ps_run_check(void)
{
    if (ps_state != PS_MOUNTED) {
        u_printf("m4 state unmountable: %s\n", st_strerror(mount_err));
        return NX_M4_CORE_STORE_FAILED;
    }
    const struct st_root *c = &store.cur;
    u_printf("m4 state gen=%lu label=%s objects=%u pins=%u history=%u\n", c->gen, c->label,
             c->nobj, c->npin, c->nhist);
    for (uint32_t i = 0; i < c->nobj; i++) {
        struct st_obj o = c->obj[i];
        uint32_t len = 0;
        int st = st_read(&store, 0, o.name, objbuf, sizeof(objbuf), &len, 0);
        if (o.kind == NX_M4_KIND_CONFIG) {
            objbuf[len < NCI_VAL_MAX ? len : NCI_VAL_MAX] = 0;
            u_printf("m4 state cfg %s=%s version=%u\n", o.name + 4,
                     st == ST_OK ? (const char *)objbuf : "?", o.version);
        } else if (o.kind == NX_M4_KIND_BLOB) {
            uint32_t h = nx_m4_name_hash(o.name + 5), ok = st == ST_OK;
            for (uint32_t k = 0; ok && k < len; k++)
                ok = objbuf[k] == nx_m4_blob_byte(h, len, k);
            u_printf("m4 state blob %s size=%u ok=%s\n", o.name + 5, len, ok ? "yes" : "no");
        }
    }
    for (uint32_t i = 0; i < c->npin; i++)
        u_printf("m4 state pin %s=%lu\n", c->pin[i].name, c->pin[i].ref.gen);
    for (uint32_t seq = 1; seq < eng.next_seq; seq++)
        for (uint32_t i = 0; i < ENG_SLOTS; i++)
            if (eng.a[i].state != ACT_NONE && eng.a[i].seq == seq && eng.a[i].persist)
                u_printf("m4 state task %s %s %s\n", eng.a[i].id, eng.a[i].op,
                         eng_state_name(eng.a[i].state));
    struct st_check_report cr;
    int st = st_check(&store, &cr);
    u_printf("m4 fsck %s roots=%u objects=%u extents=%u used=%u free=%u problems=%u%s%s\n",
             st == ST_OK ? "ok" : "FAIL", cr.roots, cr.objects, cr.extents, cr.used_blocks,
             cr.free_blocks, cr.problems, cr.problems ? " first=" : "", cr.first);
    return st == ST_OK ? NX_M3_CORE_OK : NX_M4_CORE_STORE_FAILED;
}
