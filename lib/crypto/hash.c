/* Hash interface, HMAC, HKDF; see nanox/hash.h. */
#include <nanox/hash.h>
#include <nanox/string.h>

uint32_t nx_hash_len(int id)
{
    switch (id) {
    case NX_HASH_SHA256: return 32;
    case NX_HASH_SHA384: return 48;
    case NX_HASH_SHA512: return 64;
    default: return 0;
    }
}

void nx_hash_init(struct nx_hash *h, int id)
{
    h->id = id;
    h->len = nx_hash_len(id);
    if (id == NX_HASH_SHA256) {
        h->block = 64;
        nx_sha256_init(&h->u.s256);
    } else {
        h->block = 128;
        if (id == NX_HASH_SHA384)
            nx_sha384_init(&h->u.s512);
        else
            nx_sha512_init(&h->u.s512);
    }
}

void nx_hash_update(struct nx_hash *h, const void *data, uint64_t len)
{
    if (h->id == NX_HASH_SHA256)
        nx_sha256_update(&h->u.s256, data, len);
    else
        nx_sha512_update(&h->u.s512, data, len);
}

void nx_hash_final(struct nx_hash *h, uint8_t *out)
{
    if (h->id == NX_HASH_SHA256)
        nx_sha256_final(&h->u.s256, out);
    else
        nx_sha512_final(&h->u.s512, out);
}

void nx_hash(int id, const void *data, uint64_t len, uint8_t *out)
{
    struct nx_hash h;
    nx_hash_init(&h, id);
    nx_hash_update(&h, data, len);
    nx_hash_final(&h, out);
}

void nx_hmac_init(struct nx_hmac *m, int id, const void *key, uint32_t key_len)
{
    uint8_t k[NX_HASH_BLOCK_MAX], pad[NX_HASH_BLOCK_MAX];
    uint32_t block = id == NX_HASH_SHA256 ? 64u : 128u;
    memset(k, 0, sizeof(k));
    if (key_len > block)
        nx_hash(id, key, key_len, k);
    else if (key_len)
        memcpy(k, key, key_len);
    for (uint32_t i = 0; i < block; i++)
        pad[i] = k[i] ^ 0x36u;
    nx_hash_init(&m->inner, id);
    nx_hash_update(&m->inner, pad, block);
    for (uint32_t i = 0; i < block; i++)
        pad[i] = k[i] ^ 0x5Cu;
    nx_hash_init(&m->outer, id);
    nx_hash_update(&m->outer, pad, block);
    nx_wipe(k, sizeof(k));
    nx_wipe(pad, sizeof(pad));
}

void nx_hmac_update(struct nx_hmac *m, const void *data, uint64_t len)
{
    nx_hash_update(&m->inner, data, len);
}

void nx_hmac_final(struct nx_hmac *m, uint8_t *out)
{
    uint8_t in[NX_HASH_MAX];
    nx_hash_final(&m->inner, in);
    nx_hash_update(&m->outer, in, m->inner.len);
    nx_hash_final(&m->outer, out);
    nx_wipe(in, sizeof(in));
}

void nx_hmac(int id, const void *key, uint32_t key_len, const void *data, uint64_t len,
             uint8_t *out)
{
    struct nx_hmac m;
    nx_hmac_init(&m, id, key, key_len);
    nx_hmac_update(&m, data, len);
    nx_hmac_final(&m, out);
}

void nx_hkdf_extract(int id, const void *salt, uint32_t salt_len, const void *ikm,
                     uint32_t ikm_len, uint8_t *prk)
{
    static const uint8_t zeros[NX_HASH_MAX];
    if (!salt || !salt_len) {
        salt = zeros;
        salt_len = nx_hash_len(id);
    }
    nx_hmac(id, salt, salt_len, ikm, ikm_len, prk);
}

void nx_hkdf_expand(int id, const uint8_t *prk, const void *info, uint32_t info_len,
                    uint8_t *out, uint32_t len)
{
    uint32_t hl = nx_hash_len(id), done = 0;
    uint8_t t[NX_HASH_MAX];
    uint8_t ctr = 1;
    while (done < len) {
        struct nx_hmac m;
        nx_hmac_init(&m, id, prk, hl);
        if (ctr > 1)
            nx_hmac_update(&m, t, hl);
        nx_hmac_update(&m, info, info_len);
        nx_hmac_update(&m, &ctr, 1);
        nx_hmac_final(&m, t);
        uint32_t n = len - done < hl ? len - done : hl;
        memcpy(out + done, t, n);
        done += n;
        ctr++;
    }
    nx_wipe(t, sizeof(t));
}

int nx_ct_equal(const void *a, const void *b, uint32_t len)
{
    const volatile uint8_t *x = a, *y = b;
    uint8_t d = 0;
    for (uint32_t i = 0; i < len; i++)
        d |= x[i] ^ y[i];
    return d == 0;
}

void nx_wipe(void *p, uint32_t len)
{
    volatile uint8_t *v = p;
    for (uint32_t i = 0; i < len; i++)
        v[i] = 0;
}
