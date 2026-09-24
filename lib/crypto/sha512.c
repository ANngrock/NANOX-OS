/* SHA-512 / SHA-384 (FIPS 180-4); see nanox/sha512.h.  The constants are
 * the first 64 bits of the fractional parts of the cube roots (K) and
 * square roots (initial values) of the first primes. */
#include <nanox/sha512.h>
#include <nanox/string.h>

static const uint64_t K[80] = {
    0x428a2f98d728ae22ull, 0x7137449123ef65cdull, 0xb5c0fbcfec4d3b2full,
    0xe9b5dba58189dbbcull, 0x3956c25bf348b538ull, 0x59f111f1b605d019ull,
    0x923f82a4af194f9bull, 0xab1c5ed5da6d8118ull, 0xd807aa98a3030242ull,
    0x12835b0145706fbeull, 0x243185be4ee4b28cull, 0x550c7dc3d5ffb4e2ull,
    0x72be5d74f27b896full, 0x80deb1fe3b1696b1ull, 0x9bdc06a725c71235ull,
    0xc19bf174cf692694ull, 0xe49b69c19ef14ad2ull, 0xefbe4786384f25e3ull,
    0x0fc19dc68b8cd5b5ull, 0x240ca1cc77ac9c65ull, 0x2de92c6f592b0275ull,
    0x4a7484aa6ea6e483ull, 0x5cb0a9dcbd41fbd4ull, 0x76f988da831153b5ull,
    0x983e5152ee66dfabull, 0xa831c66d2db43210ull, 0xb00327c898fb213full,
    0xbf597fc7beef0ee4ull, 0xc6e00bf33da88fc2ull, 0xd5a79147930aa725ull,
    0x06ca6351e003826full, 0x142929670a0e6e70ull, 0x27b70a8546d22ffcull,
    0x2e1b21385c26c926ull, 0x4d2c6dfc5ac42aedull, 0x53380d139d95b3dfull,
    0x650a73548baf63deull, 0x766a0abb3c77b2a8ull, 0x81c2c92e47edaee6ull,
    0x92722c851482353bull, 0xa2bfe8a14cf10364ull, 0xa81a664bbc423001ull,
    0xc24b8b70d0f89791ull, 0xc76c51a30654be30ull, 0xd192e819d6ef5218ull,
    0xd69906245565a910ull, 0xf40e35855771202aull, 0x106aa07032bbd1b8ull,
    0x19a4c116b8d2d0c8ull, 0x1e376c085141ab53ull, 0x2748774cdf8eeb99ull,
    0x34b0bcb5e19b48a8ull, 0x391c0cb3c5c95a63ull, 0x4ed8aa4ae3418acbull,
    0x5b9cca4f7763e373ull, 0x682e6ff3d6b2b8a3ull, 0x748f82ee5defb2fcull,
    0x78a5636f43172f60ull, 0x84c87814a1f0ab72ull, 0x8cc702081a6439ecull,
    0x90befffa23631e28ull, 0xa4506cebde82bde9ull, 0xbef9a3f7b2c67915ull,
    0xc67178f2e372532bull, 0xca273eceea26619cull, 0xd186b8c721c0c207ull,
    0xeada7dd6cde0eb1eull, 0xf57d4f7fee6ed178ull, 0x06f067aa72176fbaull,
    0x0a637dc5a2c898a6ull, 0x113f9804bef90daeull, 0x1b710b35131c471bull,
    0x28db77f523047d84ull, 0x32caab7b40c72493ull, 0x3c9ebe0a15c9bebcull,
    0x431d67c49c100d4cull, 0x4cc5d4becb3e42b6ull, 0x597f299cfc657e2aull,
    0x5fcb6fab3ad6faecull, 0x6c44198c4a475817ull,
};

static const uint64_t IV512[8] = {
    0x6a09e667f3bcc908ull, 0xbb67ae8584caa73bull, 0x3c6ef372fe94f82bull,
    0xa54ff53a5f1d36f1ull, 0x510e527fade682d1ull, 0x9b05688c2b3e6c1full,
    0x1f83d9abfb41bd6bull, 0x5be0cd19137e2179ull,
};

static const uint64_t IV384[8] = {
    0xcbbb9d5dc1059ed8ull, 0x629a292a367cd507ull, 0x9159015a3070dd17ull,
    0x152fecd8f70e5939ull, 0x67332667ffc00b31ull, 0x8eb44a8768581511ull,
    0xdb0c2e0d64f98fa7ull, 0x47b5481dbefa4fa4ull,
};

static inline uint64_t rotr(uint64_t x, unsigned n)
{
    return (x >> n) | (x << (64 - n));
}

static void compress(uint64_t h[8], const uint8_t *p)
{
    uint64_t w[80];
    for (unsigned i = 0; i < 16; i++) {
        uint64_t v = 0;
        for (unsigned j = 0; j < 8; j++)
            v = v << 8 | p[8 * i + j];
        w[i] = v;
    }
    for (unsigned i = 16; i < 80; i++) {
        uint64_t s0 = rotr(w[i - 15], 1) ^ rotr(w[i - 15], 8) ^ (w[i - 15] >> 7);
        uint64_t s1 = rotr(w[i - 2], 19) ^ rotr(w[i - 2], 61) ^ (w[i - 2] >> 6);
        w[i] = w[i - 16] + s0 + w[i - 7] + s1;
    }
    uint64_t a = h[0], b = h[1], c = h[2], d = h[3], e = h[4], f = h[5], g = h[6], hh = h[7];
    for (unsigned i = 0; i < 80; i++) {
        uint64_t S1 = rotr(e, 14) ^ rotr(e, 18) ^ rotr(e, 41);
        uint64_t ch = (e & f) ^ (~e & g);
        uint64_t t1 = hh + S1 + ch + K[i] + w[i];
        uint64_t S0 = rotr(a, 28) ^ rotr(a, 34) ^ rotr(a, 39);
        uint64_t maj = (a & b) ^ (a & c) ^ (b & c);
        uint64_t t2 = S0 + maj;
        hh = g;
        g = f;
        f = e;
        e = d + t1;
        d = c;
        c = b;
        b = a;
        a = t1 + t2;
    }
    h[0] += a;
    h[1] += b;
    h[2] += c;
    h[3] += d;
    h[4] += e;
    h[5] += f;
    h[6] += g;
    h[7] += hh;
}

void nx_sha512_init(struct nx_sha512 *ctx)
{
    memcpy(ctx->h, IV512, sizeof(IV512));
    ctx->total_len = 0;
    ctx->block_len = 0;
    ctx->out_len = NX_SHA512_DIGEST_SIZE;
}

void nx_sha384_init(struct nx_sha512 *ctx)
{
    memcpy(ctx->h, IV384, sizeof(IV384));
    ctx->total_len = 0;
    ctx->block_len = 0;
    ctx->out_len = NX_SHA384_DIGEST_SIZE;
}

void nx_sha512_update(struct nx_sha512 *ctx, const void *data, uint64_t len)
{
    const uint8_t *p = data;
    ctx->total_len += len;
    while (len) {
        uint32_t n = 128u - ctx->block_len;
        if (n > len)
            n = (uint32_t)len;
        memcpy(ctx->block + ctx->block_len, p, n);
        ctx->block_len += n;
        p += n;
        len -= n;
        if (ctx->block_len == 128u) {
            compress(ctx->h, ctx->block);
            ctx->block_len = 0;
        }
    }
}

void nx_sha512_final(struct nx_sha512 *ctx, uint8_t *out)
{
    uint64_t bits = ctx->total_len * 8u;
    uint8_t pad = 0x80;
    nx_sha512_update(ctx, &pad, 1);
    uint8_t zero = 0;
    while (ctx->block_len != 112u)
        nx_sha512_update(ctx, &zero, 1);
    uint8_t len[16] = {0};
    for (unsigned i = 0; i < 8; i++)
        len[15 - i] = (uint8_t)(bits >> (8 * i));
    nx_sha512_update(ctx, len, 16);
    for (unsigned i = 0; i < ctx->out_len; i++)
        out[i] = (uint8_t)(ctx->h[i / 8] >> (56 - 8 * (i % 8)));
}

void nx_sha384(const void *data, uint64_t len, uint8_t out[NX_SHA384_DIGEST_SIZE])
{
    struct nx_sha512 c;
    nx_sha384_init(&c);
    nx_sha512_update(&c, data, len);
    nx_sha512_final(&c, out);
}

void nx_sha512(const void *data, uint64_t len, uint8_t out[NX_SHA512_DIGEST_SIZE])
{
    struct nx_sha512 c;
    nx_sha512_init(&c);
    nx_sha512_update(&c, data, len);
    nx_sha512_final(&c, out);
}
