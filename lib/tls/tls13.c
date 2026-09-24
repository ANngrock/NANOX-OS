/*
 * TLS 1.3 client; see nanox/tls.h.  Message flow (RFC 8446 §2):
 *
 *   ClientHello (key_share x25519, supported_versions 1.3, SNI, ALPN,
 *                signature_algorithms, supported_groups)            ->
 *                                   <- ServerHello (+ ChangeCipherSpec)
 *                        <- {EncryptedExtensions} {Certificate}
 *                           {CertificateVerify} {Finished}
 *   {Finished}                                                       ->
 *   [application data]                                              <->
 */
#include <nanox/ecc.h>
#include <nanox/nerr.h>
#include <nanox/string.h>
#include <nanox/tls.h>

/* strlen without libc (the host tests link libc, the guest lib/string.c) */
static uint32_t cstrlen(const char *s)
{
    uint32_t n = 0;
    while (s[n])
        n++;
    return n;
}

enum {
    CT_CCS = 20,
    CT_ALERT = 21,
    CT_HANDSHAKE = 22,
    CT_APPDATA = 23,
};

enum {
    HS_CLIENT_HELLO = 1,
    HS_SERVER_HELLO = 2,
    HS_NEW_SESSION_TICKET = 4,
    HS_ENCRYPTED_EXTENSIONS = 8,
    HS_CERTIFICATE = 11,
    HS_CERTIFICATE_REQUEST = 13,
    HS_CERTIFICATE_VERIFY = 15,
    HS_FINISHED = 20,
    HS_KEY_UPDATE = 24,
};

enum {
    AL_CLOSE_NOTIFY = 0,
    AL_UNEXPECTED_MESSAGE = 10,
    AL_BAD_RECORD_MAC = 20,
    AL_HANDSHAKE_FAILURE = 40,
    AL_BAD_CERTIFICATE = 42,
    AL_UNSUPPORTED_CERTIFICATE = 43,
    AL_CERTIFICATE_EXPIRED = 45,
    AL_ILLEGAL_PARAMETER = 47,
    AL_UNKNOWN_CA = 48,
    AL_DECODE_ERROR = 50,
    AL_DECRYPT_ERROR = 51,
    AL_PROTOCOL_VERSION = 70,
    AL_INTERNAL_ERROR = 80,
    AL_UNSUPPORTED_EXTENSION = 110,
};

#define SIG_ECDSA_P256_SHA256 0x0403u
#define SIG_ECDSA_P384_SHA384 0x0503u
#define SIG_RSA_PSS_SHA256 0x0804u
#define SIG_RSA_PSS_SHA384 0x0805u
#define SIG_RSA_PSS_SHA512 0x0806u
#define SIG_RSA_PKCS1_SHA256 0x0401u
#define SIG_RSA_PKCS1_SHA384 0x0501u
#define SIG_RSA_PKCS1_SHA512 0x0601u

static const uint8_t HRR_RANDOM[32] = {
    0xCF, 0x21, 0xAD, 0x74, 0xE5, 0x9A, 0x61, 0x11, 0xBE, 0x1D, 0x8C, 0x02, 0x1E, 0x65, 0xB8, 0x91,
    0xC2, 0xA2, 0x11, 0x16, 0x7A, 0xBB, 0x8C, 0x5E, 0x07, 0x9E, 0x09, 0xE2, 0xC8, 0xA8, 0x33, 0x9C};

const char *tls_suite_name(uint16_t s)
{
    switch (s) {
    case TLS_SUITE_AES128_GCM: return "TLS_AES_128_GCM_SHA256";
    case TLS_SUITE_CHACHA20: return "TLS_CHACHA20_POLY1305_SHA256";
    default: return "none";
    }
}

const char *tls_sig_name(uint16_t s)
{
    switch (s) {
    case SIG_ECDSA_P256_SHA256: return "ecdsa_secp256r1_sha256";
    case SIG_ECDSA_P384_SHA384: return "ecdsa_secp384r1_sha384";
    case SIG_RSA_PSS_SHA256: return "rsa_pss_rsae_sha256";
    case SIG_RSA_PSS_SHA384: return "rsa_pss_rsae_sha384";
    case SIG_RSA_PSS_SHA512: return "rsa_pss_rsae_sha512";
    default: return "none";
    }
}

static inline void put16(uint8_t *p, uint32_t v)
{
    p[0] = (uint8_t)(v >> 8);
    p[1] = (uint8_t)v;
}
static inline void put24(uint8_t *p, uint32_t v)
{
    p[0] = (uint8_t)(v >> 16);
    p[1] = (uint8_t)(v >> 8);
    p[2] = (uint8_t)v;
}
static inline uint32_t get16(const uint8_t *p)
{
    return (uint32_t)p[0] << 8 | p[1];
}
static inline uint32_t get24(const uint8_t *p)
{
    return (uint32_t)p[0] << 16 | (uint32_t)p[1] << 8 | p[2];
}

/* ---- key schedule (RFC 8446 §7) ------------------------------------------------------ */

void tls_expand_label(const uint8_t *secret, const char *label, const uint8_t *ctx,
                      uint32_t ctx_len, uint8_t *out, uint32_t len)
{
    uint8_t info[2 + 1 + 6 + 64 + 1 + 64];
    uint32_t ll = (uint32_t)cstrlen(label), n = 0;
    put16(info, len);
    n = 2;
    info[n++] = (uint8_t)(6 + ll);
    memcpy(info + n, "tls13 ", 6);
    n += 6;
    memcpy(info + n, label, ll);
    n += ll;
    info[n++] = (uint8_t)ctx_len;
    if (ctx_len)
        memcpy(info + n, ctx, ctx_len);
    n += ctx_len;
    nx_hkdf_expand(NX_HASH_SHA256, secret, info, n, out, len);
}

static void transcript_hash(const struct tls_conn *c, uint8_t out[32])
{
    struct nx_hash h = c->transcript; /* a copy: the running hash continues */
    nx_hash_final(&h, out);
}

static void derive_secret(const uint8_t *secret, const char *label, const uint8_t *th,
                          uint8_t out[32])
{
    tls_expand_label(secret, label, th, 32, out, 32);
}

static void set_keys(struct tls_conn *c, struct tls_dir *d, const uint8_t secret[32])
{
    uint32_t klen = c->suite == TLS_SUITE_AES128_GCM ? 16u : 32u;
    memcpy(d->secret, secret, 32);
    tls_expand_label(secret, "key", 0, 0, d->key, klen);
    tls_expand_label(secret, "iv", 0, 0, d->iv, 12);
    d->seq = 0;
    if (c->suite == TLS_SUITE_AES128_GCM)
        nx_gcm_init(&d->gcm, d->key, 16);
}

static void nonce_of(const struct tls_dir *d, uint8_t nonce[12])
{
    memcpy(nonce, d->iv, 12);
    for (int i = 0; i < 8; i++)
        nonce[11 - i] ^= (uint8_t)(d->seq >> (8 * i));
}

/* ---- records ----------------------------------------------------------------------------- */

static int send_raw(struct tls_conn *c, const uint8_t *p, uint32_t len)
{
    int r = c->io.send(c->io.ctx, p, len);
    if (r == NXE_OK)
        c->bytes_out += len;
    return r;
}

/* One record of type ct; encrypted when the write keys are set. */
static int send_record(struct tls_conn *c, uint8_t ct, const uint8_t *data, uint32_t len,
                       int encrypted)
{
    uint8_t *o = c->out;
    if (!encrypted) {
        o[0] = ct;
        put16(o + 1, ct == CT_HANDSHAKE && c->state == 0 ? 0x0301u : 0x0303u);
        put16(o + 3, len);
        memcpy(o + 5, data, len);
        c->records_out++;
        return send_raw(c, o, 5 + len);
    }
    uint32_t inner = len + 1, total = inner + NX_AEAD_TAG;
    o[0] = CT_APPDATA;
    put16(o + 1, 0x0303u);
    put16(o + 3, total);
    memcpy(o + 5, data, len);
    o[5 + len] = ct;
    uint8_t nonce[12];
    nonce_of(&c->wr, nonce);
    if (c->suite == TLS_SUITE_AES128_GCM)
        nx_gcm_seal(&c->wr.gcm, nonce, o, 5, o + 5, inner, o + 5, o + 5 + inner);
    else
        nx_chacha_seal(c->wr.key, nonce, o, 5, o + 5, inner, o + 5, o + 5 + inner);
    c->wr.seq++;
    c->records_out++;
    return send_raw(c, o, 5 + total);
}

static void send_alert(struct tls_conn *c, uint8_t desc, int encrypted)
{
    uint8_t a[2] = {desc == AL_CLOSE_NOTIFY ? 1 : 2, desc};
    send_record(c, CT_ALERT, a, 2, encrypted);
}

static int fail(struct tls_conn *c, int err, uint8_t alert, int encrypted)
{
    if (!c->err)
        c->err = err;
    if (alert != 0xFF && err != NXE_TLS_ALERT && nx_err_class(err) == NXC_TLS)
        send_alert(c, alert, encrypted);
    return err;
}

/* Reads exactly n more bytes of the current record into c->rec. */
static int fill(struct tls_conn *c, uint32_t want, uint32_t timeout_ms)
{
    while (c->rec_have < want) {
        int r = c->io.recv(c->io.ctx, c->rec + c->rec_have, want - c->rec_have, timeout_ms);
        if (r == 0)
            return NXE_PEER_CLOSED;
        if (r < 0)
            return -r;
        c->rec_have += (uint32_t)r;
        c->bytes_in += (uint32_t)r;
    }
    return NXE_OK;
}

/* Next record into c->plain (decrypted when the read keys are set).  CCS
 * records during the handshake are skipped.  A record read only in part
 * when the transport times out stays in c->rec for the next call. */
static int read_record(struct tls_conn *c, int encrypted, uint32_t timeout_ms)
{
    for (;;) {
        int r = fill(c, 5, timeout_ms);
        if (r != NXE_OK)
            return r;
        uint8_t ct = c->rec[0];
        uint32_t len = get16(c->rec + 3);
        if (len > TLS_RECORD_MAX + 256 || (c->rec[1] != 0x03))
            return fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, encrypted);
        r = fill(c, 5 + len, timeout_ms);
        if (r != NXE_OK)
            return r;
        c->rec_have = 0; /* complete: the next call starts a new record */
        c->records_in++;
        const uint8_t *body = c->rec + 5;
        if (ct == CT_CCS) {
            if (len != 1 || body[0] != 1 || c->state >= 3)
                return fail(c, NXE_TLS_PROTOCOL, AL_UNEXPECTED_MESSAGE, encrypted);
            continue; /* middlebox compatibility (RFC 8446 §5) */
        }
        if (!encrypted) {
            if (ct != CT_HANDSHAKE && ct != CT_ALERT)
                return fail(c, NXE_TLS_PROTOCOL, AL_UNEXPECTED_MESSAGE, 0);
            memcpy(c->plain, body, len);
            c->plain_len = len;
            c->plain_off = 0;
            c->plain_type = ct;
        } else {
            if (ct != CT_APPDATA || len < NX_AEAD_TAG + 1)
                return fail(c, NXE_TLS_PROTOCOL, AL_UNEXPECTED_MESSAGE, 1);
            uint32_t clen = len - NX_AEAD_TAG;
            uint8_t nonce[12];
            nonce_of(&c->rd, nonce);
            int ok = c->suite == TLS_SUITE_AES128_GCM
                         ? nx_gcm_open(&c->rd.gcm, nonce, c->rec, 5, body, clen, body + clen,
                                       c->plain)
                         : nx_chacha_open(c->rd.key, nonce, c->rec, 5, body, clen, body + clen,
                                          c->plain);
            if (ok != 0)
                return fail(c, NXE_TLS_DECRYPT, AL_BAD_RECORD_MAC, 1);
            c->rd.seq++;
            uint32_t n = clen;
            while (n && c->plain[n - 1] == 0)
                n--; /* padding */
            if (!n)
                return fail(c, NXE_TLS_PROTOCOL, AL_UNEXPECTED_MESSAGE, 1);
            c->plain_type = c->plain[n - 1];
            c->plain_len = n - 1;
            c->plain_off = 0;
            if (c->plain_len > TLS_RECORD_MAX)
                return fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, 1);
        }
        if (c->plain_type == CT_ALERT) {
            if (c->plain_len != 2)
                return fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, encrypted);
            c->alert = c->plain[1];
            if (c->alert == AL_CLOSE_NOTIFY) {
                c->peer_closed = 1;
                return NXE_OK; /* the caller sees an empty alert record */
            }
            c->err = NXE_TLS_ALERT;
            return NXE_TLS_ALERT;
        }
        return NXE_OK;
    }
}

/* Next complete handshake message (type, body) into c->hs. */
static int read_handshake(struct tls_conn *c, int encrypted, uint32_t timeout_ms,
                          uint8_t *type, const uint8_t **body, uint32_t *len)
{
    for (;;) {
        if (c->hs_len >= 4) {
            uint32_t mlen = get24(c->hs + 1);
            if (mlen > TLS_HS_MAX)
                return fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, encrypted);
            if (c->hs_len >= 4 + mlen) {
                *type = c->hs[0];
                *body = c->hs + 4;
                *len = mlen;
                return NXE_OK;
            }
        }
        if (c->plain_off < c->plain_len && c->plain_type == CT_HANDSHAKE) {
            uint32_t n = c->plain_len - c->plain_off;
            if (c->hs_len + n > sizeof(c->hs))
                return fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, encrypted);
            memcpy(c->hs + c->hs_len, c->plain + c->plain_off, n);
            c->hs_len += n;
            c->plain_off = c->plain_len;
            continue;
        }
        int r = read_record(c, encrypted, timeout_ms);
        if (r != NXE_OK)
            return r;
        if (c->peer_closed)
            return fail(c, NXE_TLS_PROTOCOL, 0xFF, encrypted);
        if (c->plain_type != CT_HANDSHAKE)
            return fail(c, NXE_TLS_PROTOCOL, AL_UNEXPECTED_MESSAGE, encrypted);
    }
}

/* Drops the handshake message just processed (after hashing it). */
static void consume_handshake(struct tls_conn *c, int hash)
{
    uint32_t mlen = 4 + get24(c->hs + 1);
    if (hash)
        nx_hash_update(&c->transcript, c->hs, mlen);
    memmove(c->hs, c->hs + mlen, c->hs_len - mlen);
    c->hs_len -= mlen;
}

/* ---- ClientHello -------------------------------------------------------------------------- */

static uint32_t ext_header(uint8_t *p, uint32_t type, uint32_t len)
{
    put16(p, type);
    put16(p + 2, len);
    return 4;
}

static int client_hello(struct tls_conn *c, const struct tls_config *cfg, const uint8_t pub[32],
                        uint8_t session_id[32])
{
    uint8_t m[512];
    uint32_t n = 4;
    put16(m + n, 0x0303);
    n += 2;
    if (cfg->random(cfg->random_ctx, m + n, 32) != NXE_OK ||
        cfg->random(cfg->random_ctx, session_id, 32) != NXE_OK)
        return NXE_LOC_ENTROPY;
    n += 32;
    m[n++] = 32;
    memcpy(m + n, session_id, 32);
    n += 32;
    uint32_t suites = cfg->suites ? cfg->suites : TLS_SUITES_ALL;
    uint32_t ns = 0;
    uint32_t spos = n;
    n += 2;
    if (suites & 1u) {
        put16(m + n, TLS_SUITE_AES128_GCM);
        n += 2;
        ns++;
    }
    if (suites & 2u) {
        put16(m + n, TLS_SUITE_CHACHA20);
        n += 2;
        ns++;
    }
    put16(m + spos, ns * 2);
    m[n++] = 1; /* compression methods: null */
    m[n++] = 0;
    uint32_t epos = n;
    n += 2;
    /* server_name */
    uint32_t hl = (uint32_t)cstrlen(cfg->host);
    if (hl == 0 || hl > 253)
        return NXE_LOC_BAD_ARG;
    int is_ip = 1;
    for (uint32_t i = 0; i < hl; i++)
        if (!((cfg->host[i] >= '0' && cfg->host[i] <= '9') || cfg->host[i] == '.'))
            is_ip = 0;
    if (!is_ip) { /* RFC 6066: no IP literals in SNI */
        n += ext_header(m + n, 0, hl + 5);
        put16(m + n, hl + 3);
        m[n + 2] = 0;
        put16(m + n + 3, hl);
        memcpy(m + n + 5, cfg->host, hl);
        n += hl + 5;
    }
    /* supported_groups: x25519 */
    n += ext_header(m + n, 10, 4);
    put16(m + n, 2);
    put16(m + n + 2, 0x001D);
    n += 4;
    /* signature_algorithms */
    static const uint16_t sigs[] = {SIG_ECDSA_P256_SHA256, SIG_ECDSA_P384_SHA384,
                                    SIG_RSA_PSS_SHA256,    SIG_RSA_PSS_SHA384,
                                    SIG_RSA_PSS_SHA512,    SIG_RSA_PKCS1_SHA256,
                                    SIG_RSA_PKCS1_SHA384,  SIG_RSA_PKCS1_SHA512};
    uint32_t nsig = sizeof(sigs) / sizeof(sigs[0]);
    n += ext_header(m + n, 13, 2 + 2 * nsig);
    put16(m + n, 2 * nsig);
    n += 2;
    for (uint32_t i = 0; i < nsig; i++, n += 2)
        put16(m + n, sigs[i]);
    /* ALPN: http/1.1 */
    n += ext_header(m + n, 16, 11);
    put16(m + n, 9);
    m[n + 2] = 8;
    memcpy(m + n + 3, "http/1.1", 8);
    n += 11;
    /* supported_versions: TLS 1.3 */
    n += ext_header(m + n, 43, 3);
    m[n++] = 2;
    put16(m + n, 0x0304);
    n += 2;
    /* key_share: x25519 */
    n += ext_header(m + n, 51, 38);
    put16(m + n, 36);
    put16(m + n + 2, 0x001D);
    put16(m + n + 4, 32);
    memcpy(m + n + 6, pub, 32);
    n += 38;
    put16(m + epos, n - epos - 2);
    m[0] = HS_CLIENT_HELLO;
    put24(m + 1, n - 4);
    nx_hash_update(&c->transcript, m, n);
    return send_record(c, CT_HANDSHAKE, m, n, 0);
}

/* ---- ServerHello -------------------------------------------------------------------------- */

static int server_hello(struct tls_conn *c, const struct tls_config *cfg, const uint8_t *b,
                        uint32_t len, const uint8_t session_id[32], uint8_t peer[32])
{
    if (len < 2 + 32 + 1 + 32 + 2 + 1 + 2 || get16(b) != 0x0303)
        return fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, 0);
    if (memcmp(b + 2, HRR_RANDOM, 32) == 0)
        return fail(c, NXE_TLS_UNSUPPORTED, AL_HANDSHAKE_FAILURE, 0); /* HelloRetryRequest */
    uint32_t p = 34;
    if (b[p] != 32 || memcmp(b + p + 1, session_id, 32) != 0)
        return fail(c, NXE_TLS_PROTOCOL, AL_ILLEGAL_PARAMETER, 0);
    p += 33;
    uint32_t suite = get16(b + p);
    uint32_t offered = cfg->suites ? cfg->suites : TLS_SUITES_ALL;
    if (!((suite == TLS_SUITE_AES128_GCM && (offered & 1u)) ||
          (suite == TLS_SUITE_CHACHA20 && (offered & 2u))))
        return fail(c, NXE_TLS_UNSUPPORTED, AL_ILLEGAL_PARAMETER, 0);
    c->suite = (uint16_t)suite;
    p += 2;
    if (b[p++] != 0)
        return fail(c, NXE_TLS_PROTOCOL, AL_ILLEGAL_PARAMETER, 0);
    uint32_t elen = get16(b + p);
    p += 2;
    if (p + elen != len)
        return fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, 0);
    int have_version = 0, have_share = 0;
    while (p < len) {
        if (len - p < 4)
            return fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, 0);
        uint32_t type = get16(b + p), l = get16(b + p + 2);
        p += 4;
        if (len - p < l)
            return fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, 0);
        if (type == 43) {
            if (l != 2 || get16(b + p) != 0x0304)
                return fail(c, NXE_TLS_UNSUPPORTED, AL_PROTOCOL_VERSION, 0);
            have_version = 1;
        } else if (type == 51) {
            if (l != 36 || get16(b + p) != 0x001D || get16(b + p + 2) != 32)
                return fail(c, NXE_TLS_UNSUPPORTED, AL_ILLEGAL_PARAMETER, 0);
            memcpy(peer, b + p + 4, 32);
            have_share = 1;
        } else {
            return fail(c, NXE_TLS_PROTOCOL, AL_UNSUPPORTED_EXTENSION, 0);
        }
        p += l;
    }
    if (!have_version)
        return fail(c, NXE_TLS_UNSUPPORTED, AL_PROTOCOL_VERSION, 0); /* not TLS 1.3 */
    if (!have_share)
        return fail(c, NXE_TLS_PROTOCOL, AL_ILLEGAL_PARAMETER, 0);
    return NXE_OK;
}

/* ---- encrypted handshake ------------------------------------------------------------------- */

static int encrypted_extensions(struct tls_conn *c, const uint8_t *b, uint32_t len)
{
    if (len < 2 || get16(b) + 2u != len)
        return fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, 1);
    uint32_t p = 2;
    while (p < len) {
        if (len - p < 4)
            return fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, 1);
        uint32_t type = get16(b + p), l = get16(b + p + 2);
        p += 4;
        if (len - p < l)
            return fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, 1);
        if (type == 16) { /* ALPN: must be what we offered */
            if (l != 11 || get16(b + p) != 9 || b[p + 2] != 8 ||
                memcmp(b + p + 3, "http/1.1", 8) != 0)
                return fail(c, NXE_TLS_PROTOCOL, AL_ILLEGAL_PARAMETER, 1);
        }
        p += l;
    }
    return NXE_OK;
}

static int alert_for(int err)
{
    switch (err) {
    case NXE_TLS_CERT_EXPIRED: return AL_CERTIFICATE_EXPIRED;
    case NXE_TLS_CERT_UNTRUSTED: return AL_UNKNOWN_CA;
    case NXE_TLS_BAD_SIGNATURE: return AL_DECRYPT_ERROR;
    case NXE_TLS_CERT_NAME:
    case NXE_TLS_CERT_BAD: return AL_BAD_CERTIFICATE;
    default: return AL_HANDSHAKE_FAILURE;
    }
}

static int certificate(struct tls_conn *c, const struct tls_config *cfg, const uint8_t *b,
                       uint32_t len)
{
    if (len < 4 || b[0] != 0) /* certificate_request_context must be empty */
        return fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, 1);
    uint32_t list = get24(b + 1), p = 4;
    if (4 + list != len || !list)
        return fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, 1);
    c->chain_len = 0;
    while (p < len) {
        if (len - p < 3)
            return fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, 1);
        uint32_t cl = get24(b + p);
        p += 3;
        if (len - p < cl + 2 || !cl)
            return fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, 1);
        if (c->chain_len < TLS_CERTS_MAX) {
            const uint8_t *der = b + p;
            if (c->chain_len == 0) {
                /* the leaf's key is needed after this message is gone */
                if (cl > sizeof(c->leaf_der))
                    return fail(c, NXE_TLS_CERT_BAD, AL_BAD_CERTIFICATE, 1);
                memcpy(c->leaf_der, der, cl);
                der = c->leaf_der;
            }
            int st = nx_x509_parse(der, cl, &c->certs[c->chain_len]);
            if (st != NXE_OK) {
                if (c->chain_len == 0)
                    return fail(c, st, AL_BAD_CERTIFICATE, 1);
                /* an extra certificate we cannot parse is simply not used */
            } else {
                c->chain_len++;
            }
        }
        p += cl;
        uint32_t el = get16(b + p);
        p += 2;
        if (len - p < el)
            return fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, 1);
        p += el; /* per-certificate extensions (OCSP, SCT): ignored */
    }
    struct nx_chain_result res;
    int st = nx_x509_verify_chain(c->certs, c->chain_len, cfg->anchors, cfg->nanchors, cfg->host,
                                  cfg->now, &res);
    c->chain_depth = res.depth;
    memcpy(c->chain_detail, res.detail, sizeof(c->chain_detail));
    if (st != NXE_OK)
        return fail(c, st, (uint8_t)alert_for(st), 1);
    return NXE_OK;
}

static int certificate_verify(struct tls_conn *c, const uint8_t *b, uint32_t len,
                              const uint8_t th[32])
{
    if (len < 4 || get16(b + 2) + 4u != len)
        return fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, 1);
    uint32_t scheme = get16(b);
    const struct nx_cert *leaf = &c->certs[0];
    int alg;
    switch (scheme) {
    case SIG_ECDSA_P256_SHA256:
        alg = leaf->curve == NX_P256 ? NX_SIG_ECDSA_SHA256 : 0;
        break;
    case SIG_ECDSA_P384_SHA384:
        alg = leaf->curve == NX_P384 ? NX_SIG_ECDSA_SHA384 : 0;
        break;
    case SIG_RSA_PSS_SHA256: alg = NX_SIG_RSA_PSS_SHA256; break;
    case SIG_RSA_PSS_SHA384: alg = NX_SIG_RSA_PSS_SHA384; break;
    case SIG_RSA_PSS_SHA512: alg = NX_SIG_RSA_PSS_SHA512; break;
    default: alg = 0; break; /* PKCS #1 v1.5 is not allowed here (RFC 8446 §4.4.3) */
    }
    if (!alg)
        return fail(c, NXE_TLS_UNSUPPORTED, AL_ILLEGAL_PARAMETER, 1);
    c->sig_scheme = (uint16_t)scheme;
    uint8_t content[64 + 34 + 32];
    memset(content, 0x20, 64);
    memcpy(content + 64, "TLS 1.3, server CertificateVerify", 33);
    content[97] = 0;
    memcpy(content + 98, th, 32);
    int st = nx_x509_verify(leaf, alg, content, sizeof(content), b + 4, len - 4);
    if (st != NXE_OK)
        return fail(c, NXE_TLS_BAD_SIGNATURE, AL_DECRYPT_ERROR, 1);
    return NXE_OK;
}

static void finished_mac(const uint8_t secret[32], const uint8_t th[32], uint8_t out[32])
{
    uint8_t fk[32];
    tls_expand_label(secret, "finished", 0, 0, fk, 32);
    nx_hmac(NX_HASH_SHA256, fk, 32, th, 32, out);
    nx_wipe(fk, sizeof(fk));
}

int tls_connect(struct tls_conn *c, const struct tls_io *io, const struct tls_config *cfg)
{
    memset(c, 0, sizeof(*c));
    c->io = *io;
    c->alert = 0xFF;
    nx_hash_init(&c->transcript, NX_HASH_SHA256);
    uint8_t priv[32], pub[32], peer[32], shared[32], session_id[32];
    if (cfg->random(cfg->random_ctx, priv, 32) != NXE_OK)
        return c->err = NXE_LOC_ENTROPY;
    nx_x25519_base(pub, priv);
    int r = client_hello(c, cfg, pub, session_id);
    if (r != NXE_OK) {
        nx_wipe(priv, sizeof(priv));
        return c->err = r;
    }
    c->state = 1;
    uint8_t type;
    const uint8_t *body;
    uint32_t len;
    r = read_handshake(c, 0, cfg->timeout_ms, &type, &body, &len);
    if (r == NXE_OK && type != HS_SERVER_HELLO)
        r = fail(c, NXE_TLS_PROTOCOL, AL_UNEXPECTED_MESSAGE, 0);
    if (r == NXE_OK)
        r = server_hello(c, cfg, body, len, session_id, peer);
    if (r != NXE_OK) {
        nx_wipe(priv, sizeof(priv));
        return c->err = r;
    }
    consume_handshake(c, 1);
    nx_x25519(shared, priv, peer);
    nx_wipe(priv, sizeof(priv));
    uint8_t zero_check = 0;
    for (int i = 0; i < 32; i++)
        zero_check |= shared[i];
    if (!zero_check) /* small-order peer point */
        return fail(c, NXE_TLS_PROTOCOL, AL_ILLEGAL_PARAMETER, 0);
    /* handshake secrets */
    uint8_t zeros[32] = {0}, early[32], derived[32], empty_hash[32], th[32], cs[32], ss[32];
    nx_hkdf_extract(NX_HASH_SHA256, 0, 0, zeros, 32, early);
    nx_hash(NX_HASH_SHA256, "", 0, empty_hash);
    derive_secret(early, "derived", empty_hash, derived);
    nx_hkdf_extract(NX_HASH_SHA256, derived, 32, shared, 32, c->hs_secret);
    nx_wipe(shared, sizeof(shared));
    transcript_hash(c, th);
    derive_secret(c->hs_secret, "c hs traffic", th, cs);
    derive_secret(c->hs_secret, "s hs traffic", th, ss);
    set_keys(c, &c->rd, ss);
    set_keys(c, &c->wr, cs);
    c->state = 2;
    /* {EncryptedExtensions} {Certificate} {CertificateVerify} {Finished} */
    int expect = HS_ENCRYPTED_EXTENSIONS;
    for (;;) {
        r = read_handshake(c, 1, cfg->timeout_ms, &type, &body, &len);
        if (r != NXE_OK)
            return c->err = r;
        if (type == HS_CERTIFICATE_REQUEST)
            return fail(c, NXE_TLS_UNSUPPORTED, AL_HANDSHAKE_FAILURE, 1);
        if (type != expect)
            return fail(c, NXE_TLS_PROTOCOL, AL_UNEXPECTED_MESSAGE, 1);
        if (type == HS_ENCRYPTED_EXTENSIONS) {
            r = encrypted_extensions(c, body, len);
            expect = HS_CERTIFICATE;
        } else if (type == HS_CERTIFICATE) {
            r = certificate(c, cfg, body, len);
            expect = HS_CERTIFICATE_VERIFY;
        } else if (type == HS_CERTIFICATE_VERIFY) {
            transcript_hash(c, th);
            r = certificate_verify(c, body, len, th);
            expect = HS_FINISHED;
        } else { /* Finished */
            uint8_t want[32];
            transcript_hash(c, th);
            finished_mac(ss, th, want);
            if (len != 32 || !nx_ct_equal(want, body, 32))
                return fail(c, NXE_TLS_DECRYPT, AL_DECRYPT_ERROR, 1);
            consume_handshake(c, 1);
            break;
        }
        if (r != NXE_OK)
            return c->err = r;
        consume_handshake(c, 1);
    }
    /* application secrets over the transcript up to the server Finished */
    uint8_t th_sf[32], cap[32], sap[32], fin[36];
    transcript_hash(c, th_sf);
    derive_secret(c->hs_secret, "derived", empty_hash, derived);
    nx_hkdf_extract(NX_HASH_SHA256, derived, 32, zeros, 32, c->master);
    derive_secret(c->master, "c ap traffic", th_sf, cap);
    derive_secret(c->master, "s ap traffic", th_sf, sap);
    /* client Finished under the client handshake keys */
    fin[0] = HS_FINISHED;
    put24(fin + 1, 32);
    finished_mac(cs, th_sf, fin + 4);
    nx_hash_update(&c->transcript, fin, sizeof(fin));
    r = send_record(c, CT_HANDSHAKE, fin, sizeof(fin), 1);
    if (r != NXE_OK)
        return c->err = r;
    set_keys(c, &c->rd, sap);
    set_keys(c, &c->wr, cap);
    nx_wipe(cs, sizeof(cs));
    nx_wipe(ss, sizeof(ss));
    c->state = 3;
    return NXE_OK;
}

int tls_write(struct tls_conn *c, const void *buf, uint32_t len)
{
    if (c->err)
        return c->err;
    if (c->state != 3)
        return NXE_LOC_BAD_ARG;
    const uint8_t *p = buf;
    while (len) {
        uint32_t n = len < TLS_RECORD_MAX ? len : TLS_RECORD_MAX;
        int r = send_record(c, CT_APPDATA, p, n, 1);
        if (r != NXE_OK)
            return c->err = r;
        p += n;
        len -= n;
    }
    return NXE_OK;
}

/* Post-handshake messages: NewSessionTicket (ignored), KeyUpdate. */
static int post_handshake(struct tls_conn *c)
{
    for (;;) {
        if (c->hs_len < 4)
            return NXE_OK;
        uint32_t mlen = get24(c->hs + 1);
        if (mlen > TLS_HS_MAX)
            return fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, 1);
        if (c->hs_len < 4 + mlen)
            return NXE_OK;
        uint8_t type = c->hs[0];
        if (type == HS_KEY_UPDATE) {
            if (mlen != 1 || c->hs[4] > 1)
                return fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, 1);
            int requested = c->hs[4] == 1;
            uint8_t next[32];
            tls_expand_label(c->rd.secret, "traffic upd", 0, 0, next, 32);
            set_keys(c, &c->rd, next);
            c->key_updates++;
            if (requested) {
                uint8_t ku[5] = {HS_KEY_UPDATE, 0, 0, 1, 0};
                int r = send_record(c, CT_HANDSHAKE, ku, sizeof(ku), 1);
                if (r != NXE_OK)
                    return c->err = r;
                tls_expand_label(c->wr.secret, "traffic upd", 0, 0, next, 32);
                set_keys(c, &c->wr, next);
            }
        } else if (type != HS_NEW_SESSION_TICKET) {
            return fail(c, NXE_TLS_PROTOCOL, AL_UNEXPECTED_MESSAGE, 1);
        }
        consume_handshake(c, 0);
    }
}

int tls_read(struct tls_conn *c, void *buf, uint32_t cap, uint32_t timeout_ms)
{
    for (;;) {
        if (c->plain_off < c->plain_len && c->plain_type == CT_APPDATA) {
            uint32_t n = c->plain_len - c->plain_off;
            if (n > cap)
                n = cap;
            memcpy(buf, c->plain + c->plain_off, n);
            c->plain_off += n;
            return (int)n;
        }
        if (c->peer_closed)
            return 0;
        if (c->err)
            return -c->err;
        if (c->state != 3)
            return -NXE_LOC_BAD_ARG;
        int r = read_record(c, 1, timeout_ms);
        if (r != NXE_OK) {
            if (r == NXE_NET_TIMEOUT)
                return -r; /* nothing yet: the connection stays usable */
            if (!c->err)
                c->err = r;
            return -r;
        }
        if (c->peer_closed)
            return 0;
        if (c->plain_type == CT_HANDSHAKE) {
            if (c->hs_len + c->plain_len > sizeof(c->hs))
                return -fail(c, NXE_TLS_PROTOCOL, AL_DECODE_ERROR, 1);
            memcpy(c->hs + c->hs_len, c->plain, c->plain_len);
            c->hs_len += c->plain_len;
            c->plain_len = 0;
            r = post_handshake(c);
            if (r != NXE_OK)
                return -r;
        } else if (c->plain_type != CT_APPDATA) {
            return -fail(c, NXE_TLS_PROTOCOL, AL_UNEXPECTED_MESSAGE, 1);
        }
    }
}

void tls_close(struct tls_conn *c)
{
    if (c->state == 3 && !c->err)
        send_alert(c, AL_CLOSE_NOTIFY, 1);
    c->state = 4;
}
