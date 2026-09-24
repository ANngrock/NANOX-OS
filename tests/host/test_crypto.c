/*
 * Host tests of the M5 crypto code (lib/crypto, lib/tls/x509.c helpers)
 * against known answers from independent implementations
 * (crypto_vectors.h, generated with hashlib/hmac and OpenSSL through
 * tests/host/gen_crypto_vectors.py), plus the negative cases: a flipped
 * bit in a tag, signature, key or message must be rejected.
 */
#include <stdint.h>
#include <string.h>

#include <nanox/aead.h>
#include <nanox/drbg.h>
#include <nanox/ecc.h>
#include <nanox/hash.h>
#include <nanox/tls.h>
#include <nanox/x509.h>

#include "crypto_vectors.h"
#include "test.h"

#define EQ(a, b, n) (memcmp((a), (b), (n)) == 0)

static void test_hashes(void)
{
    static const uint8_t *msgs[] = {hash_msg0, hash_msg1, hash_msg2, hash_msg3, hash_msg4};
    static const uint32_t lens[] = {HASH_MSG0_LEN, HASH_MSG1_LEN, HASH_MSG2_LEN, HASH_MSG3_LEN,
                                    HASH_MSG4_LEN};
    static const uint8_t *s256[] = {hash_msg0_sha256, hash_msg1_sha256, hash_msg2_sha256,
                                    hash_msg3_sha256, hash_msg4_sha256};
    static const uint8_t *s384[] = {hash_msg0_sha384, hash_msg1_sha384, hash_msg2_sha384,
                                    hash_msg3_sha384, hash_msg4_sha384};
    static const uint8_t *s512[] = {hash_msg0_sha512, hash_msg1_sha512, hash_msg2_sha512,
                                    hash_msg3_sha512, hash_msg4_sha512};
    for (int i = 0; i < HASH_VECTORS; i++) {
        uint8_t d[64];
        nx_hash(NX_HASH_SHA256, msgs[i], lens[i], d);
        CHECK(EQ(d, s256[i], 32));
        nx_hash(NX_HASH_SHA384, msgs[i], lens[i], d);
        CHECK(EQ(d, s384[i], 48));
        nx_hash(NX_HASH_SHA512, msgs[i], lens[i], d);
        CHECK(EQ(d, s512[i], 64));
        /* the same in pieces of 1, 7 and the rest */
        struct nx_hash h;
        nx_hash_init(&h, NX_HASH_SHA384);
        uint32_t a = lens[i] > 1 ? 1 : lens[i], b = lens[i] > 8 ? 7 : 0;
        nx_hash_update(&h, msgs[i], a);
        nx_hash_update(&h, msgs[i] + a, b);
        nx_hash_update(&h, msgs[i] + a + b, lens[i] - a - b);
        nx_hash_final(&h, d);
        CHECK(EQ(d, s384[i], 48));
    }
    CHECK_EQ_INT(nx_hash_len(NX_HASH_SHA256), 32);
    CHECK_EQ_INT(nx_hash_len(NX_HASH_SHA384), 48);
    CHECK_EQ_INT(nx_hash_len(99), 0);
}

static void test_hmac_hkdf(void)
{
    uint8_t d[64];
    nx_hmac(NX_HASH_SHA256, hmac0_key, HMAC0_KEY_LEN, hmac0_msg, HMAC0_MSG_LEN, d);
    CHECK(EQ(d, hmac0_sha256, 32));
    nx_hmac(NX_HASH_SHA384, hmac0_key, HMAC0_KEY_LEN, hmac0_msg, HMAC0_MSG_LEN, d);
    CHECK(EQ(d, hmac0_sha384, 48));
    nx_hmac(NX_HASH_SHA256, hmac1_key, HMAC1_KEY_LEN, hmac1_msg, HMAC1_MSG_LEN, d); /* long key */
    CHECK(EQ(d, hmac1_sha256, 32));
    nx_hmac(NX_HASH_SHA384, hmac1_key, HMAC1_KEY_LEN, hmac1_msg, HMAC1_MSG_LEN, d);
    CHECK(EQ(d, hmac1_sha384, 48));
    uint8_t prk[32], okm[HKDF_OKM_LEN];
    nx_hkdf_extract(NX_HASH_SHA256, hkdf_salt, HKDF_SALT_LEN, hkdf_ikm, HKDF_IKM_LEN, prk);
    CHECK(EQ(prk, hkdf_prk, 32));
    nx_hkdf_expand(NX_HASH_SHA256, prk, hkdf_info, HKDF_INFO_LEN, okm, HKDF_OKM_LEN);
    CHECK(EQ(okm, hkdf_okm, HKDF_OKM_LEN));
    uint8_t x[4] = {1, 2, 3, 4}, y[4] = {1, 2, 3, 5};
    CHECK(nx_ct_equal(x, x, 4) && !nx_ct_equal(x, y, 4));
    /* TLS 1.3 HKDF-Expand-Label: the "derived" secret of the empty early
     * secret is a constant of every TLS 1.3 connection (RFC 8448 §3) */
    static const uint8_t early_derived[32] = {
        0x6f, 0x26, 0x15, 0xa1, 0x08, 0xc7, 0x02, 0xc5, 0x67, 0x8f, 0x54, 0xfc, 0x9d, 0xba,
        0xb6, 0x97, 0x16, 0xc0, 0x76, 0x18, 0x9c, 0x48, 0x25, 0x0c, 0xeb, 0xea, 0xc3, 0x57,
        0x6c, 0x36, 0x11, 0xba};
    uint8_t zeros[32] = {0}, early[32], empty[32], der[32];
    nx_hkdf_extract(NX_HASH_SHA256, 0, 0, zeros, 32, early);
    nx_hash(NX_HASH_SHA256, "", 0, empty);
    tls_expand_label(early, "derived", empty, 32, der, 32);
    CHECK(EQ(der, early_derived, 32));
}

static void test_drbg(void)
{
    struct nx_drbg d;
    uint8_t out[64];
    nx_drbg_instantiate(&d, drbg_entropy, DRBG_ENTROPY_LEN, drbg_pers, DRBG_PERS_LEN);
    CHECK_EQ_INT(nx_drbg_generate(&d, out, 40, 0, 0), 0);
    CHECK(EQ(out, drbg_out1, 40));
    CHECK_EQ_INT(nx_drbg_generate(&d, out, 64, (const uint8_t *)"addl", 4), 0);
    CHECK(EQ(out, drbg_out2, 64));
    nx_drbg_reseed(&d, drbg_entropy2, DRBG_ENTROPY2_LEN, 0, 0);
    CHECK_EQ_INT(nx_drbg_generate(&d, out, 32, 0, 0), 0);
    CHECK(EQ(out, drbg_out3, 32));
    /* reseed interval */
    for (uint32_t i = 0; i < NX_DRBG_RESEED_INTERVAL; i++)
        nx_drbg_generate(&d, out, 1, 0, 0);
    CHECK_EQ_INT(nx_drbg_generate(&d, out, 1, 0, 0), 1);
    struct nx_drbg none;
    memset(&none, 0, sizeof(none));
    CHECK_EQ_INT(nx_drbg_generate(&none, out, 1, 0, 0), -1);
    /* health tests */
    uint8_t buf[512];
    for (uint32_t i = 0; i < sizeof(buf); i++)
        buf[i] = (uint8_t)(i * 167u + (i >> 3) * 13u);
    CHECK_EQ_INT(nx_entropy_health(drbg_entropy, DRBG_ENTROPY_LEN), 0);
    CHECK_EQ_INT(nx_entropy_health(buf, sizeof(buf)), 0);
    memset(buf + 100, 0x5A, 16); /* a run of 16 */
    CHECK_EQ_INT(nx_entropy_health(buf, sizeof(buf)), -1);
    memset(buf, 0, sizeof(buf)); /* stuck source */
    CHECK_EQ_INT(nx_entropy_health(buf, 48), -1);
    for (uint32_t i = 0; i < sizeof(buf); i++) /* biased: one value in every 7th byte */
        buf[i] = i % 7 == 0 ? 0xAA : (uint8_t)(i * 167u);
    CHECK_EQ_INT(nx_entropy_health(buf, sizeof(buf)), -1);
    CHECK_EQ_INT(nx_entropy_health(buf, 0), -1);
}

#define AEAD_ROW(name, i)                                                                  \
    {name##i##_key, name##i##_nonce, name##i##_aad, name##i##_pt, name##i##_ct,            \
     name##i##_AAD_LEN_, name##i##_PT_LEN_}

struct aead_vec {
    const uint8_t *key, *nonce, *aad, *pt, *ct;
    uint32_t aad_len, pt_len;
};

#define V(n, i)                                                                            \
    {n##i##_key, n##i##_nonce, n##i##_aad, n##i##_pt, n##i##_ct, 0, 0}

static void aead_lens(struct aead_vec *v, uint32_t aad_len, uint32_t pt_len)
{
    v->aad_len = aad_len;
    v->pt_len = pt_len;
}

static void check_aead(const struct aead_vec *v, int kind)
{
    uint8_t ct[400], tag[16], pt[400];
    uint32_t n = v->pt_len;
    struct nx_gcm g;
    if (kind == 2) {
        nx_chacha_seal(v->key, v->nonce, v->aad, v->aad_len, v->pt, n, ct, tag);
    } else {
        nx_gcm_init(&g, v->key, kind == 0 ? 16 : 32);
        nx_gcm_seal(&g, v->nonce, v->aad, v->aad_len, v->pt, n, ct, tag);
    }
    CHECK(EQ(ct, v->ct, n) && EQ(tag, v->ct + n, 16));
    int r = kind == 2 ? nx_chacha_open(v->key, v->nonce, v->aad, v->aad_len, v->ct, n,
                                       v->ct + n, pt)
                      : nx_gcm_open(&g, v->nonce, v->aad, v->aad_len, v->ct, n, v->ct + n, pt);
    CHECK(r == 0 && EQ(pt, v->pt, n));
    /* a flipped bit anywhere must be refused */
    uint8_t bad[400 + 16];
    memcpy(bad, v->ct, n + 16);
    bad[(n + 16) / 2] ^= 0x04;
    r = kind == 2 ? nx_chacha_open(v->key, v->nonce, v->aad, v->aad_len, bad, n, bad + n, pt)
                  : nx_gcm_open(&g, v->nonce, v->aad, v->aad_len, bad, n, bad + n, pt);
    CHECK_EQ_INT(r, -1);
    if (v->aad_len) {
        uint8_t aad[64];
        memcpy(aad, v->aad, v->aad_len);
        aad[0] ^= 1;
        r = kind == 2 ? nx_chacha_open(v->key, v->nonce, aad, v->aad_len, v->ct, n, v->ct + n,
                                       pt)
                      : nx_gcm_open(&g, v->nonce, aad, v->aad_len, v->ct, n, v->ct + n, pt);
        CHECK_EQ_INT(r, -1);
    }
}

static void test_aead(void)
{
    struct aead_vec gcm128[] = {V(gcm128, 0), V(gcm128, 1), V(gcm128, 2), V(gcm128, 3),
                                V(gcm128, 4), V(gcm128, 5), V(gcm128, 6)};
    struct aead_vec gcm256[] = {V(gcm256, 0), V(gcm256, 1), V(gcm256, 2), V(gcm256, 3),
                                V(gcm256, 4), V(gcm256, 5), V(gcm256, 6)};
    struct aead_vec cha[] = {V(chacha, 0), V(chacha, 1), V(chacha, 2), V(chacha, 3),
                             V(chacha, 4), V(chacha, 5), V(chacha, 6)};
    static const uint32_t al[] = {GCM1280_AAD_LEN, GCM1281_AAD_LEN, GCM1282_AAD_LEN,
                                  GCM1283_AAD_LEN, GCM1284_AAD_LEN, GCM1285_AAD_LEN,
                                  GCM1286_AAD_LEN};
    static const uint32_t pl[] = {GCM1280_PT_LEN, GCM1281_PT_LEN, GCM1282_PT_LEN,
                                  GCM1283_PT_LEN, GCM1284_PT_LEN, GCM1285_PT_LEN,
                                  GCM1286_PT_LEN};
    for (int i = 0; i < AEAD_VECTORS; i++) {
        aead_lens(&gcm128[i], al[i], pl[i]);
        aead_lens(&gcm256[i], al[i], pl[i]);
        aead_lens(&cha[i], al[i], pl[i]);
        check_aead(&gcm128[i], 0);
        check_aead(&gcm256[i], 1);
        check_aead(&cha[i], 2);
    }
}

static void test_x25519(void)
{
    const uint8_t *sc[] = {x25519_0_scalar, x25519_1_scalar, x25519_2_scalar};
    const uint8_t *pt[] = {x25519_0_point, x25519_1_point, x25519_2_point};
    const uint8_t *pub[] = {x25519_0_pub, x25519_1_pub, x25519_2_pub};
    const uint8_t *sh[] = {x25519_0_shared, x25519_1_shared, x25519_2_shared};
    for (int i = 0; i < X25519_VECTORS; i++) {
        uint8_t o[32];
        nx_x25519_base(o, sc[i]);
        CHECK(EQ(o, pub[i], 32));
        nx_x25519(o, sc[i], pt[i]);
        CHECK(EQ(o, sh[i], 32));
    }
}

static void test_rsa(void)
{
    uint8_t d[64], bad[384];
    nx_hash(NX_HASH_SHA256, rsa0_msg, RSA0_MSG_LEN, d);
    CHECK_EQ_INT(nx_rsa_verify_pkcs1(rsa0_n, RSA0_N_LEN, rsa0_e, RSA0_E_LEN, NX_HASH_SHA256, d,
                                     rsa0_pkcs1, RSA0_PKCS1_LEN),
                 0);
    CHECK_EQ_INT(nx_rsa_verify_pss(rsa0_n, RSA0_N_LEN, rsa0_e, RSA0_E_LEN, NX_HASH_SHA256, d,
                                   rsa0_pss, RSA0_PSS_LEN),
                 0);
    /* the PKCS #1 signature is not a PSS signature and vice versa */
    CHECK_EQ_INT(nx_rsa_verify_pss(rsa0_n, RSA0_N_LEN, rsa0_e, RSA0_E_LEN, NX_HASH_SHA256, d,
                                   rsa0_pkcs1, RSA0_PKCS1_LEN),
                 -1);
    CHECK_EQ_INT(nx_rsa_verify_pkcs1(rsa0_n, RSA0_N_LEN, rsa0_e, RSA0_E_LEN, NX_HASH_SHA256, d,
                                     rsa0_pss, RSA0_PSS_LEN),
                 -1);
    memcpy(bad, rsa0_pss, RSA0_PSS_LEN);
    bad[100] ^= 0x10;
    CHECK_EQ_INT(nx_rsa_verify_pss(rsa0_n, RSA0_N_LEN, rsa0_e, RSA0_E_LEN, NX_HASH_SHA256, d, bad,
                                   RSA0_PSS_LEN),
                 -1);
    d[0] ^= 1;
    CHECK_EQ_INT(nx_rsa_verify_pkcs1(rsa0_n, RSA0_N_LEN, rsa0_e, RSA0_E_LEN, NX_HASH_SHA256, d,
                                     rsa0_pkcs1, RSA0_PKCS1_LEN),
                 -1);
    CHECK_EQ_INT(nx_rsa_verify_pkcs1(rsa0_n, RSA0_N_LEN, rsa0_e, RSA0_E_LEN, NX_HASH_SHA256, d,
                                     rsa0_pkcs1, RSA0_PKCS1_LEN - 1),
                 -1); /* wrong length */
    nx_hash(NX_HASH_SHA384, rsa1_msg, RSA1_MSG_LEN, d);
    CHECK_EQ_INT(nx_rsa_verify_pkcs1(rsa1_n, RSA1_N_LEN, rsa1_e, RSA1_E_LEN, NX_HASH_SHA384, d,
                                     rsa1_pkcs1, RSA1_PKCS1_LEN),
                 0);
    CHECK_EQ_INT(nx_rsa_verify_pss(rsa1_n, RSA1_N_LEN, rsa1_e, RSA1_E_LEN, NX_HASH_SHA384, d,
                                   rsa1_pss, RSA1_PSS_LEN),
                 0);
    /* another key */
    CHECK_EQ_INT(nx_rsa_verify_pss(rsa0_n, RSA0_N_LEN, rsa0_e, RSA0_E_LEN, NX_HASH_SHA384, d,
                                   rsa1_pss, RSA1_PSS_LEN),
                 -1);
}

static void test_ecdsa(void)
{
    uint8_t d[64];
    struct {
        int curve, hash;
        const uint8_t *pub, *msg, *der, *r, *s;
        uint32_t pub_len, msg_len, der_len, rl;
    } v[] = {
        {NX_P256, NX_HASH_SHA256, ecdsa0_pub, ecdsa0_msg, ecdsa0_der, ecdsa0_r, ecdsa0_s,
         ECDSA0_PUB_LEN, ECDSA0_MSG_LEN, ECDSA0_DER_LEN, ECDSA0_R_LEN},
        {NX_P384, NX_HASH_SHA384, ecdsa1_pub, ecdsa1_msg, ecdsa1_der, ecdsa1_r, ecdsa1_s,
         ECDSA1_PUB_LEN, ECDSA1_MSG_LEN, ECDSA1_DER_LEN, ECDSA1_R_LEN},
        {NX_P256, NX_HASH_SHA384, ecdsa2_pub, ecdsa2_msg, ecdsa2_der, ecdsa2_r, ecdsa2_s,
         ECDSA2_PUB_LEN, ECDSA2_MSG_LEN, ECDSA2_DER_LEN, ECDSA2_R_LEN},
    };
    for (unsigned i = 0; i < 3; i++) {
        uint32_t hl = nx_hash_len(v[i].hash);
        nx_hash(v[i].hash, v[i].msg, v[i].msg_len, d);
        CHECK_EQ_INT(nx_ecdsa_verify(v[i].curve, v[i].pub, v[i].pub_len, d, hl, v[i].r, v[i].rl,
                                     v[i].s, v[i].rl),
                     0);
        struct nx_slice r, s;
        CHECK(nx_der_ecdsa_sig(v[i].der, v[i].der_len, &r, &s));
        CHECK_EQ_INT(nx_ecdsa_verify(v[i].curve, v[i].pub, v[i].pub_len, d, hl, r.p, r.len, s.p,
                                     s.len),
                     0);
        /* swapped r and s, altered digest, altered key, other curve */
        CHECK_EQ_INT(nx_ecdsa_verify(v[i].curve, v[i].pub, v[i].pub_len, d, hl, v[i].s, v[i].rl,
                                     v[i].r, v[i].rl),
                     -1);
        d[3] ^= 0x80;
        CHECK_EQ_INT(nx_ecdsa_verify(v[i].curve, v[i].pub, v[i].pub_len, d, hl, v[i].r, v[i].rl,
                                     v[i].s, v[i].rl),
                     -1);
        d[3] ^= 0x80;
        uint8_t pub[97];
        memcpy(pub, v[i].pub, v[i].pub_len);
        pub[10] ^= 1; /* no longer on the curve */
        CHECK_EQ_INT(nx_ecdsa_verify(v[i].curve, pub, v[i].pub_len, d, hl, v[i].r, v[i].rl,
                                     v[i].s, v[i].rl),
                     -1);
        CHECK_EQ_INT(nx_ecdsa_verify(v[i].curve == NX_P256 ? NX_P384 : NX_P256, v[i].pub,
                                     v[i].pub_len, d, hl, v[i].r, v[i].rl, v[i].s, v[i].rl),
                     -1);
    }
    static const uint8_t zero[1] = {0};
    nx_hash(NX_HASH_SHA256, ecdsa0_msg, ECDSA0_MSG_LEN, d);
    CHECK_EQ_INT(nx_ecdsa_verify(NX_P256, ecdsa0_pub, ECDSA0_PUB_LEN, d, 32, zero, 1, ecdsa0_s,
                                 ECDSA0_S_LEN),
                 -1); /* r = 0 */
}

void test_crypto(void)
{
    test_hashes();
    test_hmac_hkdf();
    test_drbg();
    test_aead();
    test_x25519();
    test_rsa();
    test_ecdsa();
}
