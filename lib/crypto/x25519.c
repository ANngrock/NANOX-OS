/*
 * X25519 (RFC 7748) for the TLS 1.3 key exchange.  Field arithmetic modulo
 * 2^255 - 19 in 16 limbs of 16 bits (the representation of TweetNaCl),
 * Montgomery ladder with constant-time conditional swaps.
 */
#include <nanox/ecc.h>
#include <nanox/hash.h>
#include <nanox/string.h>

typedef int64_t gf[16];

static const gf K121665 = {0xDB41, 1};

static void car(gf o)
{
    for (int i = 0; i < 16; i++) {
        o[i] += (int64_t)1 << 16;
        int64_t c = o[i] >> 16;
        o[(i + 1) * (i < 15)] += c - 1 + 37 * (c - 1) * (i == 15);
        o[i] -= c * 65536; /* c << 16 without shifting a negative value */
    }
}

static void sel(gf p, gf q, int64_t b)
{
    int64_t c = ~(b - 1);
    for (int i = 0; i < 16; i++) {
        int64_t t = c & (p[i] ^ q[i]);
        p[i] ^= t;
        q[i] ^= t;
    }
}

static void pack(uint8_t o[32], const gf n)
{
    gf m, t;
    for (int i = 0; i < 16; i++)
        t[i] = n[i];
    car(t);
    car(t);
    car(t);
    for (int j = 0; j < 2; j++) {
        m[0] = t[0] - 0xffed;
        for (int i = 1; i < 15; i++) {
            m[i] = t[i] - 0xffff - ((m[i - 1] >> 16) & 1);
            m[i - 1] &= 0xffff;
        }
        m[15] = t[15] - 0x7fff - ((m[14] >> 16) & 1);
        int64_t b = (m[15] >> 16) & 1;
        m[14] &= 0xffff;
        sel(t, m, 1 - b);
    }
    for (int i = 0; i < 16; i++) {
        o[2 * i] = (uint8_t)(t[i] & 0xff);
        o[2 * i + 1] = (uint8_t)(t[i] >> 8);
    }
}

static void unpack(gf o, const uint8_t n[32])
{
    for (int i = 0; i < 16; i++)
        o[i] = n[2 * i] + ((int64_t)n[2 * i + 1] << 8);
    o[15] &= 0x7fff;
}

static void add(gf o, const gf a, const gf b)
{
    for (int i = 0; i < 16; i++)
        o[i] = a[i] + b[i];
}

static void sub(gf o, const gf a, const gf b)
{
    for (int i = 0; i < 16; i++)
        o[i] = a[i] - b[i];
}

static void mul(gf o, const gf a, const gf b)
{
    int64_t t[31];
    for (int i = 0; i < 31; i++)
        t[i] = 0;
    for (int i = 0; i < 16; i++)
        for (int j = 0; j < 16; j++)
            t[i + j] += a[i] * b[j];
    for (int i = 0; i < 15; i++)
        t[i] += 38 * t[i + 16];
    for (int i = 0; i < 16; i++)
        o[i] = t[i];
    car(o);
    car(o);
}

static void sq(gf o, const gf a)
{
    mul(o, a, a);
}

static void inv(gf o, const gf i)
{
    gf c;
    for (int a = 0; a < 16; a++)
        c[a] = i[a];
    for (int a = 253; a >= 0; a--) {
        sq(c, c);
        if (a != 2 && a != 4)
            mul(c, c, i);
    }
    for (int a = 0; a < 16; a++)
        o[a] = c[a];
}

void nx_x25519(uint8_t out[32], const uint8_t scalar[32], const uint8_t point[32])
{
    uint8_t z[32];
    gf x, a, b, c, d, e, f;
    memcpy(z, scalar, 32);
    z[31] = (uint8_t)((z[31] & 127) | 64);
    z[0] &= 248;
    unpack(x, point);
    for (int i = 0; i < 16; i++) {
        b[i] = x[i];
        d[i] = a[i] = c[i] = 0;
    }
    a[0] = d[0] = 1;
    for (int i = 254; i >= 0; --i) {
        int64_t r = (z[i >> 3] >> (i & 7)) & 1;
        sel(a, b, r);
        sel(c, d, r);
        add(e, a, c);
        sub(a, a, c);
        add(c, b, d);
        sub(b, b, d);
        sq(d, e);
        sq(f, a);
        mul(a, c, a);
        mul(c, b, e);
        add(e, a, c);
        sub(a, a, c);
        sq(b, a);
        sub(c, d, f);
        mul(a, c, K121665);
        add(a, a, d);
        mul(c, c, a);
        mul(a, d, f);
        mul(d, b, x);
        sq(b, e);
        sel(a, b, r);
        sel(c, d, r);
    }
    inv(c, c);
    mul(a, a, c);
    pack(out, a);
    nx_wipe(z, sizeof(z));
}

void nx_x25519_base(uint8_t out[32], const uint8_t scalar[32])
{
    static const uint8_t nine[32] = {9};
    nx_x25519(out, scalar, nine);
}
