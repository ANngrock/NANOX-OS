/*
 * Symmetric primitives of the TLS 1.3 record layer (M5): AES-128/256 and
 * AES-GCM (NIST SP 800-38D), ChaCha20-Poly1305 (RFC 8439).  Freestanding;
 * also compiled for host tests, which check them against vectors produced
 * by an independent implementation (tests/host/crypto_vectors.h).
 *
 * Side channels: AES uses a byte-indexed S-box table (not constant-time
 * with respect to cache timing); GHASH, ChaCha20 and Poly1305 are written
 * without secret-dependent branches or table indices.  See
 * docs/m5-net.md §6.
 */
#ifndef NANOX_LIB_AEAD_H
#define NANOX_LIB_AEAD_H

#include <stdint.h>

#define NX_AEAD_TAG 16u
#define NX_AEAD_NONCE 12u

struct nx_aes {
    uint32_t rk[60];
    int rounds;
};

/* key_len 16 or 32. */
void nx_aes_init(struct nx_aes *a, const uint8_t *key, uint32_t key_len);
void nx_aes_encrypt(const struct nx_aes *a, const uint8_t in[16], uint8_t out[16]);

struct nx_gcm {
    struct nx_aes aes;
    uint8_t h[16];
};

void nx_gcm_init(struct nx_gcm *g, const uint8_t *key, uint32_t key_len);
/* ct may equal pt. */
void nx_gcm_seal(const struct nx_gcm *g, const uint8_t nonce[12], const uint8_t *aad,
                 uint32_t aad_len, const uint8_t *pt, uint32_t len, uint8_t *ct,
                 uint8_t tag[16]);
/* 0 and the plaintext if the tag verifies, else -1 (pt is then zeroed). */
int nx_gcm_open(const struct nx_gcm *g, const uint8_t nonce[12], const uint8_t *aad,
                uint32_t aad_len, const uint8_t *ct, uint32_t len, const uint8_t tag[16],
                uint8_t *pt);

void nx_chacha20_block(const uint8_t key[32], uint32_t counter, const uint8_t nonce[12],
                       uint8_t out[64]);
void nx_poly1305(const uint8_t key[32], const uint8_t *msg, uint32_t len, uint8_t tag[16]);
void nx_chacha_seal(const uint8_t key[32], const uint8_t nonce[12], const uint8_t *aad,
                    uint32_t aad_len, const uint8_t *pt, uint32_t len, uint8_t *ct,
                    uint8_t tag[16]);
int nx_chacha_open(const uint8_t key[32], const uint8_t nonce[12], const uint8_t *aad,
                   uint32_t aad_len, const uint8_t *ct, uint32_t len, const uint8_t tag[16],
                   uint8_t *pt);

#endif
