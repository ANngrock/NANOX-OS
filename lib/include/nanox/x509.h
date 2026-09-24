/*
 * X.509 certificates for the M5 TLS client (RFC 5280, subset): DER
 * parsing of what verification needs, signature checks with RSA / ECDSA,
 * validity at a given time, host-name matching (RFC 6125: subjectAltName
 * dNSName and iPAddress only, a wildcard only as the whole left-most
 * label), and path building from the server's list to a configured trust
 * anchor.  Unsupported: CRL/OCSP revocation, name constraints and policies
 * (a certificate with an unknown critical extension is refused), RSA-PSS
 * certificate signatures, other curves.  docs/m5-net.md §6.
 */
#ifndef NANOX_LIB_X509_H
#define NANOX_LIB_X509_H

#include <stdint.h>

struct nx_slice {
    const uint8_t *p;
    uint32_t len;
};

enum nx_sig_alg {
    NX_SIG_NONE = 0,
    NX_SIG_RSA_PKCS1_SHA256,
    NX_SIG_RSA_PKCS1_SHA384,
    NX_SIG_RSA_PKCS1_SHA512,
    NX_SIG_ECDSA_SHA256,
    NX_SIG_ECDSA_SHA384,
};

enum nx_key_type { NX_KEY_NONE = 0, NX_KEY_RSA, NX_KEY_EC };

#define NX_KU_DIGITAL_SIGNATURE 0x80u
#define NX_KU_KEY_CERT_SIGN 0x04u

struct nx_cert {
    struct nx_slice der, tbs, sig, issuer, subject, san;
    int sig_alg;
    int64_t not_before, not_after;
    int key_type, curve;
    struct nx_slice rsa_n, rsa_e, ec_point;
    int has_bc, is_ca, path_len; /* path_len -1: no limit */
    int has_ku;
    uint32_t ku;
    int has_eku, eku_server;
    uint32_t version;
};

/* NXE_OK or NXE_TLS_CERT_BAD. */
int nx_x509_parse(const uint8_t *der, uint32_t len, struct nx_cert *c);
/* 1 if the certificate is valid for the host name or IP literal. */
int nx_x509_host_matches(const struct nx_cert *c, const char *host);
/* Signature of `child` with the key of `issuer`: NXE_OK,
 * NXE_TLS_BAD_SIGNATURE, NXE_TLS_CERT_BAD (unsupported algorithm). */
int nx_x509_check_signature(const struct nx_cert *child, const struct nx_cert *issuer);
/* Verifies a signature over `data` with the certificate's key.  alg: the
 * nx_sig_alg of the signature (certificate or TLS CertificateVerify, where
 * NX_SIG_* plus the PSS variants below apply). */
#define NX_SIG_RSA_PSS_SHA256 16
#define NX_SIG_RSA_PSS_SHA384 17
#define NX_SIG_RSA_PSS_SHA512 18
int nx_x509_verify(const struct nx_cert *key, int alg, const uint8_t *data, uint32_t len,
                   const uint8_t *sig, uint32_t sig_len);

#define NX_CHAIN_MAX 6u

struct nx_chain_result {
    int err;          /* NXE_OK or the first failure */
    uint32_t depth;   /* certificates in the path, anchor included */
    int anchor;       /* index of the trust anchor that ended the path */
    char detail[48];
};

/* Validates the server's certificates (certs[0] is the leaf) against the
 * anchors at time `now` (Unix seconds) for `host`. */
int nx_x509_verify_chain(const struct nx_cert *certs, uint32_t ncerts,
                         const struct nx_cert *anchors, uint32_t nanchors, const char *host,
                         int64_t now, struct nx_chain_result *res);

/* DER helpers shared with the TLS client: one TLV at *p (tag checked):
 * content slice, advances *p; 0 on malformed input. */
int nx_der_get(const uint8_t **p, const uint8_t *end, uint8_t tag, struct nx_slice *out);
/* ECDSA-Sig-Value ::= SEQUENCE { r INTEGER, s INTEGER }. */
int nx_der_ecdsa_sig(const uint8_t *sig, uint32_t len, struct nx_slice *r, struct nx_slice *s);

#endif
