/*
 * TLS 1.3 client (RFC 8446) of the M5 provider path.  One version (1.3),
 * cipher suites TLS_AES_128_GCM_SHA256 and TLS_CHACHA20_POLY1305_SHA256,
 * key exchange X25519, server authentication by certificate (RSA-PSS,
 * ECDSA P-256/P-384 in CertificateVerify; RSA PKCS #1 v1.5 and ECDSA in
 * the chain), SNI, ALPN "http/1.1".  No PSK or resumption, no 0-RTT, no
 * client certificates, no HelloRetryRequest (only X25519 is offered: a
 * server that asks for another group is refused).  Certificate
 * verification cannot be switched off: there is no such option.
 *
 * Transport-independent: the caller supplies send/recv (bin/core: its own
 * TCP; host tests: sockets).  Pure code; linked into bin/core and compiled
 * into the host tests and the host tool tlstool.  docs/m5-net.md §6-7.
 */
#ifndef NANOX_LIB_TLS_H
#define NANOX_LIB_TLS_H

#include <stdint.h>

#include <nanox/aead.h>
#include <nanox/hash.h>
#include <nanox/x509.h>

#define TLS_SUITE_AES128_GCM 0x1301u
#define TLS_SUITE_CHACHA20 0x1303u
#define TLS_SUITES_ALL 3u     /* bit 0: AES-128-GCM, bit 1: ChaCha20-Poly1305 */
#define TLS_RECORD_MAX 16384u
#define TLS_HS_MAX 24576u     /* largest handshake message (certificate chains) */
#define TLS_CERTS_MAX 6u

struct tls_io {
    void *ctx;
    /* all of buf: NXE_OK or an error */
    int (*send)(void *ctx, const uint8_t *buf, uint32_t len);
    /* > 0 bytes, 0 at the transport's end of stream, -NXE_* on failure or
     * when nothing arrived within timeout_ms */
    int (*recv)(void *ctx, uint8_t *buf, uint32_t cap, uint32_t timeout_ms);
};

struct tls_config {
    const char *host;                  /* SNI and certificate name */
    const struct nx_cert *anchors;
    uint32_t nanchors;
    int64_t now;                       /* Unix seconds for certificate validity */
    uint32_t suites;                   /* TLS_SUITES_* mask (0: all) */
    int (*random)(void *ctx, void *buf, uint32_t len); /* NXE_OK or NXE_LOC_ENTROPY */
    void *random_ctx;
    uint32_t timeout_ms;               /* per receive during the handshake */
};

struct tls_dir {
    uint8_t secret[32];
    uint8_t key[32], iv[12];
    uint64_t seq;
    struct nx_gcm gcm;
};

struct tls_conn {
    struct tls_io io;
    int state;          /* internal */
    int err;            /* sticky NXE_* */
    uint16_t suite, sig_scheme;
    uint8_t alert;      /* last alert description received (0xFF: none) */
    int peer_closed;    /* close_notify received */
    struct tls_dir rd, wr;
    struct nx_hash transcript;
    uint8_t hs_secret[32], master[32];
    /* receive side: one record at a time */
    uint8_t rec[5 + TLS_RECORD_MAX + 256];
    uint32_t rec_have;
    uint8_t plain[TLS_RECORD_MAX + 256];
    uint32_t plain_off, plain_len;
    uint8_t plain_type;
    /* handshake reassembly */
    uint8_t hs[TLS_HS_MAX + 4];
    uint32_t hs_len;
    /* send side */
    uint8_t out[5 + TLS_RECORD_MAX + 256];
    /* peer certificate summary */
    uint32_t chain_len, chain_depth;
    char chain_detail[48];
    struct nx_cert certs[TLS_CERTS_MAX]; /* valid during the Certificate message only, */
    uint8_t leaf_der[8192];              /* except certs[0], parsed from this copy */
    uint64_t bytes_in, bytes_out, records_in, records_out, key_updates;
};

/* Runs the handshake: NXE_OK, or NXE_TLS_*, NXE_LOC_ENTROPY or the
 * transport's error.  On failure a fatal alert has been sent when that
 * makes sense. */
int tls_connect(struct tls_conn *c, const struct tls_io *io, const struct tls_config *cfg);
/* Sends application data: NXE_OK or an error. */
int tls_write(struct tls_conn *c, const void *buf, uint32_t len);
/* Application data: > 0 bytes; 0 after the peer's close_notify;
 * -NXE_PEER_CLOSED if the transport ended without close_notify
 * (truncation); -NXE_* otherwise (the transport's timeout included). */
int tls_read(struct tls_conn *c, void *buf, uint32_t cap, uint32_t timeout_ms);
/* Sends close_notify (best effort). */
void tls_close(struct tls_conn *c);
const char *tls_suite_name(uint16_t suite);
const char *tls_sig_name(uint16_t scheme);

/* HKDF-Expand-Label of RFC 8446 §7.1 (SHA-256), exposed for host tests. */
void tls_expand_label(const uint8_t *secret, const char *label, const uint8_t *ctx,
                      uint32_t ctx_len, uint8_t *out, uint32_t len);

#endif
