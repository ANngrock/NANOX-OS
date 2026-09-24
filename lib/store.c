/*
 * nxstore v1: see nanox/store.h and docs/m4-store.md.
 *
 * Invariants the durability protocol relies on (checked by the host crash
 * simulation in tests/host/test_store.c):
 *
 *  I1  A commit writes only blocks that no retained generation of the
 *      durable current root references (s->used) and that the open
 *      transaction has not written yet (s->txused).
 *  I2  The new root and every extent it references are flushed before the
 *      superblock that points to the root is written.
 *  I3  The superblock goes to the slot that does not hold the current
 *      generation; the current slot is never written by a commit.
 *  I4  The generation is reported saved (st_commit returns ST_OK) only
 *      after the flush that follows the superblock write.
 *  I5  Mount never writes and accepts a generation only if its superblock,
 *      its root and every data extent verify; otherwise it falls back to
 *      the other slot.
 *  I6  The generation held by the other slot is retained by the current
 *      one (retain >= 2), so falling back never finds reused blocks.
 */
#include <nanox/crc32.h>
#include <nanox/store.h>
#include <nanox/string.h>

/* ---- small helpers ---------------------------------------------------------- */

static uint32_t block_crc(const void *blk)
{
    return nx_crc32(blk, ST_BLOCK - 4);
}

static int str_eq(const char *a, const char *b)
{
    while (*a && *a == *b) {
        a++;
        b++;
    }
    return *a == *b;
}

/* Length of a NUL-terminated string within `cap` bytes, or cap if none. */
static uint32_t str_nlen(const char *s, uint32_t cap)
{
    uint32_t n = 0;
    while (n < cap && s[n])
        n++;
    return n;
}

static void str_copy(char *dst, uint32_t cap, const char *src)
{
    uint32_t i = 0;
    for (; i + 1 < cap && src[i]; i++)
        dst[i] = src[i];
    for (; i < cap; i++)
        dst[i] = 0;
}

static int name_char(char c)
{
    return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
           c == '.' || c == '_' || c == '-' || c == '/';
}

int st_name_ok(const char *name)
{
    uint32_t n = 0;
    for (; name[n]; n++)
        if (n >= ST_NAME_MAX || !name_char(name[n]))
            return 0;
    return n > 0;
}

int st_pin_name_ok(const char *name)
{
    uint32_t n = 0;
    for (; name[n]; n++) {
        char c = name[n];
        if (n >= ST_PIN_NAME_MAX ||
            !((c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '_' || c == '-'))
            return 0;
    }
    return n > 0;
}

int st_label_ok(const char *label)
{
    uint32_t n = 0;
    for (; label[n]; n++)
        if (n >= ST_LABEL_MAX || label[n] <= ' ' || label[n] > '~')
            return 0;
    return n > 0;
}

static void bit_set(uint8_t *map, uint64_t b)
{
    map[b >> 3] |= (uint8_t)(1u << (b & 7));
}

static int bit_get(const uint8_t *map, uint64_t b)
{
    return (map[b >> 3] >> (b & 7)) & 1;
}

const char *st_strerror(int err)
{
    switch (err) {
    case ST_OK: return "ok";
    case ST_E_IO: return "io_error";
    case ST_E_NOROOT: return "no_valid_root";
    case ST_E_NOSPC: return "no_space";
    case ST_E_OBJFULL: return "object_table_full";
    case ST_E_NOTFOUND: return "not_found";
    case ST_E_TOOBIG: return "too_big";
    case ST_E_NAME: return "bad_name";
    case ST_E_CORRUPT: return "corrupt";
    case ST_E_PRUNED: return "pruned";
    case ST_E_STATE: return "bad_state";
    case ST_E_PINFULL: return "pin_table_full";
    case ST_E_EXISTS: return "exists";
    case ST_E_FORMAT: return "bad_device_size";
    case ST_E_UNKNOWN: return "outcome_unknown";
    default: return "unknown_error";
    }
}

const char *st_slot_state_name(int state)
{
    switch (state) {
    case ST_SLOT_EMPTY: return "empty";
    case ST_SLOT_BAD_CRC: return "bad_crc";
    case ST_SLOT_BAD_HEADER: return "bad_header";
    case ST_SLOT_BAD_ROOT: return "bad_root";
    case ST_SLOT_BAD_DATA: return "bad_data";
    case ST_SLOT_CURRENT: return "current";
    case ST_SLOT_OLDER: return "older";
    default: return "?";
    }
}

/* ---- device access ------------------------------------------------------------ */

static int dev_read(struct store *s, uint64_t blk, uint32_t n, void *buf)
{
    if (blk + n > s->dev.blocks || blk + n < blk)
        return ST_E_CORRUPT;
    s->reads++;
    return s->dev.read(s->dev.ctx, blk, n, buf) == 0 ? ST_OK : ST_E_IO;
}

static int dev_write(struct store *s, uint64_t blk, uint32_t n, const void *buf)
{
    if (blk + n > s->dev.blocks || blk + n < blk)
        return ST_E_IO;
    s->writes++;
    return s->dev.write(s->dev.ctx, blk, n, buf) == 0 ? ST_OK : ST_E_IO;
}

static int dev_flush(struct store *s)
{
    s->flushes++;
    return s->dev.flush(s->dev.ctx) == 0 ? ST_OK : ST_E_IO;
}

/* ---- structure checks ------------------------------------------------------------ */

static uint32_t nblk_for(uint32_t len)
{
    return (len + ST_BLOCK - 1) / ST_BLOCK;
}

static int overlap(uint32_t a, uint32_t an, uint32_t b, uint32_t bn)
{
    return a < b + bn && b < a + an;
}

/* Structural validity of a root (not its checksum). */
static int root_sane(const struct st_root *r, uint64_t blocks)
{
    if (memcmp(r->magic, ST_ROOT_MAGIC, 8) != 0 || r->gen == 0 || r->parent_gen >= r->gen ||
        r->nobj > ST_OBJ_MAX || r->nhist > ST_HIST_MAX || r->npin > ST_PIN_MAX ||
        r->nlog == 0 || r->nlog > ST_LOG_MAX || r->log[0].gen != r->gen ||
        r->retain < ST_RETAIN_MIN || r->retain > ST_RETAIN_MAX || r->next_oid == 0)
        return 0;
    if (str_nlen(r->label, sizeof(r->label)) == sizeof(r->label))
        return 0;
    for (uint32_t i = 0; i < r->nhist; i++) {
        const struct st_ref *h = &r->hist[i];
        if (h->gen >= (i ? r->hist[i - 1].gen : r->gen) || h->gen == 0 ||
            h->blk < ST_FIRST_DATA || h->blk >= blocks)
            return 0;
    }
    for (uint32_t i = 0; i < r->npin; i++) {
        const struct st_pin *p = &r->pin[i];
        if (str_nlen(p->name, sizeof(p->name)) == sizeof(p->name) || !st_pin_name_ok(p->name) ||
            p->ref.gen == 0 || p->ref.gen >= r->gen || p->ref.blk < ST_FIRST_DATA ||
            p->ref.blk >= blocks)
            return 0;
        for (uint32_t j = 0; j < i; j++)
            if (str_eq(r->pin[j].name, p->name))
                return 0;
    }
    for (uint32_t i = 0; i < r->nobj; i++) {
        const struct st_obj *o = &r->obj[i];
        if (str_nlen(o->name, sizeof(o->name)) == sizeof(o->name) || !st_name_ok(o->name) ||
            o->oid == 0 || o->oid >= r->next_oid || o->version == 0 || o->mod_gen == 0 ||
            o->mod_gen > r->gen || o->len > ST_OBJ_MAX_BYTES || o->nblk != nblk_for(o->len))
            return 0;
        if (o->nblk ? (o->blk < ST_FIRST_DATA || (uint64_t)o->blk + o->nblk > blocks)
                    : o->blk != 0)
            return 0;
        for (uint32_t j = 0; j < i; j++) {
            const struct st_obj *p = &r->obj[j];
            if (str_eq(p->name, o->name) || p->oid == o->oid ||
                (o->nblk && p->nblk && overlap(o->blk, o->nblk, p->blk, p->nblk)))
                return 0;
        }
    }
    return 1;
}

/* Reads block `blk` into *out and checks it is the root `gen` with checksum
 * `crc` (gen/crc 0: not checked). */
static int load_root(struct store *s, uint32_t blk, uint64_t gen, uint32_t crc,
                     struct st_root *out)
{
    if (blk < ST_FIRST_DATA || blk >= s->dev.blocks)
        return ST_E_CORRUPT;
    int st = dev_read(s, blk, 1, out);
    if (st != ST_OK)
        return st;
    uint32_t c = block_crc(out);
    if (c != out->crc || (crc && c != crc) || (gen && out->gen != gen) ||
        !root_sane(out, s->dev.blocks))
        return ST_E_CORRUPT;
    return ST_OK;
}

/* Reads the extent of `o` block by block and checks its CRC; copies the
 * first `cap` bytes to `dst` if dst is not NULL. */
static int verify_extent(struct store *s, const struct st_obj *o, void *dst, uint32_t cap)
{
    uint32_t crc = 0, left = o->len;
    for (uint32_t i = 0; i < o->nblk; i++) {
        int st = dev_read(s, (uint64_t)o->blk + i, 1, s->buf);
        if (st != ST_OK)
            return st;
        uint32_t n = left < ST_BLOCK ? left : ST_BLOCK;
        crc = nx_crc32_update(crc, s->buf, n);
        if (dst) {
            uint32_t off = i * ST_BLOCK;
            if (off < cap)
                memcpy((uint8_t *)dst + off, s->buf, cap - off < n ? cap - off : n);
        }
        left -= n;
    }
    return crc == o->crc ? ST_OK : ST_E_CORRUPT;
}

static void mark_root(uint8_t *map, const struct st_root *r, uint32_t blk, uint64_t blocks)
{
    if (blk < blocks)
        bit_set(map, blk);
    for (uint32_t i = 0; i < r->nobj; i++)
        for (uint32_t b = 0; b < r->obj[i].nblk; b++)
            if ((uint64_t)r->obj[i].blk + b < blocks)
                bit_set(map, (uint64_t)r->obj[i].blk + b);
}

/* s->used := blocks referenced by the current generation, its retained
 * history and its pins, plus the superblocks.  Returns how many retained
 * roots did not verify. */
static uint32_t recompute_used(struct store *s)
{
    uint32_t damaged = 0;
    memset(s->used, 0, sizeof(s->used));
    bit_set(s->used, 0);
    bit_set(s->used, 1);
    mark_root(s->used, &s->cur, s->cur_blk, s->dev.blocks);
    for (uint32_t i = 0; i < s->cur.nhist + s->cur.npin; i++) {
        const struct st_ref *ref = i < s->cur.nhist ? &s->cur.hist[i]
                                                     : &s->cur.pin[i - s->cur.nhist].ref;
        if (ref->blk < s->dev.blocks)
            bit_set(s->used, ref->blk); /* conservative even if damaged */
        if (load_root(s, ref->blk, ref->gen, ref->crc, &s->tmp) != ST_OK) {
            damaged++;
            continue;
        }
        mark_root(s->used, &s->tmp, ref->blk, s->dev.blocks);
    }
    return damaged;
}

uint32_t st_free_blocks(const struct store *s)
{
    uint32_t n = 0;
    for (uint64_t b = ST_FIRST_DATA; b < s->dev.blocks; b++)
        n += !bit_get(s->used, b);
    return n;
}

/* First fit of n consecutive blocks free in both maps; 0 if none.  Unless
 * `reserve_ok`, ST_RESERVE_BLOCKS must stay free afterwards. */
static uint32_t alloc(struct store *s, uint32_t n, int reserve_ok)
{
    int in_place = (s->flags & ST_TEST_IN_PLACE) != 0;
    uint32_t free_total = 0;
    for (uint64_t b = ST_FIRST_DATA; b < s->dev.blocks; b++)
        free_total += !bit_get(s->txused, b) && (in_place || !bit_get(s->used, b));
    if (free_total < n || (!reserve_ok && free_total - n < ST_RESERVE_BLOCKS))
        return 0;
    uint32_t run = 0;
    for (uint64_t b = ST_FIRST_DATA; b < s->dev.blocks; b++) {
        int is_free = !bit_get(s->txused, b) && (in_place || !bit_get(s->used, b));
        run = is_free ? run + 1 : 0;
        if (run == n) {
            uint32_t first = (uint32_t)(b + 1 - n);
            for (uint32_t i = 0; i < n; i++)
                bit_set(s->txused, (uint64_t)first + i);
            return first;
        }
    }
    return 0;
}

/* ---- format and mount ----------------------------------------------------------- */

static void make_super(struct st_super *sb, const struct st_root *r, uint32_t root_blk,
                       uint64_t blocks)
{
    memset(sb, 0, sizeof(*sb));
    memcpy(sb->magic, ST_SUPER_MAGIC, 8);
    sb->version = ST_VERSION;
    sb->block_size = ST_BLOCK;
    sb->block_count = blocks;
    sb->store_id = r->store_id;
    sb->gen = r->gen;
    sb->root_blk = root_blk;
    sb->root_crc = r->crc;
    sb->retain = r->retain;
    sb->boot_id = r->boot_id;
    sb->crc = block_crc(sb);
}

int st_format(struct store *s, const struct st_dev *dev, uint64_t store_id, uint32_t retain,
              uint64_t boot_id)
{
    memset(s, 0, sizeof(*s));
    s->dev = *dev;
    if (dev->blocks < ST_MIN_BLOCKS || dev->blocks > ST_MAX_BLOCKS)
        return ST_E_FORMAT;
    if (retain < ST_RETAIN_MIN || retain > ST_RETAIN_MAX)
        return ST_E_FORMAT;
    struct st_root *r = &s->work;
    memset(r, 0, sizeof(*r));
    memcpy(r->magic, ST_ROOT_MAGIC, 8);
    r->gen = 1;
    r->store_id = store_id;
    r->next_oid = 1;
    r->boot_id = boot_id;
    r->retain = retain;
    r->nlog = 1;
    r->log[0].gen = 1;
    r->log[0].boot_id = boot_id;
    str_copy(r->log[0].label, sizeof(r->log[0].label), "format");
    str_copy(r->label, sizeof(r->label), "format");
    r->crc = block_crc(r);
    memset(s->buf, 0, sizeof(s->buf));
    struct st_super *sb = (struct st_super *)(void *)&s->tmp;
    make_super(sb, r, ST_FIRST_DATA, dev->blocks);
    int st = dev_write(s, ST_FIRST_DATA, 1, r);
    if (st == ST_OK)
        st = dev_write(s, 1, 1, s->buf);
    if (st == ST_OK)
        st = dev_flush(s);
    if (st == ST_OK)
        st = dev_write(s, 0, 1, sb);
    if (st == ST_OK)
        st = dev_flush(s);
    return st;
}

struct slot_info {
    int state;
    uint64_t gen, store_id, blocks;
    uint32_t root_blk, root_crc;
};

static void read_slot(struct store *s, uint32_t slot, struct slot_info *si)
{
    struct st_super *sb = (struct st_super *)(void *)&s->tmp;
    memset(si, 0, sizeof(*si));
    if (s->dev.read(s->dev.ctx, slot, 1, sb) != 0) {
        si->state = ST_SLOT_BAD_CRC;
        return;
    }
    s->reads++;
    if (memcmp(sb->magic, ST_SUPER_MAGIC, 8) != 0) {
        si->state = ST_SLOT_EMPTY;
        return;
    }
    if (block_crc(sb) != sb->crc) {
        si->state = ST_SLOT_BAD_CRC;
        return;
    }
    si->gen = sb->gen;
    si->store_id = sb->store_id;
    si->blocks = sb->block_count;
    si->root_blk = sb->root_blk;
    si->root_crc = sb->root_crc;
    if (sb->version != ST_VERSION || sb->block_size != ST_BLOCK || sb->gen == 0 ||
        sb->block_count < ST_MIN_BLOCKS || sb->block_count > s->dev.blocks) {
        si->state = ST_SLOT_BAD_HEADER;
        return;
    }
    si->state = ST_SLOT_OLDER; /* candidate */
}

/* Tries to mount the generation of slot `si`: its root and every extent must
 * verify.  Returns the slot state. */
static int try_slot(struct store *s, const struct slot_info *si)
{
    uint64_t saved_blocks = s->dev.blocks;
    s->dev.blocks = si->blocks;
    if (s->flags & ST_TEST_NO_VERIFY) {
        if (si->root_blk < ST_FIRST_DATA || si->root_blk >= s->dev.blocks ||
            dev_read(s, si->root_blk, 1, &s->cur) != ST_OK) {
            s->dev.blocks = saved_blocks;
            return ST_SLOT_BAD_ROOT;
        }
        return ST_SLOT_CURRENT;
    }
    if (load_root(s, si->root_blk, si->gen, si->root_crc, &s->cur) != ST_OK ||
        s->cur.store_id != si->store_id) {
        s->dev.blocks = saved_blocks;
        return ST_SLOT_BAD_ROOT;
    }
    for (uint32_t i = 0; i < s->cur.nobj; i++)
        if (verify_extent(s, &s->cur.obj[i], 0, 0) != ST_OK) {
            s->dev.blocks = saved_blocks;
            return ST_SLOT_BAD_DATA;
        }
    return ST_SLOT_CURRENT;
}

int st_mount(struct store *s, const struct st_dev *dev, uint64_t boot_id, uint32_t flags)
{
    memset(s, 0, sizeof(*s));
    s->dev = *dev;
    s->flags = flags;
    s->boot_id = boot_id;
    if (dev->blocks < ST_MIN_BLOCKS)
        return ST_E_FORMAT;
    if (s->dev.blocks > ST_MAX_BLOCKS)
        s->dev.blocks = ST_MAX_BLOCKS;
    struct slot_info si[ST_SB_SLOTS];
    for (uint32_t i = 0; i < ST_SB_SLOTS; i++) {
        read_slot(s, i, &si[i]);
        if (si[i].state == ST_SLOT_OLDER || si[i].state == ST_SLOT_BAD_HEADER)
            s->rep.slot_gen[i] = si[i].gen;
        if ((si[i].state == ST_SLOT_OLDER || si[i].state == ST_SLOT_BAD_HEADER) &&
            si[i].gen > s->max_gen)
            s->max_gen = si[i].gen;
    }
    /* Newest candidate first; slot A wins a tie. */
    uint32_t order[2] = {0, 1};
    if (si[1].state == ST_SLOT_OLDER &&
        (si[0].state != ST_SLOT_OLDER || si[1].gen > si[0].gen)) {
        order[0] = 1;
        order[1] = 0;
    }
    int mounted = -1;
    for (uint32_t k = 0; k < 2; k++) {
        uint32_t i = order[k];
        if (si[i].state != ST_SLOT_OLDER)
            continue;
        if (mounted >= 0)
            continue; /* stays OLDER */
        int st = try_slot(s, &si[i]);
        si[i].state = st;
        if (st == ST_SLOT_CURRENT) {
            mounted = (int)i;
        } else if (!s->rep.fallback) {
            s->rep.fallback = 1;
            s->rep.rejected_gen = si[i].gen;
            s->rep.rejected_state = st;
        }
    }
    for (uint32_t i = 0; i < ST_SB_SLOTS; i++)
        s->rep.slot_state[i] = si[i].state;
    s->rep.max_gen = s->max_gen;
    if (mounted < 0) {
        s->rep.slot = -1;
        return ST_E_NOROOT;
    }
    /* An older valid generation is not a fallback; only a rejected newer one. */
    if (s->rep.fallback && s->rep.rejected_gen < s->cur.gen)
        s->rep.fallback = 0;
    s->cur_slot = mounted;
    s->cur_blk = si[mounted].root_blk;
    s->rep.slot = mounted;
    s->rep.gen = s->cur.gen;
    s->rep.hist_damaged = recompute_used(s);
    s->mounted = 1;
    return ST_OK;
}

/* ---- transactions ------------------------------------------------------------------ */

uint64_t st_next_gen(const struct store *s)
{
    return s->max_gen + 1;
}

int st_begin(struct store *s)
{
    if (!s->mounted || s->in_tx)
        return ST_E_STATE;
    memcpy(&s->work, &s->cur, sizeof(s->work));
    memset(s->txused, 0, sizeof(s->txused));
    s->tx_prune = 0;
    s->tx_reserve = 0;
    s->in_tx = 1;
    return ST_OK;
}

void st_abort(struct store *s)
{
    s->in_tx = 0;
    s->tx_prune = 0;
    s->tx_reserve = 0;
    memset(s->txused, 0, sizeof(s->txused));
}

static int find_index(const struct st_root *r, const char *name)
{
    for (uint32_t i = 0; i < r->nobj; i++)
        if (str_eq(r->obj[i].name, name))
            return (int)i;
    return -1;
}

const struct st_obj *st_find(const struct st_root *r, const char *name)
{
    int i = find_index(r, name);
    return i < 0 ? 0 : &r->obj[i];
}

int st_put(struct store *s, const char *name, uint16_t kind, const void *data, uint32_t len)
{
    if (!s->mounted || !s->in_tx)
        return ST_E_STATE;
    if (!st_name_ok(name))
        return ST_E_NAME;
    if (len > ST_OBJ_MAX_BYTES)
        return ST_E_TOOBIG;
    struct st_root *w = &s->work;
    int idx = find_index(w, name);
    if (idx < 0 && w->nobj == ST_OBJ_MAX)
        return ST_E_OBJFULL;
    uint32_t nblk = nblk_for(len), blk = 0;
    if (nblk) {
        blk = alloc(s, nblk, s->tx_reserve);
        if (!blk)
            return ST_E_NOSPC;
        uint32_t full = len / ST_BLOCK;
        int st = ST_OK;
        if (full)
            st = dev_write(s, blk, full, data);
        if (st == ST_OK && full < nblk) {
            memset(s->buf, 0, sizeof(s->buf));
            memcpy(s->buf, (const uint8_t *)data + (uint64_t)full * ST_BLOCK,
                   len - full * ST_BLOCK);
            st = dev_write(s, (uint64_t)blk + full, 1, s->buf);
        }
        if (st != ST_OK)
            return st;
    }
    struct st_obj *o;
    if (idx < 0) {
        o = &w->obj[w->nobj++];
        memset(o, 0, sizeof(*o));
        o->oid = w->next_oid++;
        o->version = 1;
        str_copy(o->name, sizeof(o->name), name);
    } else {
        o = &w->obj[idx];
        o->version++;
    }
    o->mod_gen = st_next_gen(s);
    o->kind = kind;
    o->blk = blk;
    o->nblk = (uint16_t)nblk;
    o->len = len;
    o->crc = nx_crc32(data, len);
    return ST_OK;
}

int st_del(struct store *s, const char *name)
{
    if (!s->mounted || !s->in_tx)
        return ST_E_STATE;
    struct st_root *w = &s->work;
    int idx = find_index(w, name);
    if (idx < 0)
        return ST_E_NOTFOUND;
    for (uint32_t i = (uint32_t)idx; i + 1 < w->nobj; i++)
        w->obj[i] = w->obj[i + 1];
    w->nobj--;
    memset(&w->obj[w->nobj], 0, sizeof(w->obj[0]));
    return ST_OK;
}

/* Reference to a generation retained by the durable current root. */
static int find_ref(const struct store *s, uint64_t gen, struct st_ref *out)
{
    if (gen == s->cur.gen) {
        out->gen = s->cur.gen;
        out->blk = s->cur_blk;
        out->crc = s->cur.crc;
        return 1;
    }
    for (uint32_t i = 0; i < s->cur.nhist; i++)
        if (s->cur.hist[i].gen == gen) {
            *out = s->cur.hist[i];
            return 1;
        }
    for (uint32_t i = 0; i < s->cur.npin; i++)
        if (s->cur.pin[i].ref.gen == gen) {
            *out = s->cur.pin[i].ref;
            return 1;
        }
    return 0;
}

int st_is_retained(const struct store *s, uint64_t gen)
{
    struct st_ref r;
    return s->mounted && find_ref(s, gen, &r);
}

int st_pin(struct store *s, const char *pin, uint64_t gen)
{
    if (!s->mounted || !s->in_tx)
        return ST_E_STATE;
    if (!st_pin_name_ok(pin))
        return ST_E_NAME;
    struct st_root *w = &s->work;
    for (uint32_t i = 0; i < w->npin; i++)
        if (str_eq(w->pin[i].name, pin))
            return ST_E_EXISTS;
    if (w->npin == ST_PIN_MAX)
        return ST_E_PINFULL;
    struct st_ref ref;
    if (!find_ref(s, gen, &ref))
        return ST_E_PRUNED;
    struct st_pin *p = &w->pin[w->npin++];
    memset(p, 0, sizeof(*p));
    str_copy(p->name, sizeof(p->name), pin);
    p->ref = ref;
    return ST_OK;
}

int st_unpin(struct store *s, const char *pin)
{
    if (!s->mounted || !s->in_tx)
        return ST_E_STATE;
    struct st_root *w = &s->work;
    for (uint32_t i = 0; i < w->npin; i++)
        if (str_eq(w->pin[i].name, pin)) {
            for (uint32_t j = i; j + 1 < w->npin; j++)
                w->pin[j] = w->pin[j + 1];
            w->npin--;
            memset(&w->pin[w->npin], 0, sizeof(w->pin[0]));
            return ST_OK;
        }
    return ST_E_NOTFOUND;
}

void st_prune(struct store *s)
{
    if (s->in_tx)
        s->tx_prune = 1;
}

void st_allow_reserve(struct store *s)
{
    if (s->in_tx)
        s->tx_reserve = 1;
}

int st_commit(struct store *s, const char *label, uint64_t *gen_out)
{
    if (!s->mounted || !s->in_tx)
        return ST_E_STATE;
    if (!st_label_ok(label)) {
        st_abort(s);
        return ST_E_NAME;
    }
    struct st_root *w = &s->work;
    uint64_t gen = st_next_gen(s);
    w->gen = gen;
    w->parent_gen = s->cur.gen;
    w->boot_id = s->boot_id;
    w->flags = 0;
    str_copy(w->label, sizeof(w->label), label);
    /* History: the current generation first, then as many of its own
     * retained generations as the policy allows. */
    uint32_t keep = (s->tx_prune ? ST_RETAIN_MIN : s->cur.retain) - 1;
    memset(w->hist, 0, sizeof(w->hist));
    w->nhist = 0;
    w->hist[w->nhist].gen = s->cur.gen;
    w->hist[w->nhist].blk = s->cur_blk;
    w->hist[w->nhist].crc = s->cur.crc;
    w->nhist++;
    for (uint32_t i = 0; i < s->cur.nhist && w->nhist < keep; i++)
        w->hist[w->nhist++] = s->cur.hist[i];
    memset(w->log, 0, sizeof(w->log));
    w->log[0].gen = gen;
    w->log[0].boot_id = s->boot_id;
    str_copy(w->log[0].label, sizeof(w->log[0].label), label);
    w->nlog = 1;
    for (uint32_t i = 0; i < s->cur.nlog && w->nlog < ST_LOG_MAX; i++)
        w->log[w->nlog++] = s->cur.log[i];

    uint32_t blk = alloc(s, 1, 1);
    if (!blk) {
        st_abort(s);
        return ST_E_NOSPC;
    }
    w->crc = block_crc(w);
    /* Step 1: the root (the data extents were written by st_put). */
    int st = dev_write(s, blk, 1, w);
    /* Step 2: barrier -- data and root durable before anything points to them. */
    if (st == ST_OK && !(s->flags & (ST_TEST_NO_BARRIER)))
        st = dev_flush(s);
    if (st != ST_OK) {
        st_abort(s);
        return ST_E_IO;
    }
    /* Step 3: the superblock, into the slot not holding the current generation. */
    struct st_super *sb = (struct st_super *)(void *)&s->tmp;
    make_super(sb, w, blk, s->dev.blocks);
    int slot = (s->flags & ST_TEST_SAME_SLOT) ? 0 : 1 - s->cur_slot;
    st = dev_write(s, (uint64_t)slot, 1, sb);
    /* Step 4: flush -- only now is the generation durable. */
    if (st == ST_OK && !(s->flags & ST_TEST_NO_FINAL_FLUSH))
        st = dev_flush(s);
    if (st != ST_OK) {
        st_abort(s);
        s->mounted = 0; /* the superblock may or may not have reached the disk */
        return ST_E_UNKNOWN;
    }
    memcpy(&s->cur, w, sizeof(s->cur));
    s->cur_blk = blk;
    s->cur_slot = slot;
    s->max_gen = gen;
    s->in_tx = 0;
    s->tx_prune = 0;
    s->tx_reserve = 0;
    s->commits++;
    memset(s->txused, 0, sizeof(s->txused));
    recompute_used(s);
    if (gen_out)
        *gen_out = gen;
    return ST_OK;
}

/* ---- reading ---------------------------------------------------------------------------- */

int st_root_of(struct store *s, uint64_t gen, struct st_root *out)
{
    if (!s->mounted)
        return ST_E_STATE;
    struct st_ref ref;
    if (!find_ref(s, gen, &ref))
        return ST_E_PRUNED;
    if (gen == s->cur.gen) {
        if (out != &s->cur)
            memcpy(out, &s->cur, sizeof(*out));
        return ST_OK;
    }
    return load_root(s, ref.blk, ref.gen, ref.crc, out);
}

int st_read(struct store *s, uint64_t gen, const char *name, void *buf, uint32_t cap,
            uint32_t *len, struct st_obj *meta)
{
    if (!s->mounted)
        return ST_E_STATE;
    const struct st_root *r = &s->cur;
    if (gen && gen != s->cur.gen) {
        int st = st_root_of(s, gen, &s->tmp);
        if (st != ST_OK)
            return st;
        r = &s->tmp;
    }
    const struct st_obj *o = st_find(r, name);
    if (!o)
        return ST_E_NOTFOUND;
    struct st_obj copy = *o;
    if (meta)
        *meta = copy;
    if (len)
        *len = copy.len;
    if (copy.len > cap)
        return ST_E_TOOBIG;
    return verify_extent(s, &copy, buf, cap);
}

/* ---- consistency check --------------------------------------------------------------------- */

static void problem(struct st_check_report *r, const char *what, uint64_t v)
{
    if (r->problems++)
        return;
    uint32_t n = 0;
    for (; what[n] && n < sizeof(r->first) - 24; n++)
        r->first[n] = what[n];
    r->first[n++] = '=';
    char digits[21];
    uint32_t d = 0;
    do {
        digits[d++] = (char)('0' + v % 10);
        v /= 10;
    } while (v);
    while (d)
        r->first[n++] = digits[--d];
    r->first[n] = 0;
}

static void add_extent(struct store *s, struct st_check_report *r, uint32_t *n, uint32_t blk,
                       uint32_t nblk, uint32_t crc, uint64_t oid, uint32_t owner)
{
    for (uint32_t i = 0; i < *n; i++) {
        struct st_extent *e = &s->ext[i];
        if (e->blk == blk && e->nblk == nblk && e->crc == crc && e->oid == oid)
            return; /* shared between generations */
    }
    if (*n == ST_CHECK_EXTENTS_MAX) {
        problem(r, "too_many_extents", *n);
        return;
    }
    struct st_extent *e = &s->ext[(*n)++];
    e->blk = blk;
    e->nblk = nblk;
    e->crc = crc;
    e->oid = oid;
    e->owner = owner;
}

int st_check(struct store *s, struct st_check_report *r)
{
    memset(r, 0, sizeof(*r));
    if (!s->mounted) {
        problem(r, "not_mounted", 0);
        return ST_E_CORRUPT;
    }
    /* The superblock of the current slot on disk must point to the root in memory. */
    struct st_super *sb = (struct st_super *)(void *)s->buf;
    if (dev_read(s, (uint64_t)s->cur_slot, 1, sb) != ST_OK || block_crc(sb) != sb->crc ||
        memcmp(sb->magic, ST_SUPER_MAGIC, 8) != 0 || sb->gen != s->cur.gen ||
        sb->root_blk != s->cur_blk || sb->root_crc != s->cur.crc)
        problem(r, "superblock_mismatch_slot", (uint64_t)s->cur_slot);
    uint32_t next = 0;
    struct st_ref refs[1 + ST_HIST_MAX + ST_PIN_MAX];
    uint32_t nref = 0;
    refs[nref].gen = s->cur.gen;
    refs[nref].blk = s->cur_blk;
    refs[nref].crc = s->cur.crc;
    nref++;
    for (uint32_t i = 0; i < s->cur.nhist + s->cur.npin; i++) {
        const struct st_ref *ref = i < s->cur.nhist ? &s->cur.hist[i]
                                                     : &s->cur.pin[i - s->cur.nhist].ref;
        int dup = 0;
        for (uint32_t j = 0; j < nref; j++)
            dup |= refs[j].gen == ref->gen && refs[j].blk == ref->blk;
        if (!dup)
            refs[nref++] = *ref;
    }
    for (uint32_t k = 0; k < nref; k++) {
        if (load_root(s, refs[k].blk, refs[k].gen, refs[k].crc, &s->tmp) != ST_OK) {
            problem(r, "root_invalid_gen", refs[k].gen);
            continue;
        }
        if (k == 0 && memcmp(&s->tmp, &s->cur, sizeof(s->tmp)) != 0)
            problem(r, "current_root_differs_on_disk_gen", refs[k].gen);
        r->roots++;
        add_extent(s, r, &next, refs[k].blk, 1, refs[k].crc, 0, k);
        for (uint32_t i = 0; i < s->tmp.nobj; i++) {
            struct st_obj o = s->tmp.obj[i];
            r->objects++;
            if (verify_extent(s, &o, 0, 0) != ST_OK)
                problem(r, "extent_crc_oid", o.oid);
            if (o.nblk)
                add_extent(s, r, &next, o.blk, o.nblk, o.crc, o.oid, k);
        }
    }
    r->extents = next;
    /* Distinct extents never overlap; every referenced block is marked used. */
    for (uint32_t i = 0; i < next; i++) {
        const struct st_extent *a = &s->ext[i];
        for (uint32_t j = i + 1; j < next; j++)
            if (overlap(a->blk, a->nblk, s->ext[j].blk, s->ext[j].nblk))
                problem(r, "overlapping_extents_at_block", a->blk);
        for (uint32_t b = 0; b < a->nblk; b++)
            if (!bit_get(s->used, (uint64_t)a->blk + b))
                problem(r, "referenced_block_not_marked_used", (uint64_t)a->blk + b);
    }
    for (uint64_t b = ST_FIRST_DATA; b < s->dev.blocks; b++) {
        if (!bit_get(s->used, b))
            continue;
        r->used_blocks++;
        int found = 0;
        for (uint32_t i = 0; i < next && !found; i++)
            found = b >= s->ext[i].blk && b < (uint64_t)s->ext[i].blk + s->ext[i].nblk;
        if (!found)
            problem(r, "used_block_not_referenced", b);
    }
    r->free_blocks = st_free_blocks(s);
    return r->problems ? ST_E_CORRUPT : ST_OK;
}
