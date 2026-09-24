/* SHA-256 (FIPS 180-4).  Freestanding; also compiled for host tests. */
#ifndef NANOX_LIB_SHA256_H
#define NANOX_LIB_SHA256_H

#include <stddef.h>
#include <stdint.h>

#define NX_SHA256_DIGEST_SIZE 32u

struct nx_sha256 {
    uint32_t h[8];
    uint64_t total_len;
    uint8_t block[64];
    uint32_t block_len;
};

void nx_sha256_init(struct nx_sha256 *ctx);
void nx_sha256_update(struct nx_sha256 *ctx, const void *data, uint64_t len);
void nx_sha256_final(struct nx_sha256 *ctx, uint8_t out[NX_SHA256_DIGEST_SIZE]);
void nx_sha256(const void *data, uint64_t len, uint8_t out[NX_SHA256_DIGEST_SIZE]);

#endif
