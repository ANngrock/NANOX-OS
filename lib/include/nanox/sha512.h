/* SHA-512 and SHA-384 (FIPS 180-4), M5.  Freestanding; also compiled for
 * host tests (checked against Python's hashlib). */
#ifndef NANOX_LIB_SHA512_H
#define NANOX_LIB_SHA512_H

#include <stdint.h>

#define NX_SHA512_DIGEST_SIZE 64u
#define NX_SHA384_DIGEST_SIZE 48u

struct nx_sha512 {
    uint64_t h[8];
    uint64_t total_len;
    uint8_t block[128];
    uint32_t block_len;
    uint32_t out_len; /* 64 (SHA-512) or 48 (SHA-384) */
};

void nx_sha512_init(struct nx_sha512 *ctx);
void nx_sha384_init(struct nx_sha512 *ctx);
void nx_sha512_update(struct nx_sha512 *ctx, const void *data, uint64_t len);
/* Writes ctx->out_len bytes. */
void nx_sha512_final(struct nx_sha512 *ctx, uint8_t *out);
void nx_sha384(const void *data, uint64_t len, uint8_t out[NX_SHA384_DIGEST_SIZE]);
void nx_sha512(const void *data, uint64_t len, uint8_t out[NX_SHA512_DIGEST_SIZE]);

#endif
