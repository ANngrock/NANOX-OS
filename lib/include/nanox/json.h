/*
 * JSON (RFC 8259) for the M5 provider client: a bounded writer for request
 * bodies and a validating reader that works on spans of the received text
 * without building a tree (docs/m5-net.md §8).  Nesting is limited to
 * JSON_DEPTH_MAX; everything deeper is treated as malformed.
 *
 * Pure code: linked into bin/core and compiled into the host tests.
 */
#ifndef NANOX_LIB_JSON_H
#define NANOX_LIB_JSON_H

#include <stdint.h>

#define JSON_DEPTH_MAX 32u

/* ---- writer ---- */

struct jw {
    char *p;
    uint32_t cap, len;
    int overflow; /* something did not fit; the text is unusable */
};

void jw_init(struct jw *w, char *mem, uint32_t cap);
void jw_raw(struct jw *w, const char *s, uint32_t n);
void jw_rawz(struct jw *w, const char *s);
/* A string literal of n bytes of UTF-8 (quotes, backslash and control
 * characters escaped; bytes >= 0x80 are copied, invalid UTF-8 becomes
 * U+FFFD so that the text stays valid JSON). */
void jw_str(struct jw *w, const char *s, uint32_t n);
void jw_strz(struct jw *w, const char *s);
void jw_u64(struct jw *w, uint64_t v);

/* ---- reader ---- */

struct jspan {
    const char *p;
    uint32_t len;
};

enum json_type {
    JSON_BAD = 0,
    JSON_OBJECT = 'o',
    JSON_ARRAY = 'a',
    JSON_STRING = 's',
    JSON_NUMBER = 'n',
    JSON_TRUE = 't',
    JSON_FALSE = 'f',
    JSON_NULL = 'z',
};

/* 1 if s is exactly one well-formed JSON value (surrounding whitespace
 * allowed), else 0. */
int json_valid(const char *s, uint32_t len);
/* Type of the (whitespace-trimmed) value in v; JSON_BAD if not one value. */
int json_type(struct jspan v);
/* Member `key` of the object v: 1 found (*out = the value, trimmed),
 * 0 absent, -1 v is not a well-formed object. */
int json_member(struct jspan v, const char *key, struct jspan *out);
/* Element `index` of the array v: 1 found, 0 out of range, -1 not an array. */
int json_element(struct jspan v, uint32_t index, uint32_t *count, struct jspan *out);
/* Decodes the string value v into out as NUL-terminated UTF-8: its length,
 * or -1 if v is not a string, holds a NUL or does not fit in cap - 1. */
int json_string(struct jspan v, char *out, uint32_t cap);
/* Non-negative integer value: 1 ok, 0 not such a number. */
int json_u64(struct jspan v, uint64_t *out);
/* Iterates over the members of object v: *pos starts at 0.  1 next member
 * (key decoded into key, value in *val), 0 end, -1 malformed or a key
 * longer than kcap - 1. */
int json_next_member(struct jspan v, uint32_t *pos, char *key, uint32_t kcap,
                     struct jspan *val);
/* Convenience: member `key` of v decoded as a string (1 ok, else 0). */
int json_member_str(struct jspan v, const char *key, char *out, uint32_t cap);

#endif
