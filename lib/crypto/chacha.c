/* ChaCha20, Poly1305 and the ChaCha20-Poly1305 AEAD (RFC 8439); see
 * nanox/aead.h.  Poly1305 uses 26-bit limbs (after poly1305-donna). */
#include <nanox/aead.h>
#include <nanox/hash.h>
#include <nanox/string.h>

static inline uint32_t rotl(uint32_t x, int n)
{
    return x << n | x >> (32 - n);
}

static inline uint32_t le32(const uint8_t *p)
{
    return (uint32_t)p[0] | (uint32_t)p[1] << 8 | (uint32_t)p[2] << 16 | (uint32_t)p[3] << 24;
}

static inline void put_le32(uint8_t *p, uint32_t v)
{
    p[0] = (uint8_t)v;
    p[1] = (uint8_t)(v >> 8);
    p[2] = (uint8_t)(v >> 16);
    p[3] = (uint8_t)(v >> 24);
}

#define QR(a, b, c, d)                                                                     \
    do {                                                                                   \
        a += b;                                                                            \
        d = rotl(d ^ a, 16);                                                               \
        c += d;                                                                            \
        b = rotl(b ^ c, 12);                                                               \
        a += b;                                                                            \
        d = rotl(d ^ a, 8);                                                                \
        c += d;                                                                            \
        b = rotl(b ^ c, 7);                                                                \
    } while (0)

void nx_chacha20_block(const uint8_t key[32], uint32_t counter, const uint8_t nonce[12],
                       uint8_t out[64])
{
    uint32_t s[16], x[16];
    s[0] = 0x61707865u;
    s[1] = 0x3320646eu;
    s[2] = 0x79622d32u;
    s[3] = 0x6b206574u;
    for (int i = 0; i < 8; i++)
        s[4 + i] = le32(key + 4 * i);
    s[12] = counter;
    for (int i = 0; i < 3; i++)
        s[13 + i] = le32(nonce + 4 * i);
    memcpy(x, s, sizeof(s));
    for (int i = 0; i < 10; i++) {
        QR(x[0], x[4], x[8], x[12]);
        QR(x[1], x[5], x[9], x[13]);
        QR(x[2], x[6], x[10], x[14]);
        QR(x[3], x[7], x[11], x[15]);
        QR(x[0], x[5], x[10], x[15]);
        QR(x[1], x[6], x[11], x[12]);
        QR(x[2], x[7], x[8], x[13]);
        QR(x[3], x[4], x[9], x[14]);
    }
    for (int i = 0; i < 16; i++)
        put_le32(out + 4 * i, x[i] + s[i]);
    nx_wipe(x, sizeof(x));
}

static void chacha20_xor(const uint8_t key[32], uint32_t counter, const uint8_t nonce[12],
                         const uint8_t *in, uint32_t len, uint8_t *out)
{
    uint8_t ks[64];
    while (len) {
        nx_chacha20_block(key, counter++, nonce, ks);
        uint32_t n = len < 64 ? len : 64;
        for (uint32_t i = 0; i < n; i++)
            out[i] = in[i] ^ ks[i];
        in += n;
        out += n;
        len -= n;
    }
    nx_wipe(ks, sizeof(ks));
}

struct poly {
    uint32_t r[5], h[5], pad[4];
    uint8_t buf[16];
    uint32_t buf_len;
};

static void poly_init(struct poly *p, const uint8_t key[32])
{
    p->r[0] = le32(key) & 0x3ffffff;
    p->r[1] = (le32(key + 3) >> 2) & 0x3ffff03;
    p->r[2] = (le32(key + 6) >> 4) & 0x3ffc0ff;
    p->r[3] = (le32(key + 9) >> 6) & 0x3f03fff;
    p->r[4] = (le32(key + 12) >> 8) & 0x00fffff;
    for (int i = 0; i < 5; i++)
        p->h[i] = 0;
    for (int i = 0; i < 4; i++)
        p->pad[i] = le32(key + 16 + 4 * i);
    p->buf_len = 0;
}

static void poly_block(struct poly *p, const uint8_t m[16], uint32_t hibit)
{
    uint32_t r0 = p->r[0], r1 = p->r[1], r2 = p->r[2], r3 = p->r[3], r4 = p->r[4];
    uint32_t s1 = r1 * 5, s2 = r2 * 5, s3 = r3 * 5, s4 = r4 * 5;
    uint32_t h0 = p->h[0], h1 = p->h[1], h2 = p->h[2], h3 = p->h[3], h4 = p->h[4];
    h0 += le32(m) & 0x3ffffff;
    h1 += (le32(m + 3) >> 2) & 0x3ffffff;
    h2 += (le32(m + 6) >> 4) & 0x3ffffff;
    h3 += (le32(m + 9) >> 6) & 0x3ffffff;
    h4 += (le32(m + 12) >> 8) | hibit;
    uint64_t d0 = (uint64_t)h0 * r0 + (uint64_t)h1 * s4 + (uint64_t)h2 * s3 +
                  (uint64_t)h3 * s2 + (uint64_t)h4 * s1;
    uint64_t d1 = (uint64_t)h0 * r1 + (uint64_t)h1 * r0 + (uint64_t)h2 * s4 +
                  (uint64_t)h3 * s3 + (uint64_t)h4 * s2;
    uint64_t d2 = (uint64_t)h0 * r2 + (uint64_t)h1 * r1 + (uint64_t)h2 * r0 +
                  (uint64_t)h3 * s4 + (uint64_t)h4 * s3;
    uint64_t d3 = (uint64_t)h0 * r3 + (uint64_t)h1 * r2 + (uint64_t)h2 * r1 +
                  (uint64_t)h3 * r0 + (uint64_t)h4 * s4;
    uint64_t d4 = (uint64_t)h0 * r4 + (uint64_t)h1 * r3 + (uint64_t)h2 * r2 +
                  (uint64_t)h3 * r1 + (uint64_t)h4 * r0;
    uint32_t c = (uint32_t)(d0 >> 26);
    h0 = (uint32_t)d0 & 0x3ffffff;
    d1 += c;
    c = (uint32_t)(d1 >> 26);
    h1 = (uint32_t)d1 & 0x3ffffff;
    d2 += c;
    c = (uint32_t)(d2 >> 26);
    h2 = (uint32_t)d2 & 0x3ffffff;
    d3 += c;
    c = (uint32_t)(d3 >> 26);
    h3 = (uint32_t)d3 & 0x3ffffff;
    d4 += c;
    c = (uint32_t)(d4 >> 26);
    h4 = (uint32_t)d4 & 0x3ffffff;
    h0 += c * 5;
    c = h0 >> 26;
    h0 &= 0x3ffffff;
    h1 += c;
    p->h[0] = h0;
    p->h[1] = h1;
    p->h[2] = h2;
    p->h[3] = h3;
    p->h[4] = h4;
}

static void poly_update(struct poly *p, const uint8_t *m, uint32_t len)
{
    while (len) {
        uint32_t n = 16 - p->buf_len;
        if (n > len)
            n = len;
        memcpy(p->buf + p->buf_len, m, n);
        p->buf_len += n;
        m += n;
        len -= n;
        if (p->buf_len == 16) {
            poly_block(p, p->buf, 1u << 24);
            p->buf_len = 0;
        }
    }
}

/* AEAD padding: zeros up to a multiple of 16 (a partial block is
 * processed as a full padded block). */
static void poly_pad(struct poly *p)
{
    if (p->buf_len) {
        memset(p->buf + p->buf_len, 0, 16 - p->buf_len);
        poly_block(p, p->buf, 1u << 24);
        p->buf_len = 0;
    }
}

static void poly_final(struct poly *p, uint8_t tag[16])
{
    if (p->buf_len) { /* last partial block: 1 byte, then zeros, no high bit */
        p->buf[p->buf_len] = 1;
        memset(p->buf + p->buf_len + 1, 0, 15 - p->buf_len);
        poly_block(p, p->buf, 0);
    }
    uint32_t h0 = p->h[0], h1 = p->h[1], h2 = p->h[2], h3 = p->h[3], h4 = p->h[4], c;
    c = h1 >> 26;
    h1 &= 0x3ffffff;
    h2 += c;
    c = h2 >> 26;
    h2 &= 0x3ffffff;
    h3 += c;
    c = h3 >> 26;
    h3 &= 0x3ffffff;
    h4 += c;
    c = h4 >> 26;
    h4 &= 0x3ffffff;
    h0 += c * 5;
    c = h0 >> 26;
    h0 &= 0x3ffffff;
    h1 += c;
    /* g = h - p */
    uint32_t g0 = h0 + 5;
    c = g0 >> 26;
    g0 &= 0x3ffffff;
    uint32_t g1 = h1 + c;
    c = g1 >> 26;
    g1 &= 0x3ffffff;
    uint32_t g2 = h2 + c;
    c = g2 >> 26;
    g2 &= 0x3ffffff;
    uint32_t g3 = h3 + c;
    c = g3 >> 26;
    g3 &= 0x3ffffff;
    uint32_t g4 = h4 + c - (1u << 26);
    uint32_t mask = (g4 >> 31) - 1; /* all ones if h >= p */
    g0 &= mask;
    g1 &= mask;
    g2 &= mask;
    g3 &= mask;
    g4 &= mask;
    mask = ~mask;
    h0 = (h0 & mask) | g0;
    h1 = (h1 & mask) | g1;
    h2 = (h2 & mask) | g2;
    h3 = (h3 & mask) | g3;
    h4 = (h4 & mask) | g4;
    h0 = (h0 | (h1 << 26)) & 0xffffffff;
    h1 = ((h1 >> 6) | (h2 << 20)) & 0xffffffff;
    h2 = ((h2 >> 12) | (h3 << 14)) & 0xffffffff;
    h3 = ((h3 >> 18) | (h4 << 8)) & 0xffffffff;
    uint64_t f = (uint64_t)h0 + p->pad[0];
    h0 = (uint32_t)f;
    f = (uint64_t)h1 + p->pad[1] + (f >> 32);
    h1 = (uint32_t)f;
    f = (uint64_t)h2 + p->pad[2] + (f >> 32);
    h2 = (uint32_t)f;
    f = (uint64_t)h3 + p->pad[3] + (f >> 32);
    h3 = (uint32_t)f;
    put_le32(tag, h0);
    put_le32(tag + 4, h1);
    put_le32(tag + 8, h2);
    put_le32(tag + 12, h3);
}

void nx_poly1305(const uint8_t key[32], const uint8_t *msg, uint32_t len, uint8_t tag[16])
{
    struct poly p;
    poly_init(&p, key);
    poly_update(&p, msg, len);
    poly_final(&p, tag);
    nx_wipe(&p, sizeof(p));
}

static void aead_tag(const uint8_t key[32], const uint8_t nonce[12], const uint8_t *aad,
                     uint32_t aad_len, const uint8_t *ct, uint32_t len, uint8_t tag[16])
{
    uint8_t block[64], lens[16];
    nx_chacha20_block(key, 0, nonce, block);
    struct poly p;
    poly_init(&p, block);
    poly_update(&p, aad, aad_len);
    poly_pad(&p);
    poly_update(&p, ct, len);
    poly_pad(&p);
    for (int i = 0; i < 8; i++) {
        lens[i] = (uint8_t)((uint64_t)aad_len >> (8 * i));
        lens[8 + i] = (uint8_t)((uint64_t)len >> (8 * i));
    }
    poly_update(&p, lens, 16);
    poly_final(&p, tag);
    nx_wipe(block, sizeof(block));
    nx_wipe(&p, sizeof(p));
}

void nx_chacha_seal(const uint8_t key[32], const uint8_t nonce[12], const uint8_t *aad,
                    uint32_t aad_len, const uint8_t *pt, uint32_t len, uint8_t *ct,
                    uint8_t tag[16])
{
    chacha20_xor(key, 1, nonce, pt, len, ct);
    aead_tag(key, nonce, aad, aad_len, ct, len, tag);
}

int nx_chacha_open(const uint8_t key[32], const uint8_t nonce[12], const uint8_t *aad,
                   uint32_t aad_len, const uint8_t *ct, uint32_t len, const uint8_t tag[16],
                   uint8_t *pt)
{
    uint8_t want[16];
    aead_tag(key, nonce, aad, aad_len, ct, len, want);
    if (!nx_ct_equal(want, tag, 16)) {
        memset(pt, 0, len);
        return -1;
    }
    chacha20_xor(key, 1, nonce, ct, len, pt);
    return 0;
}
