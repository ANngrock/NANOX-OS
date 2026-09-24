/*
 * nxstore v1 -- the persistent object store of NANOX (M4).  Pure code over
 * an abstract block device (no system calls, no allocator): linked into
 * bin/core, compiled into the host tests and the host tool storetool.
 * Normative description of the on-disk format and of the durability
 * protocol: docs/m4-store.md.  An independent reader in Python:
 * tools/store/nxstore.py.
 *
 * Model (ARCHITECTURE.md §10): immutable content blocks, versioned object
 * metadata, state roots (one root block per generation) and a commit log.
 * Copy-on-write: a commit never overwrites a block that a retained
 * generation references; it writes new data and a new root, flushes,
 * writes the superblock slot that does not hold the current generation,
 * and flushes again.  Only then is the generation reported saved.
 *
 * Disk layout (4096-byte blocks, little endian):
 *   block 0, 1   superblock slots A and B (struct st_super, CRC-32 at the
 *                end of the block over the rest of the block)
 *   block 2..    root blocks (struct st_root) and object data extents
 */
#ifndef NANOX_LIB_STORE_H
#define NANOX_LIB_STORE_H

#include <stdint.h>

#define ST_BLOCK 4096u
#define ST_VERSION 1u
#define ST_SB_SLOTS 2u
#define ST_FIRST_DATA 2u
#define ST_MIN_BLOCKS 16u
#define ST_MAX_BLOCKS 32768u /* 128 MiB: size of the in-memory bitmaps */
#define ST_HIST_MAX 8u       /* retained earlier generations listed in a root */
#define ST_PIN_MAX 4u
#define ST_LOG_MAX 16u
#define ST_OBJ_MAX 49u
#define ST_NAME_MAX 27u      /* object name: [A-Za-z0-9._/-]{1,27} */
#define ST_PIN_NAME_MAX 15u  /* pin name: [a-z0-9_-]{1,15} */
#define ST_LABEL_MAX 15u     /* commit label: printable, no spaces */
#define ST_OBJ_MAX_BLOCKS 8u
#define ST_OBJ_MAX_BYTES (ST_OBJ_MAX_BLOCKS * ST_BLOCK)
#define ST_RETAIN_MIN 2u     /* current + the generation in the other slot */
#define ST_RETAIN_MAX (ST_HIST_MAX + 1u)
/* An ordinary put must leave this many blocks free, so that a full store
 * can always release space: a transaction that deletes, unpins or prunes
 * (st_allow_reserve) may use them for its own small record and root, and
 * so may the next one, whose commit drops the parent generation that
 * still referenced the deleted data (2 x (4 + 1) blocks). */
#define ST_RESERVE_BLOCKS 10u

#define ST_SUPER_MAGIC "NXSTSUP1"
#define ST_ROOT_MAGIC "NXSTROOT"

/* Reference to the root block of a generation. */
struct st_ref {
    uint64_t gen;
    uint32_t blk;
    uint32_t crc; /* CRC of the root block (= st_root.crc) */
};

struct st_pin {
    char name[16];
    struct st_ref ref;
};

/* Commit log entry (kept for ST_LOG_MAX commits, even after the content of
 * the generation has been released). */
struct st_log {
    uint64_t gen;
    uint64_t boot_id;
    char label[16];
};

struct st_obj {
    uint64_t oid;      /* identity, never reused within a store */
    uint64_t mod_gen;  /* generation of the last change */
    uint32_t version;  /* 1 on creation, +1 on every change */
    uint32_t blk;      /* first block of the data extent (0: empty object) */
    uint32_t len;      /* bytes */
    uint32_t crc;      /* CRC-32 of the len data bytes */
    uint16_t kind;     /* ST_KIND_* (opaque to the store) */
    uint16_t nblk;     /* extent length = ceil(len / ST_BLOCK) */
    char name[28];
};

struct st_super {
    char magic[8];
    uint32_t version, block_size;
    uint64_t block_count;
    uint64_t store_id;
    uint64_t gen;
    uint32_t root_blk, root_crc;
    uint32_t retain, reserved0;
    uint64_t boot_id; /* boot that committed this generation (diagnostic) */
    uint8_t zero[ST_BLOCK - 64 - 4];
    uint32_t crc; /* CRC-32 of bytes [0, ST_BLOCK - 4) */
};

struct st_root {
    char magic[8];
    uint64_t gen, parent_gen, store_id, next_oid, boot_id;
    uint32_t nobj, nhist, npin, nlog;
    uint32_t retain, flags;
    char label[16];
    uint8_t reserved[40];
    struct st_ref hist[ST_HIST_MAX]; /* retained earlier generations, newest first */
    struct st_pin pin[ST_PIN_MAX];
    struct st_log log[ST_LOG_MAX];   /* newest first; log[0] is this commit */
    struct st_obj obj[ST_OBJ_MAX];
    uint8_t pad[60];
    uint32_t crc; /* CRC-32 of bytes [0, ST_BLOCK - 4) */
};

_Static_assert(sizeof(struct st_super) == ST_BLOCK, "superblock is one block");
_Static_assert(sizeof(struct st_root) == ST_BLOCK, "root is one block");
_Static_assert(sizeof(struct st_obj) == 64, "object entry layout");

/* Block device: 0 on success, negative on error.  Blocks of ST_BLOCK bytes.
 * write() may be volatile until flush() returns (a write-back cache that
 * persists unflushed writes in any order or not at all). */
struct st_dev {
    void *ctx;
    uint64_t blocks;
    int (*read)(void *ctx, uint64_t blk, uint32_t n, void *buf);
    int (*write)(void *ctx, uint64_t blk, uint32_t n, const void *buf);
    int (*flush)(void *ctx);
};

enum st_error {
    ST_OK = 0,
    ST_E_IO = -1,       /* device error before the superblock write: nothing changed */
    ST_E_NOROOT = -2,   /* mount: no valid generation on the device */
    ST_E_NOSPC = -3,    /* no free extent of the size needed */
    ST_E_OBJFULL = -4,  /* object table of the root is full */
    ST_E_NOTFOUND = -5,
    ST_E_TOOBIG = -6,
    ST_E_NAME = -7,     /* malformed object, pin or label name */
    ST_E_CORRUPT = -8,  /* checksum or structure mismatch while reading */
    ST_E_PRUNED = -9,   /* generation not retained any more */
    ST_E_STATE = -10,   /* not mounted, no transaction, or transaction active */
    ST_E_PINFULL = -11,
    ST_E_EXISTS = -12,
    ST_E_FORMAT = -13,  /* device too small or too large */
    ST_E_UNKNOWN = -14, /* device error after the superblock write: outcome unknown,
                           the store is unmounted and must be mounted again */
};

/* Slot states reported by st_mount. */
enum st_slot_state {
    ST_SLOT_EMPTY = 0,   /* no superblock magic */
    ST_SLOT_BAD_CRC,     /* superblock checksum mismatch (e.g. torn write) */
    ST_SLOT_BAD_HEADER,  /* wrong version, block size or block count */
    ST_SLOT_BAD_ROOT,    /* root block unreadable, checksum or structure mismatch */
    ST_SLOT_BAD_DATA,    /* an object extent of the generation does not verify */
    ST_SLOT_CURRENT,     /* mounted */
    ST_SLOT_OLDER,       /* valid, older than the mounted generation */
};

struct st_mount_report {
    int slot_state[ST_SB_SLOTS];
    uint64_t slot_gen[ST_SB_SLOTS]; /* valid when the superblock checksum is ok */
    int slot;                       /* mounted slot */
    uint64_t gen;                   /* mounted generation */
    uint64_t max_gen;               /* highest generation number seen */
    int fallback;                   /* a newer generation was found and rejected */
    uint64_t rejected_gen;
    int rejected_state;
    uint32_t hist_damaged;          /* retained earlier roots that failed to verify */
};

/* Negative controls for the host crash simulation only (production: 0). */
#define ST_TEST_NO_BARRIER (1u << 0)     /* no flush between root and superblock */
#define ST_TEST_NO_FINAL_FLUSH (1u << 1) /* report saved before the superblock is flushed */
#define ST_TEST_NO_VERIFY (1u << 2)      /* mount trusts the newest superblock */
#define ST_TEST_SAME_SLOT (1u << 3)      /* always overwrite slot A */
#define ST_TEST_IN_PLACE (1u << 4)       /* allocator ignores retained generations */

#define ST_CHECK_EXTENTS_MAX ((1u + ST_HIST_MAX + ST_PIN_MAX) * ST_OBJ_MAX)

struct st_extent {
    uint32_t blk, nblk, crc, owner; /* owner: index of the root */
    uint64_t oid;
};

struct st_check_report {
    uint32_t roots, objects, extents, used_blocks, free_blocks;
    uint32_t problems;
    char first[80]; /* first problem, printable */
};

struct store {
    struct st_dev dev;
    uint32_t flags; /* ST_TEST_* */
    int mounted;
    int in_tx, tx_prune, tx_reserve;
    int cur_slot;
    uint32_t cur_blk;
    uint64_t max_gen;
    uint64_t boot_id;
    struct st_root cur;  /* durable current generation */
    struct st_root work; /* transaction being built */
    struct st_root tmp;  /* scratch */
    uint8_t buf[ST_BLOCK];
    uint8_t used[ST_MAX_BLOCKS / 8];   /* referenced by a retained generation */
    uint8_t txused[ST_MAX_BLOCKS / 8]; /* allocated by the open transaction */
    struct st_mount_report rep;
    struct st_extent ext[ST_CHECK_EXTENTS_MAX];
    uint64_t commits, writes, flushes, reads;
};

const char *st_strerror(int err);
const char *st_slot_state_name(int state);

/* Writes an empty store: generation 1 with no objects in slot A, slot B
 * zeroed.  `retain` = generations whose content is kept (ST_RETAIN_MIN..
 * ST_RETAIN_MAX). */
int st_format(struct store *s, const struct st_dev *dev, uint64_t store_id, uint32_t retain,
              uint64_t boot_id);
/* Recovery: reads both slots and mounts the newest generation whose
 * superblock, root and every data extent verify.  Never writes. */
int st_mount(struct store *s, const struct st_dev *dev, uint64_t boot_id, uint32_t flags);

int st_begin(struct store *s);
/* Writes the data to new blocks (not yet referenced) and records the new
 * version of `name` in the transaction. */
int st_put(struct store *s, const char *name, uint16_t kind, const void *data, uint32_t len);
int st_del(struct store *s, const char *name);
/* Pins a retained generation (current or earlier) under `pin`. */
int st_pin(struct store *s, const char *pin, uint64_t gen);
int st_unpin(struct store *s, const char *pin);
/* The committed generation keeps only the minimum history (ST_RETAIN_MIN). */
void st_prune(struct store *s);
/* This transaction releases space: its puts may use the reserved blocks. */
void st_allow_reserve(struct store *s);
/* The durability protocol.  ST_OK only after the second flush: then *gen is
 * durable.  Any other result: the transaction is dropped; ST_E_UNKNOWN
 * additionally unmounts the store (remount to learn which generation is
 * current). */
int st_commit(struct store *s, const char *label, uint64_t *gen);
void st_abort(struct store *s);
/* Generation number the next commit will get. */
uint64_t st_next_gen(const struct store *s);

/* Object of the durable current generation (NULL: none). */
const struct st_obj *st_find(const struct st_root *r, const char *name);
/* Reads an object of generation `gen` (0: current) and verifies its CRC.
 * ST_E_PRUNED if the generation is not retained. */
int st_read(struct store *s, uint64_t gen, const char *name, void *buf, uint32_t cap,
            uint32_t *len, struct st_obj *meta);
/* Root of a retained generation (verified) into *out. */
int st_root_of(struct store *s, uint64_t gen, struct st_root *out);
int st_is_retained(const struct store *s, uint64_t gen);
uint32_t st_free_blocks(const struct store *s);
/* Full consistency check of every retained generation (roots, extents,
 * overlaps, allocation map).  Returns ST_OK or ST_E_CORRUPT. */
int st_check(struct store *s, struct st_check_report *r);

/* Validity of names (exported for callers building names). */
int st_name_ok(const char *name);
int st_pin_name_ok(const char *name);
int st_label_ok(const char *label);

#endif
