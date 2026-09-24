/*
 * Public-key primitives of the M5 TLS client: X25519 key agreement
 * (RFC 7748), signature verification with RSA (PKCS #1 v1.5 and PSS,
 * RFC 8017) and ECDSA over P-256 / P-384 (FIPS 186-4), on a small
 * Montgomery-arithmetic bignum.  Only verification: the client never
 * signs.  Verification works on public data, so the bignum code is not
 * written to be constant-time; X25519, which handles the ephemeral secret,
 * is (docs/m5-net.md §6).  Freestanding; also compiled for host tests.
 */
#ifndef NANOX_LIB_ECC_H
#define NANOX_LIB_ECC_H

#include <stdint.h>

void nx_x25519(uint8_t out[32], const uint8_t scalar[32], const uint8_t point[32]);
void nx_x25519_base(uint8_t out[32], const uint8_t scalar[32]);

/* ---- bignum: little-endian arrays of 32-bit limbs ---- */

#define NX_BN_MAX_BITS 4096u
#define NX_BN_LIMBS (NX_BN_MAX_BITS / 32u + 2u)

struct nx_mont {
    uint32_t n;                /* limbs of the modulus */
    uint32_t m[NX_BN_LIMBS];   /* modulus (odd) */
    uint32_t minv;             /* -m^-1 mod 2^32 */
    uint32_t rr[NX_BN_LIMBS];  /* R^2 mod m, R = 2^(32 n) */
    uint32_t one[NX_BN_LIMBS]; /* R mod m (1 in Montgomery form) */
    uint32_t bits;
};

/* Big-endian bytes -> limbs (n limbs, zero-extended); 0 if it does not fit. */
int nx_bn_from_be(uint32_t *out, uint32_t n, const uint8_t *be, uint32_t len);
/* limbs -> big-endian bytes (len bytes, leading zeros). */
void nx_bn_to_be(uint8_t *be, uint32_t len, const uint32_t *a, uint32_t n);
int nx_bn_cmp(const uint32_t *a, const uint32_t *b, uint32_t n);
int nx_bn_is_zero(const uint32_t *a, uint32_t n);
uint32_t nx_bn_bits(const uint32_t *a, uint32_t n);

/* Modulus from big-endian bytes (odd, at least 2 bits, <= NX_BN_MAX_BITS). */
int nx_mont_init(struct nx_mont *c, const uint8_t *mod_be, uint32_t len);
/* out = a b R^-1 mod m (a, b < m; out may alias a or b). */
void nx_mont_mul(const struct nx_mont *c, uint32_t *out, const uint32_t *a, const uint32_t *b);
void nx_mont_to(const struct nx_mont *c, uint32_t *out, const uint32_t *a);
void nx_mont_from(const struct nx_mont *c, uint32_t *out, const uint32_t *a);
void nx_mod_add(const struct nx_mont *c, uint32_t *out, const uint32_t *a, const uint32_t *b);
void nx_mod_sub(const struct nx_mont *c, uint32_t *out, const uint32_t *a, const uint32_t *b);
/* out = base^exp mod m (normal form in and out; exp big-endian bytes). */
void nx_mod_exp(const struct nx_mont *c, uint32_t *out, const uint32_t *base, const uint8_t *exp,
                uint32_t exp_len);

/* ---- signatures: 0 valid, -1 invalid or unsupported ---- */

/* RSASSA-PKCS1-v1_5 with DigestInfo of hash_id (nanox/hash.h). */
int nx_rsa_verify_pkcs1(const uint8_t *n, uint32_t n_len, const uint8_t *e, uint32_t e_len,
                        int hash_id, const uint8_t *digest, const uint8_t *sig, uint32_t sig_len);
/* RSASSA-PSS, MGF1 with the same hash, salt length = hash length (the
 * only choice TLS 1.3 allows, RFC 8446 §4.2.3). */
int nx_rsa_verify_pss(const uint8_t *n, uint32_t n_len, const uint8_t *e, uint32_t e_len,
                      int hash_id, const uint8_t *digest, const uint8_t *sig, uint32_t sig_len);

enum nx_curve { NX_P256 = 1, NX_P384 = 2 };

/* ECDSA: pub is the uncompressed point 04 || X || Y; r, s big-endian
 * integers (any length up to the curve size); digest of any length (the
 * leftmost bits of the curve order's size are used). */
int nx_ecdsa_verify(int curve, const uint8_t *pub, uint32_t pub_len, const uint8_t *digest,
                    uint32_t digest_len, const uint8_t *r, uint32_t r_len, const uint8_t *s,
                    uint32_t s_len);
/* Field size of a curve in bytes (32, 48) or 0. */
uint32_t nx_curve_bytes(int curve);

#endif
