/*
 * NCI wire format, version 1 (M3): parsing of request lines and building of
 * response lines.  Pure code (no system calls), linked into bin/core and
 * compiled into tests/host.  Normative description: docs/m3-core.md §4.
 *
 * Request (host -> guest), one line of at most NCI_LINE_MAX bytes:
 *     REQ <id> <operation> [<key>=<value> ...]
 * Response (guest -> host):
 *     RES <id> <STATE> [<key>=<value> ...]
 *     ITEM <id> [<key>=<value> ...]        (zero or more)
 *     END <id>
 * id: [A-Za-z0-9._-]{1,32}; operation: [a-z.]{1,32}; key: [a-z_]{1,16};
 * value: [A-Za-z0-9._/:,-]{1,64}.  Tokens are separated by spaces.
 */
#ifndef NANOX_CORE_NCI_H
#define NANOX_CORE_NCI_H

#include <stdint.h>

#define NCI_LINE_MAX 256u
#define NCI_ID_MAX 32u
#define NCI_OP_MAX 32u
#define NCI_KEY_MAX 16u
#define NCI_VAL_MAX 64u
#define NCI_KV_MAX 8u

enum nci_status {
    NCI_OK = 0,
    NCI_E_SYNTAX,   /* not "REQ <id> <op> ..." */
    NCI_E_TOO_LONG, /* line longer than NCI_LINE_MAX */
    NCI_E_ID,       /* bad request id */
    NCI_E_OP,       /* bad operation name */
    NCI_E_KV,       /* malformed key=value */
    NCI_E_DUPKEY,   /* the same key twice */
    NCI_E_TOO_MANY, /* more than NCI_KV_MAX arguments */
};

struct nci_kv {
    char key[NCI_KEY_MAX + 1];
    char val[NCI_VAL_MAX + 1];
};

struct nci_req {
    char id[NCI_ID_MAX + 1];
    char op[NCI_OP_MAX + 1];
    uint32_t nkv;
    struct nci_kv kv[NCI_KV_MAX];
};

/* Parses one line (without the '\n'; a trailing '\r' is ignored).  On
 * failure out->id still holds the id if it could be read, else "-". */
int nci_parse(const char *line, uint32_t len, struct nci_req *out);
const char *nci_strerror(int status);
/* Value of `key` or NULL. */
const char *nci_get(const struct nci_req *r, const char *key);
/* Decimal value of `key`: 1 found and valid, 0 absent, -1 malformed. */
int nci_get_u64(const struct nci_req *r, const char *key, uint64_t *out);
/* Hash of the operation and its arguments (in order): a repeated request
 * id must come with the same fingerprint. */
uint64_t nci_fingerprint(const struct nci_req *r);

/* Object reference of a task: "task/<boot id, 16 hex>/<task id>". */
#define NCI_REF_MAX 40u
/* 1: parsed, 0: malformed. */
int nci_ref_parse(const char *s, uint64_t *boot_id, uint32_t *task_id);
void nci_ref_format(char out[NCI_REF_MAX], uint64_t boot_id, uint32_t task_id);

/* Bounded text builder; `overflow` is set when something did not fit. */
struct nci_buf {
    char *p;
    uint32_t cap, len;
    int overflow;
};

void nb_init(struct nci_buf *b, char *mem, uint32_t cap);
void nb_char(struct nci_buf *b, char c);
void nb_str(struct nci_buf *b, const char *s);
void nb_u64(struct nci_buf *b, uint64_t v);
void nb_i64(struct nci_buf *b, int64_t v);
void nb_hex(struct nci_buf *b, uint64_t v, unsigned width);
/* " key=" followed by the value. */
void nb_kv(struct nci_buf *b, const char *key, const char *val);
void nb_kv_u64(struct nci_buf *b, const char *key, uint64_t v);
void nb_kv_i64(struct nci_buf *b, const char *key, int64_t v);
/* True if every byte of s is allowed in a value (and 1..NCI_VAL_MAX long). */
int nci_value_ok(const char *s);

int nci_streq(const char *a, const char *b);

#endif
