/*
 * Hash functions behind one interface (M5): SHA-256, SHA-384, SHA-512;
 * HMAC (RFC 2104) and HKDF (RFC 5869) over any of them.  Freestanding;
 * also compiled for host tests.
 */
#ifndef NANOX_LIB_HASH_H
#define NANOX_LIB_HASH_H

#include <stdint.h>

#include <nanox/sha256.h>
#include <nanox/sha512.h>

#define NX_HASH_MAX 64u
#define NX_HASH_BLOCK_MAX 128u

enum nx_hash_id { NX_HASH_SHA256 = 1, NX_HASH_SHA384 = 2, NX_HASH_SHA512 = 3 };

struct nx_hash {
    int id;
    uint32_t len, block;
    union {
        struct nx_sha256 s256;
        struct nx_sha512 s512;
    } u;
};

/* Digest length of `id` (0 if unknown). */
uint32_t nx_hash_len(int id);
void nx_hash_init(struct nx_hash *h, int id);
void nx_hash_update(struct nx_hash *h, const void *data, uint64_t len);
void nx_hash_final(struct nx_hash *h, uint8_t *out);
void nx_hash(int id, const void *data, uint64_t len, uint8_t *out);

struct nx_hmac {
    struct nx_hash inner, outer;
};

void nx_hmac_init(struct nx_hmac *m, int id, const void *key, uint32_t key_len);
void nx_hmac_update(struct nx_hmac *m, const void *data, uint64_t len);
void nx_hmac_final(struct nx_hmac *m, uint8_t *out);
void nx_hmac(int id, const void *key, uint32_t key_len, const void *data, uint64_t len,
             uint8_t *out);

/* HKDF-Extract(salt, ikm) -> prk (digest length); salt NULL/0: zeros. */
void nx_hkdf_extract(int id, const void *salt, uint32_t salt_len, const void *ikm,
                     uint32_t ikm_len, uint8_t *prk);
/* HKDF-Expand(prk, info, len <= 255 * digest). */
void nx_hkdf_expand(int id, const uint8_t *prk, const void *info, uint32_t info_len,
                    uint8_t *out, uint32_t len);

/* Constant-time comparison: 1 if equal. */
int nx_ct_equal(const void *a, const void *b, uint32_t len);
/* Overwrites len bytes with zeros (not optimised away). */
void nx_wipe(void *p, uint32_t len);

#endif
