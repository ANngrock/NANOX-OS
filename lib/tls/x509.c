/* X.509 parsing and path validation; see nanox/x509.h. */
#include <nanox/civil.h>
#include <nanox/ecc.h>
#include <nanox/hash.h>
#include <nanox/nerr.h>
#include <nanox/string.h>
#include <nanox/x509.h>

/* strlen without libc (the host tests link libc, the guest lib/string.c) */
static uint32_t cstrlen(const char *s)
{
    uint32_t n = 0;
    while (s[n])
        n++;
    return n;
}

/* ---- DER ------------------------------------------------------------------------- */

/* Tag and length at *p; content in out; *p after the TLV. */
static int der_tlv(const uint8_t **p, const uint8_t *end, uint8_t *tag, struct nx_slice *out)
{
    const uint8_t *q = *p;
    if (end - q < 2)
        return 0;
    *tag = *q++;
    if ((*tag & 0x1F) == 0x1F)
        return 0; /* multi-byte tags are not used by X.509 */
    uint32_t len = *q++;
    if (len & 0x80) {
        uint32_t n = len & 0x7F;
        if (n == 0 || n > 3 || end - q < (long)n)
            return 0; /* indefinite or absurd lengths */
        len = 0;
        for (uint32_t i = 0; i < n; i++)
            len = len << 8 | *q++;
        if (len < 0x80 || (n > 1 && len < (1u << (8 * (n - 1)))))
            return 0; /* not the minimal (DER) encoding */
    }
    if ((uint64_t)(end - q) < len)
        return 0;
    out->p = q;
    out->len = len;
    *p = q + len;
    return 1;
}

int nx_der_get(const uint8_t **p, const uint8_t *end, uint8_t tag, struct nx_slice *out)
{
    const uint8_t *q = *p;
    uint8_t t;
    if (!der_tlv(&q, end, &t, out) || t != tag)
        return 0;
    *p = q;
    return 1;
}

static int peek_tag(const uint8_t *p, const uint8_t *end, uint8_t tag)
{
    return p < end && *p == tag;
}

static int slice_eq(struct nx_slice a, const uint8_t *b, uint32_t len)
{
    return a.len == len && memcmp(a.p, b, len) == 0;
}

/* INTEGER content without its sign byte. */
static struct nx_slice uint_content(struct nx_slice s)
{
    while (s.len > 1 && s.p[0] == 0) {
        s.p++;
        s.len--;
    }
    return s;
}

int nx_der_ecdsa_sig(const uint8_t *sig, uint32_t len, struct nx_slice *r, struct nx_slice *s)
{
    const uint8_t *p = sig, *end = sig + len;
    struct nx_slice seq;
    if (!nx_der_get(&p, end, 0x30, &seq) || p != end)
        return 0;
    p = seq.p;
    end = seq.p + seq.len;
    if (!nx_der_get(&p, end, 0x02, r) || !nx_der_get(&p, end, 0x02, s) || p != end)
        return 0;
    if (!r->len || !s->len || (r->p[0] & 0x80) || (s->p[0] & 0x80))
        return 0; /* negative */
    *r = uint_content(*r);
    *s = uint_content(*s);
    return 1;
}

/* ---- object identifiers --------------------------------------------------------------- */

static const uint8_t OID_RSA[] = {0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01};
static const uint8_t OID_SHA256_RSA[] = {0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b};
static const uint8_t OID_SHA384_RSA[] = {0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c};
static const uint8_t OID_SHA512_RSA[] = {0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d};
static const uint8_t OID_EC_PUB[] = {0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01};
static const uint8_t OID_P256[] = {0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07};
static const uint8_t OID_P384[] = {0x2b, 0x81, 0x04, 0x00, 0x22};
static const uint8_t OID_ECDSA_SHA256[] = {0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02};
static const uint8_t OID_ECDSA_SHA384[] = {0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03};
static const uint8_t OID_BC[] = {0x55, 0x1d, 0x13};
static const uint8_t OID_KU[] = {0x55, 0x1d, 0x0f};
static const uint8_t OID_SAN[] = {0x55, 0x1d, 0x11};
static const uint8_t OID_EKU[] = {0x55, 0x1d, 0x25};
static const uint8_t OID_SKI[] = {0x55, 0x1d, 0x0e};
static const uint8_t OID_AKI[] = {0x55, 0x1d, 0x23};
static const uint8_t OID_KP_SERVER[] = {0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01};
static const uint8_t OID_EKU_ANY[] = {0x55, 0x1d, 0x25, 0x00};

/* AlgorithmIdentifier of a signature -> nx_sig_alg (0: unsupported). */
static int sig_alg_of(struct nx_slice algid)
{
    const uint8_t *p = algid.p, *end = algid.p + algid.len;
    struct nx_slice oid, params;
    if (!nx_der_get(&p, end, 0x06, &oid))
        return 0;
    int rsa = 0, alg = 0;
    if (slice_eq(oid, OID_SHA256_RSA, sizeof(OID_SHA256_RSA)))
        alg = NX_SIG_RSA_PKCS1_SHA256, rsa = 1;
    else if (slice_eq(oid, OID_SHA384_RSA, sizeof(OID_SHA384_RSA)))
        alg = NX_SIG_RSA_PKCS1_SHA384, rsa = 1;
    else if (slice_eq(oid, OID_SHA512_RSA, sizeof(OID_SHA512_RSA)))
        alg = NX_SIG_RSA_PKCS1_SHA512, rsa = 1;
    else if (slice_eq(oid, OID_ECDSA_SHA256, sizeof(OID_ECDSA_SHA256)))
        alg = NX_SIG_ECDSA_SHA256;
    else if (slice_eq(oid, OID_ECDSA_SHA384, sizeof(OID_ECDSA_SHA384)))
        alg = NX_SIG_ECDSA_SHA384;
    else
        return 0;
    if (p == end)
        return alg;
    /* RSA: parameters NULL; ECDSA: absent */
    if (rsa && nx_der_get(&p, end, 0x05, &params) && params.len == 0 && p == end)
        return alg;
    return 0;
}

/* ---- time ------------------------------------------------------------------------------- */

static int digits(const uint8_t *p, uint32_t n, uint32_t *out)
{
    uint32_t v = 0;
    for (uint32_t i = 0; i < n; i++) {
        if (p[i] < '0' || p[i] > '9')
            return 0;
        v = v * 10 + (uint32_t)(p[i] - '0');
    }
    *out = v;
    return 1;
}

static int parse_time(const uint8_t **p, const uint8_t *end, int64_t *out)
{
    uint8_t tag;
    struct nx_slice t;
    if (!der_tlv(p, end, &tag, &t))
        return 0;
    uint32_t y, mo, d, h, mi, s;
    const uint8_t *q = t.p;
    if (tag == 0x17 && t.len == 13 && q[12] == 'Z') { /* UTCTime YYMMDDHHMMSSZ */
        if (!digits(q, 2, &y))
            return 0;
        y += y < 50 ? 2000 : 1900;
        q += 2;
    } else if (tag == 0x18 && t.len == 15 && q[14] == 'Z') { /* GeneralizedTime */
        if (!digits(q, 4, &y))
            return 0;
        q += 4;
    } else {
        return 0;
    }
    if (!digits(q, 2, &mo) || !digits(q + 2, 2, &d) || !digits(q + 4, 2, &h) ||
        !digits(q + 6, 2, &mi) || !digits(q + 8, 2, &s) || mo < 1 || mo > 12 || d < 1 ||
        d > 31 || h > 23 || mi > 59 || s > 60)
        return 0;
    *out = nx_unix_time(y, mo, d, h, mi, s);
    return 1;
}

/* ---- extensions ------------------------------------------------------------------------ */

static int parse_extensions(struct nx_cert *c, struct nx_slice exts)
{
    const uint8_t *p = exts.p, *end = exts.p + exts.len;
    struct nx_slice seq;
    if (!nx_der_get(&p, end, 0x30, &seq) || p != end)
        return 0;
    p = seq.p;
    end = seq.p + seq.len;
    while (p < end) {
        struct nx_slice ext, oid, crit, val;
        if (!nx_der_get(&p, end, 0x30, &ext))
            return 0;
        const uint8_t *q = ext.p, *qe = ext.p + ext.len;
        if (!nx_der_get(&q, qe, 0x06, &oid))
            return 0;
        int critical = 0;
        if (peek_tag(q, qe, 0x01)) {
            if (!nx_der_get(&q, qe, 0x01, &crit) || crit.len != 1)
                return 0;
            critical = crit.p[0] != 0;
        }
        if (!nx_der_get(&q, qe, 0x04, &val) || q != qe)
            return 0;
        const uint8_t *v = val.p, *ve = val.p + val.len;
        if (slice_eq(oid, OID_BC, sizeof(OID_BC))) {
            struct nx_slice bc, b, pl;
            if (!nx_der_get(&v, ve, 0x30, &bc) || v != ve)
                return 0;
            const uint8_t *b0 = bc.p, *be = bc.p + bc.len;
            c->has_bc = 1;
            if (peek_tag(b0, be, 0x01)) {
                if (!nx_der_get(&b0, be, 0x01, &b) || b.len != 1)
                    return 0;
                c->is_ca = b.p[0] != 0;
            }
            if (peek_tag(b0, be, 0x02)) {
                if (!nx_der_get(&b0, be, 0x02, &pl) || pl.len != 1 || (pl.p[0] & 0x80))
                    return 0;
                c->path_len = pl.p[0];
            }
            if (b0 != be)
                return 0;
        } else if (slice_eq(oid, OID_KU, sizeof(OID_KU))) {
            struct nx_slice bits;
            if (!nx_der_get(&v, ve, 0x03, &bits) || v != ve || bits.len < 2 || bits.p[0] > 7)
                return 0;
            c->has_ku = 1;
            c->ku = bits.p[1];
        } else if (slice_eq(oid, OID_SAN, sizeof(OID_SAN))) {
            if (!nx_der_get(&v, ve, 0x30, &c->san) || v != ve)
                return 0;
        } else if (slice_eq(oid, OID_EKU, sizeof(OID_EKU))) {
            struct nx_slice list, kp;
            if (!nx_der_get(&v, ve, 0x30, &list) || v != ve)
                return 0;
            c->has_eku = 1;
            const uint8_t *k = list.p, *ke = list.p + list.len;
            while (k < ke) {
                if (!nx_der_get(&k, ke, 0x06, &kp))
                    return 0;
                if (slice_eq(kp, OID_KP_SERVER, sizeof(OID_KP_SERVER)) ||
                    slice_eq(kp, OID_EKU_ANY, sizeof(OID_EKU_ANY)))
                    c->eku_server = 1;
            }
        } else if (slice_eq(oid, OID_SKI, sizeof(OID_SKI)) ||
                   slice_eq(oid, OID_AKI, sizeof(OID_AKI))) {
            /* identifiers: path building compares names instead */
        } else if (critical) {
            return 0; /* an unknown critical extension must not be ignored */
        }
    }
    return 1;
}

/* ---- certificate ------------------------------------------------------------------------ */

int nx_x509_parse(const uint8_t *der, uint32_t len, struct nx_cert *c)
{
    memset(c, 0, sizeof(*c));
    c->path_len = -1;
    c->der.p = der;
    c->der.len = len;
    const uint8_t *p = der, *end = der + len;
    struct nx_slice cert, tbs, algid, sigbits;
    if (!nx_der_get(&p, end, 0x30, &cert) || p != end)
        return NXE_TLS_CERT_BAD;
    p = cert.p;
    end = cert.p + cert.len;
    const uint8_t *tbs_start = p;
    if (!nx_der_get(&p, end, 0x30, &tbs))
        return NXE_TLS_CERT_BAD;
    c->tbs.p = tbs_start;
    c->tbs.len = (uint32_t)(p - tbs_start);
    if (!nx_der_get(&p, end, 0x30, &algid) || !nx_der_get(&p, end, 0x03, &sigbits) || p != end)
        return NXE_TLS_CERT_BAD;
    if (sigbits.len < 2 || sigbits.p[0] != 0)
        return NXE_TLS_CERT_BAD;
    c->sig.p = sigbits.p + 1;
    c->sig.len = sigbits.len - 1;
    c->sig_alg = sig_alg_of(algid);

    const uint8_t *t = tbs.p, *te = tbs.p + tbs.len;
    struct nx_slice ver, serial, inner_alg, validity, spki;
    c->version = 1;
    if (peek_tag(t, te, 0xA0)) {
        const uint8_t *v;
        if (!nx_der_get(&t, te, 0xA0, &ver))
            return NXE_TLS_CERT_BAD;
        v = ver.p;
        struct nx_slice vi;
        if (!nx_der_get(&v, ver.p + ver.len, 0x02, &vi) || vi.len != 1 || vi.p[0] > 2)
            return NXE_TLS_CERT_BAD;
        c->version = vi.p[0] + 1u;
    }
    const uint8_t *issuer_start;
    if (!nx_der_get(&t, te, 0x02, &serial) || !nx_der_get(&t, te, 0x30, &inner_alg))
        return NXE_TLS_CERT_BAD;
    if (inner_alg.len != algid.len || memcmp(inner_alg.p, algid.p, algid.len) != 0)
        return NXE_TLS_CERT_BAD; /* RFC 5280 §4.1.1.2: both must be the same */
    issuer_start = t;
    struct nx_slice name;
    if (!nx_der_get(&t, te, 0x30, &name))
        return NXE_TLS_CERT_BAD;
    c->issuer.p = issuer_start;
    c->issuer.len = (uint32_t)(t - issuer_start);
    if (!nx_der_get(&t, te, 0x30, &validity))
        return NXE_TLS_CERT_BAD;
    const uint8_t *vp = validity.p, *ve = validity.p + validity.len;
    if (!parse_time(&vp, ve, &c->not_before) || !parse_time(&vp, ve, &c->not_after) || vp != ve)
        return NXE_TLS_CERT_BAD;
    const uint8_t *subject_start = t;
    if (!nx_der_get(&t, te, 0x30, &name))
        return NXE_TLS_CERT_BAD;
    c->subject.p = subject_start;
    c->subject.len = (uint32_t)(t - subject_start);
    if (!nx_der_get(&t, te, 0x30, &spki))
        return NXE_TLS_CERT_BAD;
    /* SubjectPublicKeyInfo */
    const uint8_t *s = spki.p, *se = spki.p + spki.len;
    struct nx_slice kalg, kbits, koid, kparam;
    if (!nx_der_get(&s, se, 0x30, &kalg) || !nx_der_get(&s, se, 0x03, &kbits) || s != se ||
        kbits.len < 2 || kbits.p[0] != 0)
        return NXE_TLS_CERT_BAD;
    const uint8_t *a = kalg.p, *ae = kalg.p + kalg.len;
    if (!nx_der_get(&a, ae, 0x06, &koid))
        return NXE_TLS_CERT_BAD;
    if (slice_eq(koid, OID_RSA, sizeof(OID_RSA))) {
        const uint8_t *k = kbits.p + 1, *ke = kbits.p + kbits.len;
        struct nx_slice rsa, n, e;
        if (!nx_der_get(&k, ke, 0x30, &rsa) || k != ke)
            return NXE_TLS_CERT_BAD;
        k = rsa.p;
        ke = rsa.p + rsa.len;
        if (!nx_der_get(&k, ke, 0x02, &n) || !nx_der_get(&k, ke, 0x02, &e) || k != ke ||
            (n.p[0] & 0x80) || (e.p[0] & 0x80))
            return NXE_TLS_CERT_BAD;
        c->key_type = NX_KEY_RSA;
        c->rsa_n = uint_content(n);
        c->rsa_e = uint_content(e);
    } else if (slice_eq(koid, OID_EC_PUB, sizeof(OID_EC_PUB))) {
        if (!nx_der_get(&a, ae, 0x06, &kparam) || a != ae)
            return NXE_TLS_CERT_BAD;
        if (slice_eq(kparam, OID_P256, sizeof(OID_P256)))
            c->curve = NX_P256;
        else if (slice_eq(kparam, OID_P384, sizeof(OID_P384)))
            c->curve = NX_P384;
        c->key_type = NX_KEY_EC;
        c->ec_point.p = kbits.p + 1;
        c->ec_point.len = kbits.len - 1;
    }
    /* optional unique ids [1] [2], extensions [3] */
    struct nx_slice skip;
    if (peek_tag(t, te, 0x81) && !nx_der_get(&t, te, 0x81, &skip))
        return NXE_TLS_CERT_BAD;
    if (peek_tag(t, te, 0x82) && !nx_der_get(&t, te, 0x82, &skip))
        return NXE_TLS_CERT_BAD;
    if (peek_tag(t, te, 0xA3)) {
        struct nx_slice exts;
        if (c->version != 3 || !nx_der_get(&t, te, 0xA3, &exts) || !parse_extensions(c, exts))
            return NXE_TLS_CERT_BAD;
    }
    if (t != te)
        return NXE_TLS_CERT_BAD;
    return NXE_OK;
}

/* ---- names ------------------------------------------------------------------------------- */

static char lower(char ch)
{
    return ch >= 'A' && ch <= 'Z' ? (char)(ch - 'A' + 'a') : ch;
}

/* dNSName pattern (not NUL-terminated) against host. */
static int dns_match(const uint8_t *pat, uint32_t plen, const char *host)
{
    uint32_t hlen = (uint32_t)cstrlen(host);
    if (hlen && host[hlen - 1] == '.')
        hlen--;
    if (plen >= 2 && pat[0] == '*' && pat[1] == '.') {
        /* "*." + suffix: exactly one non-empty left-most label */
        uint32_t dot = 0;
        while (dot < hlen && host[dot] != '.')
            dot++;
        if (dot == 0 || dot == hlen)
            return 0;
        pat++;
        plen--;
        host += dot;
        hlen -= dot;
        /* the suffix must itself have two labels ("*.com" is refused) */
        uint32_t dots = 0;
        for (uint32_t i = 1; i < plen; i++)
            dots += pat[i] == '.';
        if (dots < 1)
            return 0;
    }
    if (plen != hlen)
        return 0;
    for (uint32_t i = 0; i < plen; i++) {
        if (pat[i] == '*')
            return 0; /* partial wildcards are not supported */
        if (lower((char)pat[i]) != lower(host[i]))
            return 0;
    }
    return 1;
}

static int parse_ipv4(const char *s, uint8_t out[4])
{
    for (int part = 0; part < 4; part++) {
        uint32_t v = 0, n = 0;
        while (*s >= '0' && *s <= '9' && n < 3) {
            v = v * 10 + (uint32_t)(*s++ - '0');
            n++;
        }
        if (!n || v > 255)
            return 0;
        out[part] = (uint8_t)v;
        if (part < 3 && *s++ != '.')
            return 0;
    }
    return *s == 0;
}

int nx_x509_host_matches(const struct nx_cert *c, const char *host)
{
    if (!c->san.p)
        return 0; /* no fallback to the subject's common name (RFC 6125 §6.4.4 allows
                     none for new clients) */
    uint8_t ip[4];
    int is_ip = parse_ipv4(host, ip);
    const uint8_t *p = c->san.p, *end = c->san.p + c->san.len;
    while (p < end) {
        uint8_t tag;
        struct nx_slice v;
        if (!der_tlv(&p, end, &tag, &v))
            return 0;
        if (!is_ip && tag == 0x82 && dns_match(v.p, v.len, host))
            return 1;
        if (is_ip && tag == 0x87 && v.len == 4 && memcmp(v.p, ip, 4) == 0)
            return 1;
    }
    return 0;
}

/* ---- signatures ---------------------------------------------------------------------------- */

static int hash_of(int alg)
{
    switch (alg) {
    case NX_SIG_RSA_PKCS1_SHA256:
    case NX_SIG_ECDSA_SHA256:
    case NX_SIG_RSA_PSS_SHA256: return NX_HASH_SHA256;
    case NX_SIG_RSA_PKCS1_SHA384:
    case NX_SIG_ECDSA_SHA384:
    case NX_SIG_RSA_PSS_SHA384: return NX_HASH_SHA384;
    case NX_SIG_RSA_PKCS1_SHA512:
    case NX_SIG_RSA_PSS_SHA512: return NX_HASH_SHA512;
    default: return 0;
    }
}

int nx_x509_verify(const struct nx_cert *key, int alg, const uint8_t *data, uint32_t len,
                   const uint8_t *sig, uint32_t sig_len)
{
    int h = hash_of(alg);
    if (!h)
        return NXE_TLS_CERT_BAD;
    uint8_t digest[NX_HASH_MAX];
    nx_hash(h, data, len, digest);
    uint32_t hl = nx_hash_len(h);
    if (alg == NX_SIG_ECDSA_SHA256 || alg == NX_SIG_ECDSA_SHA384) {
        if (key->key_type != NX_KEY_EC || !key->curve)
            return key->key_type == NX_KEY_EC ? NXE_TLS_CERT_BAD : NXE_TLS_BAD_SIGNATURE;
        struct nx_slice r, s;
        if (!nx_der_ecdsa_sig(sig, sig_len, &r, &s))
            return NXE_TLS_BAD_SIGNATURE;
        return nx_ecdsa_verify(key->curve, key->ec_point.p, key->ec_point.len, digest, hl, r.p,
                               r.len, s.p, s.len) == 0
                   ? NXE_OK
                   : NXE_TLS_BAD_SIGNATURE;
    }
    if (key->key_type != NX_KEY_RSA)
        return NXE_TLS_BAD_SIGNATURE;
    int ok;
    if (alg == NX_SIG_RSA_PSS_SHA256 || alg == NX_SIG_RSA_PSS_SHA384 ||
        alg == NX_SIG_RSA_PSS_SHA512)
        ok = nx_rsa_verify_pss(key->rsa_n.p, key->rsa_n.len, key->rsa_e.p, key->rsa_e.len, h,
                               digest, sig, sig_len) == 0;
    else
        ok = nx_rsa_verify_pkcs1(key->rsa_n.p, key->rsa_n.len, key->rsa_e.p, key->rsa_e.len, h,
                                 digest, sig, sig_len) == 0;
    return ok ? NXE_OK : NXE_TLS_BAD_SIGNATURE;
}

int nx_x509_check_signature(const struct nx_cert *child, const struct nx_cert *issuer)
{
    if (!child->sig_alg)
        return NXE_TLS_CERT_BAD;
    return nx_x509_verify(issuer, child->sig_alg, child->tbs.p, child->tbs.len, child->sig.p,
                          child->sig.len);
}

/* ---- path validation ----------------------------------------------------------------------- */

static void set_detail(struct nx_chain_result *res, const char *s)
{
    uint32_t i = 0;
    for (; s[i] && i + 1 < sizeof(res->detail); i++)
        res->detail[i] = s[i];
    res->detail[i] = 0;
}

static int fail(struct nx_chain_result *res, int err, const char *detail)
{
    res->err = err;
    set_detail(res, detail);
    return err;
}

static int same_name(struct nx_slice a, struct nx_slice b)
{
    return a.len == b.len && memcmp(a.p, b.p, a.len) == 0;
}

static int valid_at(const struct nx_cert *c, int64_t now)
{
    return now >= c->not_before && now <= c->not_after;
}

/* May c issue certificates, with `below` CA certificates under it? */
static int can_issue(const struct nx_cert *c, uint32_t below)
{
    if (c->version == 3 && (!c->has_bc || !c->is_ca))
        return 0;
    if (c->has_ku && !(c->ku & NX_KU_KEY_CERT_SIGN))
        return 0;
    if (c->path_len >= 0 && below > (uint32_t)c->path_len)
        return 0;
    return 1;
}

int nx_x509_verify_chain(const struct nx_cert *certs, uint32_t ncerts,
                         const struct nx_cert *anchors, uint32_t nanchors, const char *host,
                         int64_t now, struct nx_chain_result *res)
{
    res->err = NXE_OK;
    res->depth = 0;
    res->anchor = -1;
    res->detail[0] = 0;
    if (!ncerts)
        return fail(res, NXE_TLS_CERT_BAD, "no_certificate");
    const struct nx_cert *leaf = &certs[0];
    if (!nx_x509_host_matches(leaf, host))
        return fail(res, NXE_TLS_CERT_NAME, "leaf_name");
    if (leaf->has_eku && !leaf->eku_server)
        return fail(res, NXE_TLS_CERT_BAD, "leaf_eku");
    if (leaf->has_ku && !(leaf->ku & NX_KU_DIGITAL_SIGNATURE))
        return fail(res, NXE_TLS_CERT_BAD, "leaf_key_usage");
    const struct nx_cert *cur = leaf;
    uint32_t used = 1; /* bit i: certs[i] already on the path */
    int sig_failed = 0;
    for (uint32_t depth = 0; depth < NX_CHAIN_MAX; depth++) {
        if (!valid_at(cur, now))
            return fail(res, NXE_TLS_CERT_EXPIRED, depth ? "intermediate_validity" : "leaf_validity");
        /* a trust anchor issued it? */
        for (uint32_t i = 0; i < nanchors; i++) {
            const struct nx_cert *a = &anchors[i];
            if (!same_name(cur->issuer, a->subject))
                continue;
            int st = nx_x509_check_signature(cur, a);
            if (st != NXE_OK) {
                sig_failed = st;
                continue;
            }
            if (!valid_at(a, now))
                return fail(res, NXE_TLS_CERT_EXPIRED, "anchor_validity");
            if (!can_issue(a, depth))
                return fail(res, NXE_TLS_CERT_BAD, "anchor_not_ca");
            res->depth = depth + 2;
            res->anchor = (int)i;
            return NXE_OK;
        }
        /* the certificate is itself an anchor (sent by the server) */
        for (uint32_t i = 0; i < nanchors; i++)
            if (cur->der.len == anchors[i].der.len &&
                memcmp(cur->der.p, anchors[i].der.p, cur->der.len) == 0) {
                res->depth = depth + 1;
                res->anchor = (int)i;
                return NXE_OK;
            }
        /* an intermediate from the server's list */
        const struct nx_cert *next = 0;
        for (uint32_t i = 1; i < ncerts && !next; i++) {
            if ((used >> i) & 1u || !same_name(cur->issuer, certs[i].subject))
                continue;
            int st = nx_x509_check_signature(cur, &certs[i]);
            if (st != NXE_OK) {
                sig_failed = st;
                continue;
            }
            if (!can_issue(&certs[i], depth))
                return fail(res, NXE_TLS_CERT_BAD, "intermediate_not_ca");
            next = &certs[i];
            used |= 1u << i;
        }
        if (!next) {
            if (sig_failed)
                return fail(res, sig_failed, "chain_signature");
            return fail(res, NXE_TLS_CERT_UNTRUSTED, depth ? "no_anchor_for_intermediate"
                                                           : "no_anchor_for_leaf");
        }
        cur = next;
    }
    return fail(res, NXE_TLS_CERT_UNTRUSTED, "path_too_long");
}
