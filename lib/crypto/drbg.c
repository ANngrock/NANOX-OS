/* HMAC_DRBG (SHA-256) and entropy health tests; see nanox/drbg.h. */
#include <nanox/drbg.h>
#include <nanox/hash.h>
#include <nanox/string.h>

#define RCT_CUTOFF 16u
#define APT_WINDOW 512u
#define APT_CUTOFF 64u

int nx_entropy_health(const uint8_t *buf, uint32_t len)
{
    if (!len)
        return -1;
    uint32_t run = 1;
    for (uint32_t i = 1; i < len; i++) {
        run = buf[i] == buf[i - 1] ? run + 1 : 1;
        if (run >= RCT_CUTOFF)
            return -1;
    }
    for (uint32_t w = 0; w < len; w += APT_WINDOW) {
        uint32_t end = w + APT_WINDOW < len ? w + APT_WINDOW : len;
        uint32_t count[256];
        memset(count, 0, sizeof(count));
        for (uint32_t i = w; i < end; i++)
            if (++count[buf[i]] > APT_CUTOFF)
                return -1;
    }
    return 0;
}

/* HMAC_DRBG_Update (SP 800-90A §10.1.2.2) with provided_data = a || b || c. */
static void update(struct nx_drbg *d, const uint8_t *a, uint32_t al, const uint8_t *b,
                   uint32_t bl, const uint8_t *c, uint32_t cl)
{
    for (uint8_t round = 0; round < 2; round++) {
        struct nx_hmac m;
        nx_hmac_init(&m, NX_HASH_SHA256, d->K, 32);
        nx_hmac_update(&m, d->V, 32);
        nx_hmac_update(&m, &round, 1);
        if (al)
            nx_hmac_update(&m, a, al);
        if (bl)
            nx_hmac_update(&m, b, bl);
        if (cl)
            nx_hmac_update(&m, c, cl);
        nx_hmac_final(&m, d->K);
        nx_hmac(NX_HASH_SHA256, d->K, 32, d->V, 32, d->V);
        if (!al && !bl && !cl)
            break;
    }
}

void nx_drbg_instantiate(struct nx_drbg *d, const uint8_t *entropy, uint32_t elen,
                         const uint8_t *pers, uint32_t plen)
{
    memset(d->K, 0x00, 32);
    memset(d->V, 0x01, 32);
    update(d, entropy, elen, pers, plen, 0, 0);
    d->reseed_counter = 1;
    d->instantiated = 1;
    d->reseeds = 0;
    d->generated = 0;
}

void nx_drbg_reseed(struct nx_drbg *d, const uint8_t *entropy, uint32_t elen,
                    const uint8_t *addl, uint32_t alen)
{
    update(d, entropy, elen, addl, alen, 0, 0);
    d->reseed_counter = 1;
    d->reseeds++;
}

int nx_drbg_generate(struct nx_drbg *d, uint8_t *out, uint32_t len, const uint8_t *addl,
                     uint32_t alen)
{
    if (!d->instantiated || len > NX_DRBG_MAX_REQUEST)
        return -1;
    if (d->reseed_counter > NX_DRBG_RESEED_INTERVAL)
        return 1;
    if (alen)
        update(d, addl, alen, 0, 0, 0, 0);
    uint32_t done = 0;
    while (done < len) {
        nx_hmac(NX_HASH_SHA256, d->K, 32, d->V, 32, d->V);
        uint32_t n = len - done < 32 ? len - done : 32;
        memcpy(out + done, d->V, n);
        done += n;
    }
    update(d, addl, alen, 0, 0, 0, 0);
    d->reseed_counter++;
    d->generated += len;
    return 0;
}
