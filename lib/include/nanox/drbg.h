/*
 * HMAC_DRBG with SHA-256 (NIST SP 800-90A Rev. 1 §10.1.2) and simple
 * health tests of the raw entropy input (M5).  The CSPRNG of bin/core: TLS
 * key shares and random values, TCP initial sequence numbers, DNS ids.
 * Freestanding; also compiled for host tests.
 *
 * Health tests (docs/m5-net.md §6), applied to every entropy input before
 * it is used: a repetition count test (no byte value 16 times in a row)
 * and an adaptive proportion test (no byte value more than 64 times in a
 * window of 512 bytes).  They are modelled on SP 800-90B §4.4 with
 * thresholds chosen for a source that claims full entropy per byte; they
 * detect a stuck or grossly biased source, they are not an entropy
 * assessment of it.
 */
#ifndef NANOX_LIB_DRBG_H
#define NANOX_LIB_DRBG_H

#include <stdint.h>

#define NX_DRBG_SEED_LEN 48u         /* entropy bytes per (re)seed: 256 bits + nonce */
#define NX_DRBG_RESEED_INTERVAL 4096u /* generate calls between reseeds (policy) */
#define NX_DRBG_MAX_REQUEST 65536u

struct nx_drbg {
    uint8_t K[32], V[32];
    uint64_t reseed_counter;
    int instantiated;
    uint64_t reseeds, generated;
};

/* 0 if the entropy input passes the health tests, else -1. */
int nx_entropy_health(const uint8_t *buf, uint32_t len);

void nx_drbg_instantiate(struct nx_drbg *d, const uint8_t *entropy, uint32_t elen,
                         const uint8_t *pers, uint32_t plen);
void nx_drbg_reseed(struct nx_drbg *d, const uint8_t *entropy, uint32_t elen,
                    const uint8_t *addl, uint32_t alen);
/* 0 ok; 1 a reseed is required first (nothing generated); -1 bad request
 * or not instantiated. */
int nx_drbg_generate(struct nx_drbg *d, uint8_t *out, uint32_t len, const uint8_t *addl,
                     uint32_t alen);

#endif
