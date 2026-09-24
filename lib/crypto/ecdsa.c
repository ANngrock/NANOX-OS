/*
 * ECDSA signature verification over P-256 and P-384 (FIPS 186-4 §6.4.2);
 * see nanox/ecc.h.  Points in Jacobian coordinates, field elements in
 * Montgomery form (generic CIOS multiplication of bn.c), u1 G + u2 Q by
 * Shamir's trick.  Curve parameters: SEC 2 / FIPS 186-4 D.1.2.3-4 (checked
 * against OpenSSL when they were written down: docs/m5-net.md §6).
 */
#include <nanox/ecc.h>
#include <nanox/string.h>

#define FMAX 12u /* limbs of the largest field (P-384) */

struct curve {
    uint32_t bytes;
    const uint8_t *p, *n, *b, *gx, *gy;
};

static const uint8_t P256_P[] = {
    0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
static const uint8_t P256_N[] = {
    0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xbc, 0xe6, 0xfa, 0xad, 0xa7, 0x17, 0x9e, 0x84, 0xf3, 0xb9, 0xca, 0xc2, 0xfc, 0x63, 0x25, 0x51};
static const uint8_t P256_B[] = {
    0x5a, 0xc6, 0x35, 0xd8, 0xaa, 0x3a, 0x93, 0xe7, 0xb3, 0xeb, 0xbd, 0x55, 0x76, 0x98, 0x86, 0xbc,
    0x65, 0x1d, 0x06, 0xb0, 0xcc, 0x53, 0xb0, 0xf6, 0x3b, 0xce, 0x3c, 0x3e, 0x27, 0xd2, 0x60, 0x4b};
static const uint8_t P256_GX[] = {
    0x6b, 0x17, 0xd1, 0xf2, 0xe1, 0x2c, 0x42, 0x47, 0xf8, 0xbc, 0xe6, 0xe5, 0x63, 0xa4, 0x40, 0xf2,
    0x77, 0x03, 0x7d, 0x81, 0x2d, 0xeb, 0x33, 0xa0, 0xf4, 0xa1, 0x39, 0x45, 0xd8, 0x98, 0xc2, 0x96};
static const uint8_t P256_GY[] = {
    0x4f, 0xe3, 0x42, 0xe2, 0xfe, 0x1a, 0x7f, 0x9b, 0x8e, 0xe7, 0xeb, 0x4a, 0x7c, 0x0f, 0x9e, 0x16,
    0x2b, 0xce, 0x33, 0x57, 0x6b, 0x31, 0x5e, 0xce, 0xcb, 0xb6, 0x40, 0x68, 0x37, 0xbf, 0x51, 0xf5};

static const uint8_t P384_P[] = {
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe,
    0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff};
static const uint8_t P384_N[] = {
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xc7, 0x63, 0x4d, 0x81, 0xf4, 0x37, 0x2d, 0xdf,
    0x58, 0x1a, 0x0d, 0xb2, 0x48, 0xb0, 0xa7, 0x7a, 0xec, 0xec, 0x19, 0x6a, 0xcc, 0xc5, 0x29, 0x73};
static const uint8_t P384_B[] = {
    0xb3, 0x31, 0x2f, 0xa7, 0xe2, 0x3e, 0xe7, 0xe4, 0x98, 0x8e, 0x05, 0x6b, 0xe3, 0xf8, 0x2d, 0x19,
    0x18, 0x1d, 0x9c, 0x6e, 0xfe, 0x81, 0x41, 0x12, 0x03, 0x14, 0x08, 0x8f, 0x50, 0x13, 0x87, 0x5a,
    0xc6, 0x56, 0x39, 0x8d, 0x8a, 0x2e, 0xd1, 0x9d, 0x2a, 0x85, 0xc8, 0xed, 0xd3, 0xec, 0x2a, 0xef};
static const uint8_t P384_GX[] = {
    0xaa, 0x87, 0xca, 0x22, 0xbe, 0x8b, 0x05, 0x37, 0x8e, 0xb1, 0xc7, 0x1e, 0xf3, 0x20, 0xad, 0x74,
    0x6e, 0x1d, 0x3b, 0x62, 0x8b, 0xa7, 0x9b, 0x98, 0x59, 0xf7, 0x41, 0xe0, 0x82, 0x54, 0x2a, 0x38,
    0x55, 0x02, 0xf2, 0x5d, 0xbf, 0x55, 0x29, 0x6c, 0x3a, 0x54, 0x5e, 0x38, 0x72, 0x76, 0x0a, 0xb7};
static const uint8_t P384_GY[] = {
    0x36, 0x17, 0xde, 0x4a, 0x96, 0x26, 0x2c, 0x6f, 0x5d, 0x9e, 0x98, 0xbf, 0x92, 0x92, 0xdc, 0x29,
    0xf8, 0xf4, 0x1d, 0xbd, 0x28, 0x9a, 0x14, 0x7c, 0xe9, 0xda, 0x31, 0x13, 0xb5, 0xf0, 0xb8, 0xc0,
    0x0a, 0x60, 0xb1, 0xce, 0x1d, 0x7e, 0x81, 0x9d, 0x7a, 0x43, 0x1d, 0x7c, 0x90, 0xea, 0x0e, 0x5f};

static const struct curve CURVES[] = {
    {0, 0, 0, 0, 0, 0},
    {32, P256_P, P256_N, P256_B, P256_GX, P256_GY},
    {48, P384_P, P384_N, P384_B, P384_GX, P384_GY},
};

uint32_t nx_curve_bytes(int curve)
{
    return curve == NX_P256 || curve == NX_P384 ? CURVES[curve].bytes : 0;
}

struct pt {
    uint32_t x[FMAX], y[FMAX], z[FMAX]; /* Montgomery form; z = 0: infinity */
};

struct ctx {
    struct nx_mont f; /* field */
    uint32_t b[FMAX], three[FMAX];
    uint32_t n;
};

static void cp(const struct ctx *c, uint32_t *d, const uint32_t *s)
{
    memcpy(d, s, c->n * 4u);
}

static void mul(const struct ctx *c, uint32_t *o, const uint32_t *a, const uint32_t *b)
{
    nx_mont_mul(&c->f, o, a, b);
}

static void add(const struct ctx *c, uint32_t *o, const uint32_t *a, const uint32_t *b)
{
    nx_mod_add(&c->f, o, a, b);
}

static void sub(const struct ctx *c, uint32_t *o, const uint32_t *a, const uint32_t *b)
{
    nx_mod_sub(&c->f, o, a, b);
}

/* Doubling for a = -3 ("dbl-2001-b"). */
static void pt_double(const struct ctx *c, struct pt *r, const struct pt *p)
{
    if (nx_bn_is_zero(p->z, c->n) || nx_bn_is_zero(p->y, c->n)) {
        memset(r, 0, sizeof(*r));
        return;
    }
    uint32_t delta[FMAX], gamma[FMAX], beta[FMAX], alpha[FMAX], t1[FMAX], t2[FMAX];
    mul(c, delta, p->z, p->z);
    mul(c, gamma, p->y, p->y);
    mul(c, beta, p->x, gamma);
    sub(c, t1, p->x, delta);
    add(c, t2, p->x, delta);
    mul(c, alpha, t1, t2);
    mul(c, alpha, alpha, c->three);
    struct pt o;
    /* X3 = alpha^2 - 8 beta */
    mul(c, o.x, alpha, alpha);
    add(c, t1, beta, beta);
    add(c, t1, t1, t1);
    add(c, t2, t1, t1); /* 8 beta */
    sub(c, o.x, o.x, t2);
    /* Z3 = (Y + Z)^2 - gamma - delta */
    add(c, o.z, p->y, p->z);
    mul(c, o.z, o.z, o.z);
    sub(c, o.z, o.z, gamma);
    sub(c, o.z, o.z, delta);
    /* Y3 = alpha (4 beta - X3) - 8 gamma^2 */
    sub(c, t1, t1, o.x); /* t1 = 4 beta */
    mul(c, o.y, alpha, t1);
    mul(c, t2, gamma, gamma);
    add(c, t2, t2, t2);
    add(c, t2, t2, t2);
    add(c, t2, t2, t2);
    sub(c, o.y, o.y, t2);
    *r = o;
}

/* General addition ("add-2007-bl"). */
static void pt_add(const struct ctx *c, struct pt *r, const struct pt *p, const struct pt *q)
{
    if (nx_bn_is_zero(p->z, c->n)) {
        *r = *q;
        return;
    }
    if (nx_bn_is_zero(q->z, c->n)) {
        *r = *p;
        return;
    }
    uint32_t z1z1[FMAX], z2z2[FMAX], u1[FMAX], u2[FMAX], s1[FMAX], s2[FMAX], h[FMAX], i[FMAX],
        j[FMAX], rr[FMAX], v[FMAX], t[FMAX];
    mul(c, z1z1, p->z, p->z);
    mul(c, z2z2, q->z, q->z);
    mul(c, u1, p->x, z2z2);
    mul(c, u2, q->x, z1z1);
    mul(c, s1, p->y, q->z);
    mul(c, s1, s1, z2z2);
    mul(c, s2, q->y, p->z);
    mul(c, s2, s2, z1z1);
    sub(c, h, u2, u1);
    sub(c, rr, s2, s1);
    if (nx_bn_is_zero(h, c->n)) {
        if (nx_bn_is_zero(rr, c->n))
            pt_double(c, r, p);
        else
            memset(r, 0, sizeof(*r));
        return;
    }
    add(c, i, h, h);
    mul(c, i, i, i);
    mul(c, j, h, i);
    add(c, rr, rr, rr);
    mul(c, v, u1, i);
    struct pt o;
    mul(c, o.x, rr, rr);
    sub(c, o.x, o.x, j);
    sub(c, o.x, o.x, v);
    sub(c, o.x, o.x, v);
    sub(c, t, v, o.x);
    mul(c, o.y, rr, t);
    mul(c, t, s1, j);
    add(c, t, t, t);
    sub(c, o.y, o.y, t);
    add(c, o.z, p->z, q->z);
    mul(c, o.z, o.z, o.z);
    sub(c, o.z, o.z, z1z1);
    sub(c, o.z, o.z, z2z2);
    mul(c, o.z, o.z, h);
    *r = o;
}

/* Loads an affine point (big-endian coordinates) and checks that it is on
 * the curve.  0 if not. */
static int load_point(const struct ctx *c, struct pt *p, const uint8_t *x, const uint8_t *y,
                      uint32_t len)
{
    uint32_t ax[FMAX], ay[FMAX];
    if (!nx_bn_from_be(ax, c->n, x, len) || !nx_bn_from_be(ay, c->n, y, len) ||
        nx_bn_cmp(ax, c->f.m, c->n) >= 0 || nx_bn_cmp(ay, c->f.m, c->n) >= 0)
        return 0;
    nx_mont_to(&c->f, p->x, ax);
    nx_mont_to(&c->f, p->y, ay);
    cp(c, p->z, c->f.one);
    /* y^2 = x^3 - 3 x + b */
    uint32_t l[FMAX], r[FMAX], t[FMAX];
    mul(c, l, p->y, p->y);
    mul(c, r, p->x, p->x);
    mul(c, r, r, p->x);
    mul(c, t, p->x, c->three);
    sub(c, r, r, t);
    add(c, r, r, c->b);
    return nx_bn_cmp(l, r, c->n) == 0;
}

/* The leftmost bits of the digest as an integer < 2^bits(order), reduced
 * modulo the order n. */
static void digest_int(const struct nx_mont *order, uint32_t *e, const uint8_t *digest,
                       uint32_t len)
{
    uint32_t bits = order->bits, obytes = (bits + 7u) / 8u;
    uint8_t buf[64];
    uint32_t take = len < obytes ? len : obytes;
    memset(buf, 0, sizeof(buf));
    memcpy(buf + (obytes - take), digest, take);
    nx_bn_from_be(e, order->n, buf, obytes);
    uint32_t excess = take * 8u > bits ? take * 8u - bits : 0; /* shift right */
    for (uint32_t k = 0; k < excess; k++) {
        for (uint32_t i = 0; i < order->n; i++)
            e[i] = e[i] >> 1 | (i + 1 < order->n ? e[i + 1] << 31 : 0);
    }
    uint32_t tmp[FMAX];
    while (nx_bn_cmp(e, order->m, order->n) >= 0) {
        uint64_t borrow = 0;
        for (uint32_t i = 0; i < order->n; i++) {
            uint64_t d = (uint64_t)e[i] - order->m[i] - borrow;
            tmp[i] = (uint32_t)d;
            borrow = (d >> 32) & 1u;
        }
        memcpy(e, tmp, order->n * 4u);
    }
}

int nx_ecdsa_verify(int curve, const uint8_t *pub, uint32_t pub_len, const uint8_t *digest,
                    uint32_t digest_len, const uint8_t *rb, uint32_t r_len, const uint8_t *sb,
                    uint32_t s_len)
{
    if (curve != NX_P256 && curve != NX_P384)
        return -1;
    const struct curve *cv = &CURVES[curve];
    uint32_t L = cv->bytes;
    if (pub_len != 1 + 2 * L || pub[0] != 0x04)
        return -1;
    static struct ctx c;
    static struct nx_mont order;
    if (!nx_mont_init(&c.f, cv->p, L) || !nx_mont_init(&order, cv->n, L))
        return -1;
    c.n = c.f.n;
    uint32_t tmp[FMAX];
    nx_bn_from_be(tmp, c.n, cv->b, L);
    nx_mont_to(&c.f, c.b, tmp);
    memset(tmp, 0, sizeof(tmp));
    tmp[0] = 3;
    nx_mont_to(&c.f, c.three, tmp);

    uint32_t r[FMAX], s[FMAX], e[FMAX], w[FMAX], u1[FMAX], u2[FMAX];
    while (r_len && !*rb) {
        rb++;
        r_len--;
    }
    while (s_len && !*sb) {
        sb++;
        s_len--;
    }
    if (!r_len || !s_len || r_len > L || s_len > L || !nx_bn_from_be(r, order.n, rb, r_len) ||
        !nx_bn_from_be(s, order.n, sb, s_len) || nx_bn_cmp(r, order.m, order.n) >= 0 ||
        nx_bn_cmp(s, order.m, order.n) >= 0)
        return -1;
    struct pt q, g;
    if (!load_point(&c, &q, pub + 1, pub + 1 + L, L) || !load_point(&c, &g, cv->gx, cv->gy, L))
        return -1;
    digest_int(&order, e, digest, digest_len);
    /* w = s^(n - 2) mod n */
    uint8_t nm2[48];
    memcpy(nm2, cv->n, L);
    for (int i = (int)L - 1; i >= 0; i--) { /* n - 2 (n is odd and large: no wrap past byte 0) */
        uint8_t sub2 = i == (int)L - 1 ? 2 : 1;
        if (nm2[i] >= sub2) {
            nm2[i] = (uint8_t)(nm2[i] - sub2);
            break;
        }
        nm2[i] = (uint8_t)(nm2[i] + 256 - sub2);
    }
    nx_mod_exp(&order, w, s, nm2, L);
    /* u1 = e w, u2 = r w (mod n): Montgomery products fixed up by R^2 */
    nx_mont_mul(&order, u1, e, w);
    nx_mont_mul(&order, u1, u1, order.rr);
    nx_mont_mul(&order, u2, r, w);
    nx_mont_mul(&order, u2, u2, order.rr);
    /* R = u1 G + u2 Q (Shamir) */
    struct pt gq, acc;
    pt_add(&c, &gq, &g, &q);
    memset(&acc, 0, sizeof(acc));
    uint32_t bits = order.bits;
    for (uint32_t i = bits; i-- > 0;) {
        pt_double(&c, &acc, &acc);
        uint32_t b1 = (u1[i / 32] >> (i % 32)) & 1u, b2 = (u2[i / 32] >> (i % 32)) & 1u;
        if (b1 && b2)
            pt_add(&c, &acc, &acc, &gq);
        else if (b1)
            pt_add(&c, &acc, &acc, &g);
        else if (b2)
            pt_add(&c, &acc, &acc, &q);
    }
    if (nx_bn_is_zero(acc.z, c.n))
        return -1;
    /* affine x = X / Z^2, then mod n, compared with r */
    uint32_t zi[FMAX], x[FMAX], zz[FMAX];
    uint8_t pm2[48];
    memcpy(pm2, cv->p, L);
    pm2[L - 1] = (uint8_t)(pm2[L - 1] - 2); /* p ends in 0xff: no borrow */
    nx_mont_from(&c.f, zz, acc.z);
    nx_mod_exp(&c.f, zi, zz, pm2, L); /* Z^-1 (normal form) */
    nx_mont_to(&c.f, zi, zi);
    mul(&c, zz, zi, zi);
    mul(&c, x, acc.x, zz);
    nx_mont_from(&c.f, x, x);
    while (nx_bn_cmp(x, order.m, order.n) >= 0) {
        uint64_t borrow = 0;
        for (uint32_t i = 0; i < order.n; i++) {
            uint64_t d = (uint64_t)x[i] - order.m[i] - borrow;
            tmp[i] = (uint32_t)d;
            borrow = (d >> 32) & 1u;
        }
        memcpy(x, tmp, order.n * 4u);
    }
    return nx_bn_cmp(x, r, order.n) == 0 ? 0 : -1;
}
