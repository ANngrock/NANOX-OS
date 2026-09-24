/* RSA signature verification (RFC 8017 §8.1.2, §8.2.2); see nanox/ecc.h. */
#include <nanox/ecc.h>
#include <nanox/hash.h>
#include <nanox/string.h>

#define RSA_MAX_BYTES (NX_BN_MAX_BITS / 8u)

/* s^e mod n as big-endian bytes of the modulus length; 0 on bad input. */
static int rsa_public(const uint8_t *n, uint32_t n_len, const uint8_t *e, uint32_t e_len,
                      const uint8_t *sig, uint32_t sig_len, uint8_t *em, uint32_t *k_out)
{
    static struct nx_mont c;
    while (n_len && !*n) {
        n++;
        n_len--;
    }
    if (n_len < 128 || n_len > RSA_MAX_BYTES || sig_len != n_len || !e_len || e_len > 8)
        return 0; /* at least 1024 bits; the signature has the modulus length */
    if (!nx_mont_init(&c, n, n_len))
        return 0;
    uint32_t s[NX_BN_LIMBS], m[NX_BN_LIMBS];
    if (!nx_bn_from_be(s, c.n, sig, sig_len) || nx_bn_cmp(s, c.m, c.n) >= 0)
        return 0;
    nx_mod_exp(&c, m, s, e, e_len);
    nx_bn_to_be(em, n_len, m, c.n);
    *k_out = n_len;
    return 1;
}

/* DER DigestInfo prefixes (RFC 8017 §9.2 note 1). */
static const uint8_t DI_SHA256[] = {0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01,
                                    0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0x04, 0x20};
static const uint8_t DI_SHA384[] = {0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01,
                                    0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00, 0x04, 0x30};
static const uint8_t DI_SHA512[] = {0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01,
                                    0x65, 0x03, 0x04, 0x02, 0x03, 0x05, 0x00, 0x04, 0x40};

int nx_rsa_verify_pkcs1(const uint8_t *n, uint32_t n_len, const uint8_t *e, uint32_t e_len,
                        int hash_id, const uint8_t *digest, const uint8_t *sig, uint32_t sig_len)
{
    static uint8_t em[RSA_MAX_BYTES], want[RSA_MAX_BYTES];
    const uint8_t *di = hash_id == NX_HASH_SHA256   ? DI_SHA256
                        : hash_id == NX_HASH_SHA384 ? DI_SHA384
                        : hash_id == NX_HASH_SHA512 ? DI_SHA512
                                                    : 0;
    if (!di)
        return -1;
    uint32_t hl = nx_hash_len(hash_id), k;
    if (!rsa_public(n, n_len, e, e_len, sig, sig_len, em, &k))
        return -1;
    uint32_t tlen = (uint32_t)sizeof(DI_SHA256) + hl;
    if (k < tlen + 11)
        return -1;
    /* EM' = 00 01 FF..FF 00 DigestInfo; compared as a whole */
    want[0] = 0x00;
    want[1] = 0x01;
    memset(want + 2, 0xFF, k - tlen - 3);
    want[k - tlen - 1] = 0x00;
    memcpy(want + k - tlen, di, sizeof(DI_SHA256));
    memcpy(want + k - hl, digest, hl);
    return nx_ct_equal(em, want, k) ? 0 : -1;
}

/* MGF1 (RFC 8017 §B.2.1): out ^= MGF1(seed, len). */
static void mgf1_xor(int hash_id, const uint8_t *seed, uint32_t seed_len, uint8_t *out,
                     uint32_t len)
{
    uint32_t hl = nx_hash_len(hash_id), done = 0;
    uint8_t t[NX_HASH_MAX];
    for (uint32_t ctr = 0; done < len; ctr++) {
        uint8_t c[4] = {(uint8_t)(ctr >> 24), (uint8_t)(ctr >> 16), (uint8_t)(ctr >> 8),
                        (uint8_t)ctr};
        struct nx_hash h;
        nx_hash_init(&h, hash_id);
        nx_hash_update(&h, seed, seed_len);
        nx_hash_update(&h, c, 4);
        nx_hash_final(&h, t);
        for (uint32_t i = 0; i < hl && done < len; i++)
            out[done++] ^= t[i];
    }
}

int nx_rsa_verify_pss(const uint8_t *n, uint32_t n_len, const uint8_t *e, uint32_t e_len,
                      int hash_id, const uint8_t *digest, const uint8_t *sig, uint32_t sig_len)
{
    static uint8_t em[RSA_MAX_BYTES];
    uint32_t hl = nx_hash_len(hash_id), k;
    if (!hl || !rsa_public(n, n_len, e, e_len, sig, sig_len, em, &k))
        return -1;
    /* modBits and emLen = ceil((modBits - 1) / 8) */
    const uint8_t *nn = n;
    while (!*nn)
        nn++;
    uint32_t top_bits = 8u - (uint32_t)__builtin_clz((uint32_t)nn[0] << 24 | 0x00FFFFFFu);
    uint32_t mod_bits = (k - 1) * 8u + top_bits;
    uint32_t em_bits = mod_bits - 1, em_len = (em_bits + 7u) / 8u;
    const uint8_t *EM = em + (k - em_len); /* a leading zero byte when em_len < k */
    if (em_len < k && em[0] != 0)
        return -1;
    uint32_t slen = hl;
    if (em_len < hl + slen + 2 || EM[em_len - 1] != 0xBC)
        return -1;
    uint32_t db_len = em_len - hl - 1;
    uint8_t db[RSA_MAX_BYTES];
    memcpy(db, EM, db_len);
    const uint8_t *H = EM + db_len;
    uint32_t zero_bits = 8u * em_len - em_bits;
    if (db[0] & (uint8_t)(0xFFu << (8u - zero_bits)))
        return -1;
    mgf1_xor(hash_id, H, hl, db, db_len);
    db[0] &= (uint8_t)(0xFFu >> zero_bits);
    uint32_t ps = db_len - slen - 1;
    for (uint32_t i = 0; i < ps; i++)
        if (db[i])
            return -1;
    if (db[ps] != 0x01)
        return -1;
    const uint8_t *salt = db + db_len - slen;
    uint8_t mprime[8 + NX_HASH_MAX * 2], h2[NX_HASH_MAX];
    memset(mprime, 0, 8);
    memcpy(mprime + 8, digest, hl);
    memcpy(mprime + 8 + hl, salt, slen);
    nx_hash(hash_id, mprime, 8 + hl + slen, h2);
    return nx_ct_equal(h2, H, hl) ? 0 : -1;
}
