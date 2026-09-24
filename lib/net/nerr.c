/* Error codes and telemetry classes of the M5 provider path; see nerr.h. */
#include <nanox/nerr.h>

const char *nx_err_name(int e)
{
    switch (e) {
    case NXE_OK: return "ok";
    case NXE_LINK_DOWN: return "link_down";
    case NXE_ARP_TIMEOUT: return "arp_timeout";
    case NXE_DNS_TIMEOUT: return "dns_timeout";
    case NXE_DNS_NOTFOUND: return "dns_notfound";
    case NXE_DNS_BAD_REPLY: return "dns_bad_reply";
    case NXE_CONN_REFUSED: return "conn_refused";
    case NXE_CONN_TIMEOUT: return "conn_timeout";
    case NXE_CONN_RESET: return "conn_reset";
    case NXE_UNACKED_TIMEOUT: return "unacked_timeout";
    case NXE_PEER_CLOSED: return "peer_closed";
    case NXE_NET_IO: return "net_io";
    case NXE_NET_TIMEOUT: return "net_timeout";
    case NXE_TLS_CERT_UNTRUSTED: return "cert_untrusted";
    case NXE_TLS_CERT_EXPIRED: return "cert_expired";
    case NXE_TLS_CERT_NAME: return "cert_name";
    case NXE_TLS_CERT_BAD: return "cert_bad";
    case NXE_TLS_BAD_SIGNATURE: return "bad_signature";
    case NXE_TLS_ALERT: return "tls_alert";
    case NXE_TLS_PROTOCOL: return "tls_protocol";
    case NXE_TLS_DECRYPT: return "tls_decrypt";
    case NXE_TLS_UNSUPPORTED: return "tls_unsupported";
    case NXE_PRV_BAD_REQUEST: return "bad_request";
    case NXE_PRV_AUTH: return "auth";
    case NXE_PRV_NOT_FOUND: return "not_found";
    case NXE_PRV_RATE_LIMIT: return "rate_limit";
    case NXE_PRV_OVERLOADED: return "overloaded";
    case NXE_PRV_SERVER: return "server_error";
    case NXE_PRV_MALFORMED_HTTP: return "malformed_http";
    case NXE_PRV_MALFORMED: return "malformed";
    case NXE_PRV_STREAM_ERROR: return "stream_error";
    case NXE_PRV_TIMEOUT: return "provider_timeout";
    case NXE_PRV_TRUNCATED: return "truncated";
    case NXE_PRV_BAD_TOOL: return "bad_tool";
    case NXE_PRV_NO_ANSWER: return "no_answer";
    case NXE_PRV_TOO_LARGE: return "too_large";
    case NXE_ACT_FAILED: return "action_failed";
    case NXE_ACT_VERIFY_FAILED: return "verify_failed";
    case NXE_ACT_REJECTED: return "action_rejected";
    case NXE_ACT_UNKNOWN: return "outcome_unknown";
    case NXE_LOC_CONFIG: return "config";
    case NXE_LOC_NO_KEY: return "no_key";
    case NXE_LOC_ENTROPY: return "entropy";
    case NXE_LOC_RESOURCES: return "resources";
    case NXE_LOC_INTERNAL: return "internal";
    case NXE_LOC_BAD_ARG: return "bad_arg";
    default: return "unknown";
    }
}

int nx_err_class(int e)
{
    if (e == NXE_OK)
        return NXC_OK;
    if (e >= NXE_LINK_DOWN && e <= NXE_NET_TIMEOUT)
        return NXC_NET;
    if (e >= NXE_TLS_CERT_UNTRUSTED && e <= NXE_TLS_UNSUPPORTED)
        return NXC_TLS;
    if (e >= NXE_PRV_BAD_REQUEST && e <= NXE_PRV_TOO_LARGE)
        return NXC_PROVIDER;
    if (e >= NXE_ACT_FAILED && e <= NXE_ACT_UNKNOWN)
        return NXC_ACTION;
    return NXC_LOCAL;
}

const char *nx_err_class_name(int c)
{
    switch (c) {
    case NXC_OK: return "ok";
    case NXC_NET: return "net";
    case NXC_TLS: return "tls";
    case NXC_PROVIDER: return "provider";
    case NXC_ACTION: return "action";
    case NXC_LOCAL: return "local";
    case NXC_UNCLASSIFIED: return "unclassified";
    default: return "?";
    }
}

int nx_err_retryable(int e)
{
    switch (e) {
    case NXE_LINK_DOWN:
    case NXE_ARP_TIMEOUT:
    case NXE_DNS_TIMEOUT:
    case NXE_CONN_REFUSED:
    case NXE_CONN_TIMEOUT:
    case NXE_CONN_RESET:
    case NXE_UNACKED_TIMEOUT:
    case NXE_PEER_CLOSED:
    case NXE_NET_TIMEOUT:
    case NXE_PRV_RATE_LIMIT:
    case NXE_PRV_OVERLOADED:
    case NXE_PRV_SERVER:
    case NXE_PRV_STREAM_ERROR:
    case NXE_PRV_TIMEOUT:
    case NXE_PRV_TRUNCATED:
        return 1;
    default:
        return 0;
    }
}

int nx_err_from_http(uint32_t status)
{
    if (status >= 200 && status < 300)
        return NXE_OK;
    switch (status) {
    case 401:
    case 403: return NXE_PRV_AUTH;
    case 404: return NXE_PRV_NOT_FOUND;
    case 429: return NXE_PRV_RATE_LIMIT;
    case 529: return NXE_PRV_OVERLOADED;
    default: break;
    }
    if (status >= 500 && status < 600)
        return NXE_PRV_SERVER;
    if (status >= 400 && status < 500)
        return NXE_PRV_BAD_REQUEST;
    return NXE_PRV_MALFORMED_HTTP; /* 1xx after the headers, 3xx: not part of the protocol */
}
