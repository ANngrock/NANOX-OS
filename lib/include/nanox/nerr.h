/*
 * Error codes of the M5 provider path and their telemetry classes
 * (docs/m5-net.md §9).  One code space for every layer, so that a failure
 * keeps the identity of the layer that detected it all the way up to the
 * telemetry record:
 *
 *   net       the network stack (link, ARP, DNS, TCP) could not deliver
 *   tls       the secure channel could not be established or broke
 *   provider  the provider answered, but not with a usable model response
 *             (HTTP error, rate limit, malformed response, stream error,
 *             no response although the request was delivered)
 *   action    the model's response was fine, executing the chosen action
 *             failed (FAILED, VERIFY_FAILED, REJECTED, OUTCOME_UNKNOWN)
 *   local     the guest itself (configuration, key, entropy, resources)
 *
 * Pure code: linked into bin/core and compiled into the host tests.
 */
#ifndef NANOX_LIB_NERR_H
#define NANOX_LIB_NERR_H

#include <stdint.h>

enum nx_err {
    NXE_OK = 0,
    /* net */
    NXE_LINK_DOWN = 1,       /* device reports no link */
    NXE_ARP_TIMEOUT,         /* next hop did not answer ARP */
    NXE_DNS_TIMEOUT,         /* no DNS answer */
    NXE_DNS_NOTFOUND,        /* NXDOMAIN or no A record */
    NXE_DNS_BAD_REPLY,       /* malformed DNS answer */
    NXE_CONN_REFUSED,        /* RST answering our SYN */
    NXE_CONN_TIMEOUT,        /* SYN never answered */
    NXE_CONN_RESET,          /* RST on an established connection */
    NXE_UNACKED_TIMEOUT,     /* data never acknowledged: retransmissions exhausted */
    NXE_PEER_CLOSED,         /* TCP FIN before the response was complete */
    NXE_NET_IO,              /* the device failed */
    NXE_NET_TIMEOUT,         /* waiting for data on a connection whose data is not acked */
    /* tls */
    NXE_TLS_CERT_UNTRUSTED = 32, /* chain does not end at a trust anchor */
    NXE_TLS_CERT_EXPIRED,    /* outside the validity period */
    NXE_TLS_CERT_NAME,       /* host name not in the certificate */
    NXE_TLS_CERT_BAD,        /* malformed or unsupported certificate */
    NXE_TLS_BAD_SIGNATURE,   /* certificate or handshake signature invalid */
    NXE_TLS_ALERT,           /* the server sent a fatal alert */
    NXE_TLS_PROTOCOL,        /* unexpected or malformed handshake message */
    NXE_TLS_DECRYPT,         /* record authentication failed */
    NXE_TLS_UNSUPPORTED,     /* the server chose something not offered/implemented */
    /* provider */
    NXE_PRV_BAD_REQUEST = 64, /* HTTP 400 and other 4xx not listed below */
    NXE_PRV_AUTH,            /* HTTP 401, 403 */
    NXE_PRV_NOT_FOUND,       /* HTTP 404 */
    NXE_PRV_RATE_LIMIT,      /* HTTP 429 */
    NXE_PRV_OVERLOADED,      /* HTTP 529 */
    NXE_PRV_SERVER,          /* HTTP 5xx */
    NXE_PRV_MALFORMED_HTTP,  /* not an HTTP/1.1 response */
    NXE_PRV_MALFORMED,       /* body, JSON or event stream not as the protocol says */
    NXE_PRV_STREAM_ERROR,    /* the event stream reported an error */
    NXE_PRV_TIMEOUT,         /* request delivered (acked), no response in time */
    NXE_PRV_TRUNCATED,       /* the provider closed the stream (close_notify) early */
    NXE_PRV_BAD_TOOL,        /* tool call for an unknown tool or with bad input */
    NXE_PRV_NO_ANSWER,       /* the model did not finish within the step limit */
    NXE_PRV_TOO_LARGE,       /* response larger than the client's buffers */
    /* action */
    NXE_ACT_FAILED = 96,
    NXE_ACT_VERIFY_FAILED,
    NXE_ACT_REJECTED,
    NXE_ACT_UNKNOWN,         /* OUTCOME_UNKNOWN */
    /* local */
    NXE_LOC_CONFIG = 128,    /* provider or network configuration missing/invalid */
    NXE_LOC_NO_KEY,          /* the configured key reference does not resolve */
    NXE_LOC_ENTROPY,         /* entropy source missing or failed its health test */
    NXE_LOC_RESOURCES,       /* out of connections or buffer space */
    NXE_LOC_INTERNAL,
    NXE_LOC_BAD_ARG,
};

enum nx_err_class {
    NXC_OK = 0,
    NXC_NET,
    NXC_TLS,
    NXC_PROVIDER,
    NXC_ACTION,
    NXC_LOCAL,
    NXC_UNCLASSIFIED, /* only the negative control m5-flattel produces it */
};

/* Short stable names ("conn_refused", "rate_limit", ...). */
const char *nx_err_name(int e);
/* Class of an error code; unknown codes are NXC_LOCAL. */
int nx_err_class(int e);
const char *nx_err_class_name(int c);
/* Whether the provider client may repeat a model request after this
 * failure (the request has no effects; retrying an action is never done
 * on this basis). */
int nx_err_retryable(int e);
/* HTTP status -> provider error (0 for 2xx). */
int nx_err_from_http(uint32_t status);

#endif
