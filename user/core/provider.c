/*
 * bin/core, M5: the provider client (docs/m5-net.md §8-9).  One model
 * request is an HTTP/1.1 POST of the Messages protocol over the guest's own
 * TLS 1.3 connection (m5tls.c), answered with an event stream that is
 * assembled by lib/http/messages.c.  The connection is kept open between
 * requests (keep-alive); a connection the provider closed while idle is
 * replaced once, transparently, before any retry is counted.
 *
 * Configuration, all in the store (cfg/<key>), no defaults for the
 * provider's identity:
 *   provider.host        DNS name (SNI and certificate name)      required
 *   provider.port        default 443
 *   provider.model       model identifier sent in the request     required
 *   provider.key_ref     store object holding the key, default secret/provider
 *   provider.timeout_ms  receive timeout of one attempt, default 20000
 *   provider.attempts    attempts of one model request, default 3 (1..5)
 *   provider.max_tokens  default 1024
 *
 * The key goes into the x-api-key header of the encrypted request and
 * nowhere else: it is never printed, never part of an NCI response, and
 * provider.status reports only its reference and length.
 *
 * Every attempt produces one telemetry line on the serial log:
 *   tel ask=<id> step=<n> attempt=<k> phase=model result=<nxe name>
 *       class=<class> http=<status> ms=<t> [retry_in=<ms>] [reconnect=stale]
 */
#include <nanox/http.h>
#include <nanox/m5.h>
#include <nanox/messages.h>
#include <nanox/store.h>
#include <nanox/string.h>
#include <nanox/tls.h>

#include "m5.h"
#include "nanox_user.h"

struct prv_cfg prv;
struct m5_tel m5_tel;

static char api_key[256];
static int cfg_loaded;

static uint32_t parse_dec(const char *s, uint32_t def, uint32_t lo, uint32_t hi, int *bad)
{
    if (!s[0])
        return def;
    uint32_t v = 0, n = 0;
    while (s[n] >= '0' && s[n] <= '9' && n < 9)
        v = v * 10 + (uint32_t)(s[n++] - '0');
    if (s[n] || v < lo || v > hi) {
        *bad = 1;
        return def;
    }
    return v;
}

int prv_load(void)
{
    if (cfg_loaded)
        return prv.err;
    cfg_loaded = 1;
    char v[24];
    int bad = 0;
    memset(&prv, 0, sizeof(prv));
    m5_cfg("provider.host", prv.host, sizeof(prv.host), "");
    m5_cfg("provider.model", prv.model, sizeof(prv.model), "");
    m5_cfg("provider.key_ref", prv.key_ref, sizeof(prv.key_ref), "secret/provider");
    m5_cfg("provider.port", v, sizeof(v), "");
    prv.port = parse_dec(v, 443, 1, 65535, &bad);
    m5_cfg("provider.timeout_ms", v, sizeof(v), "");
    prv.timeout_ms = parse_dec(v, 20000, 100, 120000, &bad);
    m5_cfg("provider.attempts", v, sizeof(v), "");
    prv.attempts = parse_dec(v, 3, 1, 5, &bad);
    m5_cfg("provider.max_tokens", v, sizeof(v), "");
    prv.max_tokens = parse_dec(v, 1024, 16, 32000, &bad);
    if (m5_flags & NX_M5_CORE_NORETRY)
        prv.attempts = 1; /* negative control m5-noretry */
    uint32_t len = 0;
    if (!prv.host[0] || !prv.model[0] || bad) {
        prv.err = NXE_LOC_CONFIG;
    } else if (ps_read_obj(prv.key_ref, api_key, sizeof(api_key) - 1, &len) != ST_OK || !len) {
        prv.err = NXE_LOC_NO_KEY;
    } else {
        /* the key must be a header value: printable ASCII without spaces */
        prv.key_len = len;
        api_key[len] = 0;
        for (uint32_t i = 0; i < len; i++)
            if (api_key[i] <= ' ' || api_key[i] > '~')
                prv.err = NXE_LOC_NO_KEY;
    }
    u_printf("provider config host=%s port=%u model=%s key_ref=%s key=%s timeout_ms=%u"
             " attempts=%u result=%s\n",
             prv.host[0] ? prv.host : "-", prv.port, prv.model[0] ? prv.model : "-",
             prv.key_ref, prv.err == NXE_LOC_NO_KEY ? "missing" : "present", prv.timeout_ms,
             prv.attempts, nx_err_name(prv.err));
    return prv.err;
}

void m5_tel_count(int err)
{
    int c = m5_class(err);
    if (c >= 0 && c < (int)(sizeof(m5_tel.by_class) / sizeof(m5_tel.by_class[0])))
        m5_tel.by_class[c]++;
    if (err != NXE_OK) {
        m5_tel.last_err = err;
        m5_tel.last_class = c;
    }
}

/* ---- one exchange ------------------------------------------------------------------- */

static struct http_resp resp;
static struct sse sse;
static char errbody[1024];
static uint32_t errbody_len;

struct exch {
    struct msg_stream *m;
    int stream_ok; /* 200 with an event stream */
};

static int to_msg(void *ctx, const char *event, const char *data, uint32_t len)
{
    return msg_event((struct msg_stream *)ctx, event, data, len);
}

static int body_sink(void *ctx, const char *data, uint32_t len)
{
    struct exch *x = ctx;
    if (resp.status == 200) {
        if (!x->stream_ok) {
            int is_sse = memcmp(resp.content_type, "text/event-stream", 18) == 0;
            if (!is_sse)
                return NXE_PRV_MALFORMED;
            x->stream_ok = 1;
        }
        return sse_feed(&sse, data, len, to_msg, x->m);
    }
    uint32_t k = len < sizeof(errbody) - errbody_len ? len : sizeof(errbody) - errbody_len;
    memcpy(errbody + errbody_len, data, k);
    errbody_len += k;
    return NXE_OK;
}

static char reqbuf[PRV_BODY_MAX + 1024];

static uint32_t head_len(const char *body_len_text)
{
    struct nci_buf b;
    nb_init(&b, reqbuf, sizeof(reqbuf));
    nb_str(&b, "POST /v1/messages HTTP/1.1\r\nHost: ");
    nb_str(&b, prv.host);
    if (prv.port != 443) {
        nb_char(&b, ':');
        nb_u64(&b, prv.port);
    }
    nb_str(&b, "\r\nuser-agent: nanox-core/m5\r\ncontent-type: application/json\r\n"
               "accept: text/event-stream\r\nanthropic-version: 2023-06-01\r\nx-api-key: ");
    nb_str(&b, api_key);
    nb_str(&b, "\r\ncontent-length: ");
    nb_str(&b, body_len_text);
    nb_str(&b, "\r\n\r\n");
    return b.overflow ? 0 : b.len;
}

/* Classification of a failed receive (docs/m5-net.md §9). */
static int recv_error(int e)
{
    if (e == NXE_NET_TIMEOUT)
        e = m5_tls_unacked() ? NXE_UNACKED_TIMEOUT : NXE_PRV_TIMEOUT;
    if (!net_link_ok() && nx_err_class(e) == NXC_NET)
        e = NXE_LINK_DOWN;
    return e;
}

/* One attempt: NXE_OK with the assembled response in *m, or the error.
 * info->http is the HTTP status (0 before one arrived). */
static int exchange(const char *body, uint32_t blen, struct msg_stream *m, struct prv_info *info)
{
    char lt[12];
    uint32_t n = 0, v = blen;
    char t[12];
    do {
        t[n++] = (char)('0' + v % 10);
        v /= 10;
    } while (v);
    for (uint32_t i = 0; i < n; i++)
        lt[i] = t[n - 1 - i];
    lt[n] = 0;
    uint32_t hl = head_len(lt);
    if (!hl || hl + blen > sizeof(reqbuf))
        return NXE_LOC_RESOURCES;
    memcpy(reqbuf + hl, body, blen);

    for (int round = 0; round < 2; round++) {
        int reused = m5_tls_is_open();
        if (!reused) {
            uint64_t t0 = m5_now_ms();
            int e = m5_tls_open(prv.host, (uint16_t)prv.port, TLS_SUITES_ALL,
                                NET_CONNECT_TIMEOUT_MS);
            info->connect_ms = (uint32_t)(m5_now_ms() - t0);
            if (e != NXE_OK)
                return e;
            m5_tel.connects++;
        }
        int e = m5_tls_write(reqbuf, hl + blen);
        http_resp_init(&resp);
        sse_init(&sse);
        msg_stream_init(m);
        errbody_len = 0;
        struct exch x = {m, 0};
        uint64_t got = 0;
        static char in[4096];
        int done = 0;
        while (e == NXE_OK && !done) {
            int k = m5_tls_read(in, sizeof(in), prv.timeout_ms);
            if (k > 0) {
                got += (uint32_t)k;
                uint32_t used = 0;
                int r = http_feed(&resp, in, (uint32_t)k, &used, body_sink, &x);
                if (r < 0)
                    e = -r;
                else if (r == HTTP_DONE)
                    done = 1;
            } else if (k == 0) { /* close_notify */
                if (http_eof_completes(&resp))
                    done = 1;
                else
                    e = got ? NXE_PRV_TRUNCATED : NXE_PEER_CLOSED;
            } else {
                e = recv_error(-k);
            }
        }
        info->http = http_head_done(&resp) ? resp.status : 0;
        info->retry_after_s = resp.retry_after_s;
        /* a kept-alive connection that turned out closed before anything came
         * back: the provider closed it while idle; one new connection */
        if (e != NXE_OK && reused && !got &&
            (e == NXE_PEER_CLOSED || e == NXE_CONN_RESET || e == NXE_PRV_TRUNCATED)) {
            m5_tls_close(0);
            m5_tel.reconnects++;
            info->reconnected = 1;
            continue;
        }
        if (e != NXE_OK) {
            m5_tls_close(0);
            return e;
        }
        if (resp.conn_close)
            m5_tls_close(1);
        if (resp.status != 200) {
            msg_error_type(errbody, errbody_len, info->error_type, sizeof(info->error_type));
            return nx_err_from_http(resp.status);
        }
        if (!x.stream_ok) {
            m5_tls_close(0);
            return NXE_PRV_MALFORMED; /* 200 without a body */
        }
        e = msg_finish(m);
        if (e != NXE_OK) {
            m5_tls_close(0);
            if (e == NXE_PRV_TRUNCATED)
                e = NXE_PRV_MALFORMED; /* the HTTP body ended, the message did not */
        }
        return e;
    }
    return NXE_PEER_CLOSED;
}

static void wait_ms(uint32_t ms)
{
    uint64_t until = m5_now_ms() + ms;
    for (;;) {
        uint64_t now = m5_now_ms();
        if (now >= until)
            return;
        uint32_t left = (uint32_t)(until - now);
        net_poll(&g_ns, left < 100 ? left : 100);
    }
}

int prv_request(const char *ask, uint32_t step, const char *body, uint32_t blen,
                struct msg_stream *m, struct prv_summary *sum)
{
    int e = NXE_LOC_INTERNAL;
    for (uint32_t attempt = 1; attempt <= prv.attempts; attempt++) {
        struct prv_info info;
        memset(&info, 0, sizeof(info));
        uint64_t t0 = m5_now_ms();
        e = exchange(body, blen, m, &info);
        uint32_t ms = (uint32_t)(m5_now_ms() - t0);
        sum->attempts++;
        sum->http = info.http;
        memcpy(sum->error_type, info.error_type, sizeof(sum->error_type));
        if (info.reconnected)
            sum->reconnects++;
        m5_tel.model_attempts++;
        m5_tel_count(e);
        int retry = e != NXE_OK && nx_err_retryable(e) && attempt < prv.attempts;
        uint32_t delay = 0;
        if (retry) {
            delay = attempt == 1 ? 500u : 1000u;
            if (info.retry_after_s)
                delay = info.retry_after_s > 10 ? 10000u : info.retry_after_s * 1000u;
        }
        char extra[64];
        struct nci_buf xb;
        nb_init(&xb, extra, sizeof(extra));
        if (retry) {
            nb_str(&xb, " retry_in=");
            nb_u64(&xb, delay);
        }
        if (info.reconnected)
            nb_str(&xb, " reconnect=stale");
        if (info.error_type[0] && info.error_type[0] != '-') {
            nb_str(&xb, " error_type=");
            nb_str(&xb, info.error_type);
        }
        u_printf("tel ask=%s step=%u attempt=%u phase=model result=%s class=%s http=%u ms=%u"
                 " in_tokens=%lu out_tokens=%lu%s\n",
                 ask, step, attempt, e == NXE_OK ? "ok" : nx_err_name(e),
                 nx_err_class_name(m5_class(e)), info.http, ms,
                 e == NXE_OK ? m->in_tokens : 0, e == NXE_OK ? m->out_tokens : 0, extra);
        if (!retry)
            return e;
        sum->retries++;
        m5_tel.retries++;
        wait_ms(delay);
    }
    return e;
}

const char *prv_key_state(void)
{
    return prv.err == NXE_LOC_NO_KEY ? "missing" : prv.key_len ? "present" : "-";
}
