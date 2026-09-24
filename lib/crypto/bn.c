/* Bignum with Montgomery multiplication (CIOS); see nanox/ecc.h. */
#include <nanox/ecc.h>
#include <nanox/string.h>

int nx_bn_from_be(uint32_t *out, uint32_t n, const uint8_t *be, uint32_t len)
{
    memset(out, 0, (uint64_t)n * 4u);
    for (uint32_t i = 0; i < len; i++) {
        uint8_t v = be[len - 1 - i];
        if (i / 4 >= n) {
            if (v)
                return 0;
            continue;
        }
        out[i / 4] |= (uint32_t)v << (8 * (i % 4));
    }
    return 1;
}

void nx_bn_to_be(uint8_t *be, uint32_t len, const uint32_t *a, uint32_t n)
{
    for (uint32_t i = 0; i < len; i++)
        be[len - 1 - i] = i / 4 < n ? (uint8_t)(a[i / 4] >> (8 * (i % 4))) : 0;
}

int nx_bn_cmp(const uint32_t *a, const uint32_t *b, uint32_t n)
{
    for (uint32_t i = n; i-- > 0;)
        if (a[i] != b[i])
            return a[i] < b[i] ? -1 : 1;
    return 0;
}

int nx_bn_is_zero(const uint32_t *a, uint32_t n)
{
    uint32_t x = 0;
    for (uint32_t i = 0; i < n; i++)
        x |= a[i];
    return x == 0;
}

uint32_t nx_bn_bits(const uint32_t *a, uint32_t n)
{
    for (uint32_t i = n; i-- > 0;)
        if (a[i])
            return i * 32u + (32u - (uint32_t)__builtin_clz(a[i]));
    return 0;
}

static uint32_t sub_n(uint32_t *out, const uint32_t *a, const uint32_t *b, uint32_t n)
{
    uint64_t borrow = 0;
    for (uint32_t i = 0; i < n; i++) {
        uint64_t d = (uint64_t)a[i] - b[i] - borrow;
        out[i] = (uint32_t)d;
        borrow = (d >> 32) & 1u;
    }
    return (uint32_t)borrow;
}

static uint32_t add_n(uint32_t *out, const uint32_t *a, const uint32_t *b, uint32_t n)
{
    uint64_t carry = 0;
    for (uint32_t i = 0; i < n; i++) {
        uint64_t s = (uint64_t)a[i] + b[i] + carry;
        out[i] = (uint32_t)s;
        carry = s >> 32;
    }
    return (uint32_t)carry;
}

/* a = 2 a mod m (a < m). */
static void dbl_mod(const struct nx_mont *c, uint32_t *a)
{
    uint32_t top = a[c->n - 1] >> 31;
    for (uint32_t i = c->n; i-- > 1;)
        a[i] = a[i] << 1 | a[i - 1] >> 31;
    a[0] <<= 1;
    if (top || nx_bn_cmp(a, c->m, c->n) >= 0)
        sub_n(a, a, c->m, c->n);
}

int nx_mont_init(struct nx_mont *c, const uint8_t *mod_be, uint32_t len)
{
    while (len && !*mod_be) {
        mod_be++;
        len--;
    }
    if (!len || len > NX_BN_MAX_BITS / 8u || !(mod_be[len - 1] & 1u))
        return 0;
    memset(c, 0, sizeof(*c));
    c->n = (len + 3u) / 4u;
    nx_bn_from_be(c->m, c->n, mod_be, len);
    c->bits = nx_bn_bits(c->m, c->n);
    if (c->bits < 2)
        return 0;
    uint32_t x = c->m[0]; /* Newton: x = m^-1 mod 2^32 */
    for (int i = 0; i < 5; i++)
        x *= 2u - c->m[0] * x;
    c->minv = 0u - x;
    c->one[0] = 1; /* R mod m by doubling 1 (32 n) times */
    for (uint32_t i = 0; i < 32u * c->n; i++)
        dbl_mod(c, c->one);
    memcpy(c->rr, c->one, c->n * 4u);
    for (uint32_t i = 0; i < 32u * c->n; i++)
        dbl_mod(c, c->rr);
    return 1;
}

void nx_mont_mul(const struct nx_mont *c, uint32_t *out, const uint32_t *a, const uint32_t *b)
{
    uint32_t n = c->n;
    uint32_t t[NX_BN_LIMBS + 2];
    memset(t, 0, (n + 2u) * 4u);
    for (uint32_t i = 0; i < n; i++) {
        uint64_t carry = 0;
        for (uint32_t j = 0; j < n; j++) {
            uint64_t s = (uint64_t)t[j] + (uint64_t)a[j] * b[i] + carry;
            t[j] = (uint32_t)s;
            carry = s >> 32;
        }
        uint64_t s = (uint64_t)t[n] + carry;
        t[n] = (uint32_t)s;
        t[n + 1] = (uint32_t)(s >> 32);
        uint32_t q = t[0] * c->minv;
        carry = ((uint64_t)t[0] + (uint64_t)q * c->m[0]) >> 32;
        for (uint32_t j = 1; j < n; j++) {
            s = (uint64_t)t[j] + (uint64_t)q * c->m[j] + carry;
            t[j - 1] = (uint32_t)s;
            carry = s >> 32;
        }
        s = (uint64_t)t[n] + carry;
        t[n - 1] = (uint32_t)s;
        t[n] = t[n + 1] + (uint32_t)(s >> 32);
    }
    if (t[n] || nx_bn_cmp(t, c->m, n) >= 0)
        sub_n(t, t, c->m, n);
    memcpy(out, t, n * 4u);
}

void nx_mont_to(const struct nx_mont *c, uint32_t *out, const uint32_t *a)
{
    nx_mont_mul(c, out, a, c->rr);
}

void nx_mont_from(const struct nx_mont *c, uint32_t *out, const uint32_t *a)
{
    uint32_t one[NX_BN_LIMBS];
    memset(one, 0, c->n * 4u);
    one[0] = 1;
    nx_mont_mul(c, out, a, one);
}

void nx_mod_add(const struct nx_mont *c, uint32_t *out, const uint32_t *a, const uint32_t *b)
{
    uint32_t carry = add_n(out, a, b, c->n);
    if (carry || nx_bn_cmp(out, c->m, c->n) >= 0)
        sub_n(out, out, c->m, c->n);
}

void nx_mod_sub(const struct nx_mont *c, uint32_t *out, const uint32_t *a, const uint32_t *b)
{
    if (sub_n(out, a, b, c->n))
        add_n(out, out, c->m, c->n);
}

void nx_mod_exp(const struct nx_mont *c, uint32_t *out, const uint32_t *base, const uint8_t *exp,
                uint32_t exp_len)
{
    uint32_t x[NX_BN_LIMBS], acc[NX_BN_LIMBS];
    nx_mont_to(c, x, base);
    memcpy(acc, c->one, c->n * 4u);
    for (uint32_t i = 0; i < exp_len; i++)
        for (int bit = 7; bit >= 0; bit--) {
            nx_mont_mul(c, acc, acc, acc);
            if ((exp[i] >> bit) & 1u)
                nx_mont_mul(c, acc, acc, x);
        }
    nx_mont_from(c, out, acc);
}
