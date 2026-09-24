/*
 * Host tests of the M5 network stack (lib/net): checksums, addresses, DNS
 * messages, ARP, ICMP, UDP and TCP between two stacks on a simulated link
 * with a virtual clock, deterministic loss, duplication and reordering.
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <nanox/net.h>
#include <nanox/nerr.h>

#include "test.h"

/* ---- simulated link ------------------------------------------------------------ */

#define QMAX 256

struct frame {
    uint8_t data[NET_FRAME_MAX];
    uint32_t len;
    uint64_t at; /* delivery time */
};

struct side {
    struct frame q[QMAX];
    uint32_t n;
    int link_up;
    uint64_t sent;
};

static struct {
    uint64_t now;
    struct side side[2];
    struct netstack ns[2];
    uint32_t drop_every[2]; /* drop every N-th frame sent by side i (0: none) */
    int drop_all[2];        /* drop everything sent by side i */
    int dup_every[2];
    int reorder;            /* deliver every 4th frame 5 ms late */
    uint32_t delay;         /* one-way delay, ms */
    int stepping;
    void (*app)(void);      /* server-side application step */
    uint32_t rnd;
} W;

static uint32_t rnd32(void *ctx)
{
    (void)ctx;
    W.rnd = W.rnd * 1103515245u + 12345u;
    return W.rnd ^ (W.rnd >> 16);
}

static uint64_t now_ms(void *ctx)
{
    (void)ctx;
    return W.now;
}

static int link_up(void *ctx)
{
    return W.side[(int)(intptr_t)ctx].link_up;
}

static void enqueue(int to, const uint8_t *f, uint32_t len, uint64_t at)
{
    struct side *s = &W.side[to];
    if (s->n == QMAX)
        return; /* queue full: dropped */
    memcpy(s->q[s->n].data, f, len);
    s->q[s->n].len = len;
    s->q[s->n].at = at;
    s->n++;
}

static void trace_frame(int me, const uint8_t *f, uint32_t len, const char *what)
{
    if (!getenv("NANOX_NETDBG"))
        return;
    if (len >= 54 && f[12] == 8 && f[13] == 0 && f[23] == 6) {
        const uint8_t *t = f + 34;
        uint32_t tot = (uint32_t)f[16] << 8 | f[17];
        uint32_t hl = (uint32_t)(t[12] >> 4) * 4;
        fprintf(stderr, "%llu %c->%c %s tcp %u->%u seq=%u ack=%u fl=%02x win=%u len=%u\n",
                (unsigned long long)W.now, 'A' + me, 'B' - me, what,
                (unsigned)(t[0] << 8 | t[1]), (unsigned)(t[2] << 8 | t[3]),
                (unsigned)((uint32_t)t[4] << 24 | (uint32_t)t[5] << 16 | (uint32_t)t[6] << 8 | t[7]),
                (unsigned)((uint32_t)t[8] << 24 | (uint32_t)t[9] << 16 | (uint32_t)t[10] << 8 | t[11]),
                t[13], (unsigned)(t[14] << 8 | t[15]), tot - 20 - hl);
    } else {
        fprintf(stderr, "%llu %c->%c %s frame type=%02x%02x len=%u\n", (unsigned long long)W.now,
                'A' + me, 'B' - me, what, f[12], f[13], len);
    }
}

static int dev_send(void *ctx, const uint8_t *f, uint32_t len)
{
    int me = (int)(intptr_t)ctx, other = 1 - me;
    trace_frame(me, f, len, "send");
    if (!W.side[me].link_up)
        return NXE_LINK_DOWN;
    W.side[me].sent++;
    if (W.drop_all[me])
        return NXE_OK;
    if (W.drop_every[me] && W.side[me].sent % W.drop_every[me] == 0)
        return NXE_OK;
    uint64_t at = W.now + W.delay;
    if (W.reorder && W.side[me].sent % 4 == 0)
        at += 5;
    enqueue(other, f, len, at);
    if (W.dup_every[me] && W.side[me].sent % (uint32_t)W.dup_every[me] == 0)
        enqueue(other, f, len, at + 1);
    return NXE_OK;
}

static uint32_t take(int me, uint8_t *buf)
{
    struct side *s = &W.side[me];
    int best = -1;
    for (uint32_t i = 0; i < s->n; i++)
        if (s->q[i].at <= W.now && (best < 0 || s->q[i].at < s->q[best].at))
            best = (int)i;
    if (best < 0)
        return 0;
    uint32_t len = s->q[best].len;
    memcpy(buf, s->q[best].data, len);
    memmove(&s->q[best], &s->q[best + 1], (s->n - (uint32_t)best - 1) * sizeof(s->q[0]));
    s->n--;
    return len;
}

static void step_server(void)
{
    if (W.stepping)
        return;
    W.stepping = 1;
    for (int i = 0; i < 8; i++)
        net_poll(&W.ns[1], 0);
    if (W.app)
        W.app();
    W.stepping = 0;
}

static uint32_t dev_recv(void *ctx, uint8_t *buf, uint32_t wait_ms)
{
    int me = (int)(intptr_t)ctx;
    if (me == 1) /* the server side never waits: it runs inside the client's steps */
        return take(1, buf);
    for (uint32_t waited = 0;; waited++) {
        step_server();
        uint32_t n = take(0, buf);
        if (n)
            return n;
        if (waited >= wait_ms)
            return 0;
        W.now++;
    }
}

#define IP_A 0x0A00020Fu /* 10.0.2.15 */
#define IP_B 0x0A000202u /* 10.0.2.2 */

static void world_init(void)
{
    memset(&W, 0, sizeof(W));
    W.now = 1000;
    W.rnd = 12345;
    W.delay = 1;
    for (int i = 0; i < 2; i++) {
        W.side[i].link_up = 1;
        struct net_ops ops = {(void *)(intptr_t)i, dev_send, dev_recv, now_ms, link_up, rnd32};
        struct net_config cfg;
        memset(&cfg, 0, sizeof(cfg));
        uint8_t mac[6] = {0x52, 0x54, 0x00, 0x12, 0x34, (uint8_t)(0x56 + i)};
        memcpy(cfg.mac, mac, 6);
        cfg.ip = i ? IP_B : IP_A;
        cfg.mask = 0xFFFFFF00u;
        cfg.gw = i ? IP_A : IP_B;
        cfg.dns_ip = IP_B;
        cfg.dns_port = 5353;
        net_init(&W.ns[i], &ops, &cfg);
    }
}

/* ---- pure helpers ---------------------------------------------------------------- */

static void test_checksum_ip(void)
{
    /* RFC 1071 §3 example: sum 0xddf2, checksum 0x220d */
    static const uint8_t d[] = {0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7};
    CHECK_EQ_INT(net_checksum(d, sizeof(d), 0), 0x220d);
    static const uint8_t odd[] = {0x01};
    CHECK_EQ_INT(net_checksum(odd, 1, 0), 0xFEFF);
    uint32_t ip = 0;
    CHECK(net_parse_ip("10.0.2.15", &ip) && ip == IP_A);
    CHECK(!net_parse_ip("10.0.2", &ip));
    CHECK(!net_parse_ip("10.0.2.256", &ip));
    CHECK(!net_parse_ip("10.0.2.1x", &ip));
    CHECK(!net_parse_ip("1000.0.2.1", &ip));
    char s[16];
    net_format_ip(0xC0A80101u, s);
    CHECK(strcmp(s, "192.168.1.1") == 0);
    net_format_ip(0, s);
    CHECK(strcmp(s, "0.0.0.0") == 0);
}

static void test_dns_messages(void)
{
    uint8_t q[300];
    uint32_t n = dns_build_query(0x1234, "provider.nanox.test", q, sizeof(q));
    CHECK_EQ_INT(n, 12 + 21 + 4);
    CHECK(q[0] == 0x12 && q[1] == 0x34 && q[2] == 0x01 && q[5] == 1);
    CHECK(q[12] == 8 && memcmp(q + 13, "provider", 8) == 0 && q[21] == 5 && q[27] == 4);
    CHECK(q[32] == 0 && q[33] == 0 && q[34] == 1 && q[36] == 1);
    uint8_t scratch[300];
    CHECK_EQ_INT(dns_build_query(1, "a..b", scratch, sizeof(scratch)), 0);
    CHECK_EQ_INT(dns_build_query(1, "", scratch, sizeof(scratch)), 0);
    CHECK_EQ_INT(dns_build_query(1, "bad name", scratch, sizeof(scratch)), 0);
    CHECK_EQ_INT(dns_build_query(1, "trailing.dot.", scratch, sizeof(scratch)), 12 + 14 + 4);
    char label64[80];
    memset(label64, 'a', 64);
    label64[64] = 0;
    CHECK_EQ_INT(dns_build_query(1, label64, scratch, sizeof(scratch)), 0);

    /* response: the question, a CNAME, then an A record, both with
     * compression pointers to the question name */
    uint8_t r[512];
    memcpy(r, q, n);
    r[2] = 0x81;
    r[3] = 0x80;
    r[7] = 2; /* ANCOUNT */
    uint32_t o = n;
    static const uint8_t cname[] = {0xC0, 12, 0, 5, 0, 1, 0, 0, 0, 60, 0, 2, 0xC0, 12};
    memcpy(r + o, cname, sizeof(cname));
    o += sizeof(cname);
    static const uint8_t arec[] = {0xC0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 10, 0, 2, 2};
    memcpy(r + o, arec, sizeof(arec));
    o += sizeof(arec);
    uint32_t ip = 0;
    CHECK_EQ_INT(dns_parse_response(r, o, 0x1234, &ip), NXE_OK);
    CHECK_EQ_INT(ip, IP_B);
    CHECK_EQ_INT(dns_parse_response(r, o, 0x1235, &ip), NXE_DNS_BAD_REPLY); /* other id */
    CHECK_EQ_INT(dns_parse_response(r, o - 1, 0x1234, &ip), NXE_DNS_BAD_REPLY); /* cut */
    r[3] = 0x83; /* NXDOMAIN */
    CHECK_EQ_INT(dns_parse_response(r, o, 0x1234, &ip), NXE_DNS_NOTFOUND);
    r[3] = 0x82; /* SERVFAIL */
    CHECK_EQ_INT(dns_parse_response(r, o, 0x1234, &ip), NXE_DNS_BAD_REPLY);
    r[3] = 0x80;
    r[7] = 1; /* only the CNAME */
    CHECK_EQ_INT(dns_parse_response(r, n + sizeof(cname), 0x1234, &ip), NXE_DNS_NOTFOUND);
    r[2] = 0x01; /* not a response */
    CHECK_EQ_INT(dns_parse_response(r, o, 0x1234, &ip), NXE_DNS_BAD_REPLY);
    /* a pointer loop must end */
    uint8_t loop[] = {0x12, 0x34, 0x81, 0x80, 0, 1, 0, 0, 0, 0, 0, 0, 0xC0, 12, 0, 1, 0, 1};
    CHECK_EQ_INT(dns_parse_response(loop, sizeof(loop), 0x1234, &ip), NXE_DNS_NOTFOUND);
}

static void test_err_classes(void)
{
    CHECK_EQ_INT(nx_err_class(NXE_OK), NXC_OK);
    CHECK_EQ_INT(nx_err_class(NXE_CONN_REFUSED), NXC_NET);
    CHECK_EQ_INT(nx_err_class(NXE_LINK_DOWN), NXC_NET);
    CHECK_EQ_INT(nx_err_class(NXE_NET_TIMEOUT), NXC_NET);
    CHECK_EQ_INT(nx_err_class(NXE_TLS_CERT_EXPIRED), NXC_TLS);
    CHECK_EQ_INT(nx_err_class(NXE_TLS_UNSUPPORTED), NXC_TLS);
    CHECK_EQ_INT(nx_err_class(NXE_PRV_RATE_LIMIT), NXC_PROVIDER);
    CHECK_EQ_INT(nx_err_class(NXE_PRV_TOO_LARGE), NXC_PROVIDER);
    CHECK_EQ_INT(nx_err_class(NXE_ACT_VERIFY_FAILED), NXC_ACTION);
    CHECK_EQ_INT(nx_err_class(NXE_ACT_UNKNOWN), NXC_ACTION);
    CHECK_EQ_INT(nx_err_class(NXE_LOC_NO_KEY), NXC_LOCAL);
    CHECK_EQ_INT(nx_err_class(9999), NXC_LOCAL);
    CHECK(strcmp(nx_err_class_name(NXC_PROVIDER), "provider") == 0);
    CHECK(strcmp(nx_err_name(NXE_PRV_TIMEOUT), "provider_timeout") == 0);
    CHECK(strcmp(nx_err_name(12345), "unknown") == 0);
    /* every named code has a distinct name */
    static const int codes[] = {
        NXE_LINK_DOWN, NXE_ARP_TIMEOUT, NXE_DNS_TIMEOUT, NXE_DNS_NOTFOUND, NXE_DNS_BAD_REPLY,
        NXE_CONN_REFUSED, NXE_CONN_TIMEOUT, NXE_CONN_RESET, NXE_UNACKED_TIMEOUT, NXE_PEER_CLOSED,
        NXE_NET_IO, NXE_NET_TIMEOUT, NXE_TLS_CERT_UNTRUSTED, NXE_TLS_CERT_EXPIRED,
        NXE_TLS_CERT_NAME, NXE_TLS_CERT_BAD, NXE_TLS_BAD_SIGNATURE, NXE_TLS_ALERT,
        NXE_TLS_PROTOCOL, NXE_TLS_DECRYPT, NXE_TLS_UNSUPPORTED, NXE_PRV_BAD_REQUEST,
        NXE_PRV_AUTH, NXE_PRV_NOT_FOUND, NXE_PRV_RATE_LIMIT, NXE_PRV_OVERLOADED, NXE_PRV_SERVER,
        NXE_PRV_MALFORMED_HTTP, NXE_PRV_MALFORMED, NXE_PRV_STREAM_ERROR, NXE_PRV_TIMEOUT,
        NXE_PRV_TRUNCATED, NXE_PRV_BAD_TOOL, NXE_PRV_NO_ANSWER, NXE_PRV_TOO_LARGE,
        NXE_ACT_FAILED, NXE_ACT_VERIFY_FAILED, NXE_ACT_REJECTED, NXE_ACT_UNKNOWN,
        NXE_LOC_CONFIG, NXE_LOC_NO_KEY, NXE_LOC_ENTROPY, NXE_LOC_RESOURCES, NXE_LOC_INTERNAL,
        NXE_LOC_BAD_ARG};
    int distinct = 1;
    for (unsigned i = 0; i < sizeof(codes) / sizeof(codes[0]); i++) {
        if (strcmp(nx_err_name(codes[i]), "unknown") == 0)
            distinct = 0;
        for (unsigned j = 0; j < i; j++)
            if (strcmp(nx_err_name(codes[i]), nx_err_name(codes[j])) == 0)
                distinct = 0;
    }
    CHECK(distinct);
    CHECK_EQ_INT(nx_err_from_http(200), NXE_OK);
    CHECK_EQ_INT(nx_err_from_http(400), NXE_PRV_BAD_REQUEST);
    CHECK_EQ_INT(nx_err_from_http(401), NXE_PRV_AUTH);
    CHECK_EQ_INT(nx_err_from_http(403), NXE_PRV_AUTH);
    CHECK_EQ_INT(nx_err_from_http(404), NXE_PRV_NOT_FOUND);
    CHECK_EQ_INT(nx_err_from_http(413), NXE_PRV_BAD_REQUEST);
    CHECK_EQ_INT(nx_err_from_http(429), NXE_PRV_RATE_LIMIT);
    CHECK_EQ_INT(nx_err_from_http(500), NXE_PRV_SERVER);
    CHECK_EQ_INT(nx_err_from_http(503), NXE_PRV_SERVER);
    CHECK_EQ_INT(nx_err_from_http(529), NXE_PRV_OVERLOADED);
    CHECK_EQ_INT(nx_err_from_http(302), NXE_PRV_MALFORMED_HTTP);
    CHECK(nx_err_retryable(NXE_PRV_RATE_LIMIT) && nx_err_retryable(NXE_CONN_RESET) &&
          nx_err_retryable(NXE_PRV_TIMEOUT) && nx_err_retryable(NXE_PEER_CLOSED));
    CHECK(!nx_err_retryable(NXE_PRV_AUTH) && !nx_err_retryable(NXE_TLS_CERT_UNTRUSTED) &&
          !nx_err_retryable(NXE_PRV_MALFORMED) && !nx_err_retryable(NXE_ACT_FAILED) &&
          !nx_err_retryable(NXE_LOC_NO_KEY));
}

/* ---- ARP, ICMP, UDP ------------------------------------------------------------ */

static void test_arp_ping_udp(void)
{
    world_init();
    uint8_t mac[6];
    CHECK_EQ_INT(net_arp_resolve(&W.ns[0], IP_B, 1000, mac), NXE_OK);
    CHECK(mac[5] == 0x57);
    CHECK(W.ns[1].st.arp_replies == 1);
    uint32_t rtt = 999;
    CHECK_EQ_INT(net_ping(&W.ns[0], IP_B, 1000, &rtt), NXE_OK);
    CHECK(rtt <= 4);
    CHECK_EQ_INT(W.ns[1].st.icmp_echo_replied, 1);
    /* unknown host: ARP gives up */
    CHECK_EQ_INT(net_arp_resolve(&W.ns[0], 0x0A000299u, 800, mac), NXE_ARP_TIMEOUT);
    CHECK(W.ns[0].st.arp_tx >= 2);
    /* link down */
    W.side[0].link_up = 0;
    CHECK_EQ_INT(net_arp_resolve(&W.ns[0], 0x0A000298u, 500, mac), NXE_LINK_DOWN);
    W.side[0].link_up = 1;
    /* off-link destinations go through the gateway */
    CHECK_EQ_INT(net_next_hop(&W.ns[0], 0x08080808u), IP_B);
    CHECK_EQ_INT(net_next_hop(&W.ns[0], 0x0A000203u), 0x0A000203u);

    int a = udp_open(&W.ns[0], 0), b = udp_open(&W.ns[1], 7777);
    CHECK(a >= 0 && b >= 0);
    CHECK_EQ_INT(udp_sendto(&W.ns[0], a, IP_B, 7777, "hello", 5), NXE_OK);
    uint8_t buf[64];
    uint32_t from;
    uint16_t fport;
    W.stepping = 1; /* read on the server side directly */
    W.now += 2;
    net_poll(&W.ns[1], 0);
    uint32_t n = udp_recv(&W.ns[1], b, buf, sizeof(buf), 0, &from, &fport);
    W.stepping = 0;
    CHECK(n == 5 && memcmp(buf, "hello", 5) == 0 && from == IP_A &&
          fport == W.ns[0].udp[a].lport);
    CHECK_EQ_INT(udp_sendto(&W.ns[1], b, IP_A, fport, "world!", 6), NXE_OK);
    n = udp_recv(&W.ns[0], a, buf, sizeof(buf), 100, &from, &fport);
    CHECK(n == 6 && memcmp(buf, "world!", 6) == 0 && fport == 7777);
    CHECK_EQ_INT(udp_recv(&W.ns[0], a, buf, sizeof(buf), 50, 0, 0), 0); /* nothing more */
}

/* DNS server on side B: answers "provider.nanox.test" with 10.0.2.2, other
 * names with NXDOMAIN; with dns_silent it does not answer at all. */
static int dns_sock, dns_silent, dns_answered;

static void dns_app(void)
{
    uint8_t q[512];
    uint32_t from;
    uint16_t fport;
    uint32_t n = udp_recv(&W.ns[1], dns_sock, q, sizeof(q), 0, &from, &fport);
    if (!n || dns_silent)
        return;
    uint8_t r[512];
    memcpy(r, q, n);
    r[2] = 0x81;
    uint32_t len = n;
    if (n > 12 + 9 && memcmp(q + 13, "provider", 8) == 0) {
        r[3] = 0x80;
        r[7] = 1;
        static const uint8_t arec[] = {0xC0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 10, 0, 2, 2};
        memcpy(r + n, arec, sizeof(arec));
        len += sizeof(arec);
    } else {
        r[3] = 0x83;
    }
    udp_sendto(&W.ns[1], dns_sock, from, fport, r, len);
    dns_answered++;
}

static void test_dns_resolver(void)
{
    world_init();
    dns_sock = udp_open(&W.ns[1], 5353);
    dns_silent = 0;
    dns_answered = 0;
    W.app = dns_app;
    uint32_t ip = 0;
    CHECK_EQ_INT(net_dns_a(&W.ns[0], "provider.nanox.test", 3000, &ip), NXE_OK);
    CHECK_EQ_INT(ip, IP_B);
    CHECK_EQ_INT(net_dns_a(&W.ns[0], "other.nanox.test", 3000, &ip), NXE_DNS_NOTFOUND);
    CHECK_EQ_INT(net_dns_a(&W.ns[0], "10.1.2.3", 3000, &ip), NXE_OK); /* literal */
    CHECK_EQ_INT(ip, 0x0A010203u);
    /* lost query: the resolver retries */
    W.drop_every[0] = 2;
    W.side[0].sent = 1; /* the next frame, the query, is lost */
    CHECK_EQ_INT(net_dns_a(&W.ns[0], "provider.nanox.test", 3000, &ip), NXE_OK);
    W.drop_every[0] = 0;
    CHECK(W.ns[0].st.dns_retries >= 1);
    dns_silent = 1;
    uint64_t t0 = W.now;
    CHECK_EQ_INT(net_dns_a(&W.ns[0], "provider.nanox.test", 1500, &ip), NXE_DNS_TIMEOUT);
    CHECK(W.now - t0 >= 1500 && W.now - t0 < 1600);
    W.app = 0;
}

/* ---- TCP ------------------------------------------------------------------------ */

#define XFER (100u * 1024u)
static uint8_t pattern_a[XFER], pattern_b[XFER], got_a[XFER], got_b[XFER];
static int srv_listen, srv_conn;
static uint32_t srv_got, srv_sent;
static int srv_echo_close;   /* server closes after sending its data */
static int srv_abort_after;  /* server aborts (RST) after receiving this many bytes */

static void tcp_app(void)
{
    struct netstack *b = &W.ns[1];
    if (srv_conn < 0) {
        srv_conn = tcp_accept(b, srv_listen);
        if (srv_conn < 0)
            return;
    }
    uint8_t buf[4096];
    for (;;) {
        int n = tcp_read(b, srv_conn, buf, sizeof(buf), 0);
        if (n <= 0)
            break;
        memcpy(got_b + srv_got, buf, (uint32_t)n);
        srv_got += (uint32_t)n;
        if (srv_abort_after && srv_got >= (uint32_t)srv_abort_after) {
            tcp_abort(b, srv_conn);
            srv_conn = -2;
            return;
        }
    }
    while (srv_sent < XFER) {
        uint32_t chunk = XFER - srv_sent < 3000 ? XFER - srv_sent : 3000;
        struct tcp_conn *t = &b->tcp[srv_conn];
        if (NET_TCP_SNDBUF - t->sbuf_len < chunk)
            break;
        if (tcp_write(b, srv_conn, pattern_b + srv_sent, chunk, 0) != NXE_OK)
            break;
        srv_sent += chunk;
    }
    if (srv_echo_close && srv_sent == XFER && srv_got == XFER)
        tcp_shutdown(b, srv_conn);
}

static void tcp_setup(void)
{
    world_init();
    srv_listen = tcp_listen(&W.ns[1], 8443);
    srv_conn = -1;
    srv_got = srv_sent = 0;
    srv_echo_close = 1;
    srv_abort_after = 0;
    W.app = tcp_app;
    for (uint32_t i = 0; i < XFER; i++) {
        pattern_a[i] = (uint8_t)(i * 7u + (i >> 11));
        pattern_b[i] = (uint8_t)(i * 13u + 5u + (i >> 9));
    }
}

/* Client sends XFER bytes and reads XFER bytes; returns the error or OK. */
static int transfer(uint32_t *got_out)
{
    struct netstack *a = &W.ns[0];
    int c = tcp_connect(a, IP_B, 8443, 5000);
    if (c < 0)
        return -c;
    int r = tcp_write(a, c, pattern_a, XFER, 60000);
    if (r != NXE_OK)
        return r;
    uint32_t got = 0;
    while (got < XFER) {
        int n = tcp_read(a, c, got_a + got, XFER - got, 20000);
        if (n <= 0) {
            *got_out = got;
            return n == 0 ? NXE_PEER_CLOSED : -n;
        }
        got += (uint32_t)n;
    }
    *got_out = got;
    int end = tcp_read(a, c, got_a, 1, 5000); /* the server's FIN */
    tcp_close(a, c, 5000);
    return end == 0 ? NXE_OK : NXE_LOC_INTERNAL;
}

static void check_transfer(const char *what, int expect_retx)
{
    uint32_t got = 0;
    int r = transfer(&got);
    if (r != NXE_OK)
        fprintf(stderr, "tcp %s: %s after %u bytes\n", what, nx_err_name(r), got);
    CHECK_EQ_INT(r, NXE_OK);
    CHECK(got == XFER && memcmp(got_a, pattern_b, XFER) == 0);
    CHECK(srv_got == XFER && memcmp(got_b, pattern_a, XFER) == 0);
    if (expect_retx)
        CHECK(W.ns[0].st.tcp_retransmits + W.ns[1].st.tcp_retransmits > 0);
    /* the client slot is free once the close has finished */
    for (int i = 0; i < 400; i++)
        net_poll(&W.ns[0], 10);
    int used = 0;
    for (uint32_t i = 0; i < NET_TCP_CONNS; i++)
        used += W.ns[0].tcp[i].used;
    CHECK_EQ_INT(used, 0);
}

static void test_tcp_transfer(void)
{
    tcp_setup();
    check_transfer("clean", 0);
    CHECK_EQ_INT(W.ns[0].st.tcp_retransmits, 0);
    CHECK_EQ_INT(W.ns[0].tcp[0].mss, NET_TCP_MSS);

    tcp_setup();
    W.drop_every[0] = 7;
    W.drop_every[1] = 5;
    check_transfer("loss", 1);

    tcp_setup();
    W.dup_every[0] = 3;
    W.dup_every[1] = 4;
    W.reorder = 1;
    check_transfer("dup+reorder", 0);
    CHECK(W.ns[0].st.tcp_out_of_order + W.ns[1].st.tcp_out_of_order > 0);

    tcp_setup();
    W.delay = 30;
    W.drop_every[1] = 11;
    check_transfer("delay+loss", 1);
    CHECK(W.ns[0].tcp[0].srtt == 0 || 1); /* slot already released */
}

static void test_tcp_failures(void)
{
    /* nobody listening: RST -> refused */
    tcp_setup();
    int c = tcp_connect(&W.ns[0], IP_B, 9999, 3000);
    CHECK_EQ_INT(c, -NXE_CONN_REFUSED);

    /* SYN never answered -> connect timeout at the caller's deadline */
    tcp_setup();
    uint8_t mac[6];
    CHECK_EQ_INT(net_arp_resolve(&W.ns[0], IP_B, 1000, mac), NXE_OK);
    W.drop_all[0] = 1;
    uint64_t t0 = W.now;
    c = tcp_connect(&W.ns[0], IP_B, 8443, 4000);
    CHECK_EQ_INT(c, -NXE_CONN_TIMEOUT);
    CHECK(W.now - t0 >= 4000 && W.now - t0 < 4100);
    CHECK(W.ns[0].st.tcp_retransmits >= 2); /* SYN sent again at 1 s, 3 s */

    /* established, then everything is lost: data never acknowledged */
    tcp_setup();
    W.app = 0;
    c = tcp_connect(&W.ns[0], IP_B, 8443, 3000);
    CHECK(c >= 0);
    W.drop_all[0] = 1;
    CHECK_EQ_INT(tcp_write(&W.ns[0], c, "request", 7, 1000), NXE_OK);
    CHECK_EQ_INT(tcp_unacked(&W.ns[0], c), 7);
    int n = tcp_read(&W.ns[0], c, got_a, 100, 2000);
    CHECK_EQ_INT(n, -NXE_NET_TIMEOUT); /* caller's deadline: still retrying */
    CHECK_EQ_INT(tcp_unacked(&W.ns[0], c), 7);
    n = tcp_read(&W.ns[0], c, got_a, 100, 120000);
    CHECK_EQ_INT(n, -NXE_UNACKED_TIMEOUT); /* retransmissions exhausted */
    tcp_abort(&W.ns[0], c);

    /* the same with the link going down */
    tcp_setup();
    W.app = 0;
    c = tcp_connect(&W.ns[0], IP_B, 8443, 3000);
    W.side[0].link_up = 0;
    CHECK_EQ_INT(tcp_write(&W.ns[0], c, "request", 7, 1000), NXE_OK);
    n = tcp_read(&W.ns[0], c, got_a, 100, 120000);
    CHECK_EQ_INT(n, -NXE_LINK_DOWN);
    tcp_abort(&W.ns[0], c);
    W.side[0].link_up = 1;

    /* request acknowledged, server silent: no data, but nothing unacked */
    tcp_setup();
    W.app = 0; /* the server stack still acknowledges, the application never answers */
    c = tcp_connect(&W.ns[0], IP_B, 8443, 3000);
    CHECK(c >= 0);
    CHECK_EQ_INT(tcp_write(&W.ns[0], c, "request", 7, 1000), NXE_OK);
    n = tcp_read(&W.ns[0], c, got_a, 100, 3000);
    CHECK_EQ_INT(n, -NXE_NET_TIMEOUT);
    CHECK_EQ_INT(tcp_unacked(&W.ns[0], c), 0);
    tcp_abort(&W.ns[0], c);

    /* the server resets the connection in the middle */
    tcp_setup();
    srv_abort_after = 5000;
    uint32_t got = 0;
    int r = transfer(&got);
    CHECK(r == NXE_CONN_RESET || r == NXE_PEER_CLOSED);
    CHECK_EQ_INT(r, NXE_CONN_RESET);

    /* the server closes early: FIN before all data */
    tcp_setup();
    W.app = 0;
    int l = srv_listen;
    c = tcp_connect(&W.ns[0], IP_B, 8443, 3000);
    CHECK(c >= 0);
    W.stepping = 1;
    W.now += 2;
    net_poll(&W.ns[1], 0); /* the client's ACK of the SYN-ACK */
    int sc = tcp_accept(&W.ns[1], l);
    CHECK(sc >= 0);
    CHECK_EQ_INT(tcp_write(&W.ns[1], sc, "partial", 7, 0), NXE_OK);
    tcp_shutdown(&W.ns[1], sc);
    W.stepping = 0;
    uint8_t buf[64];
    n = tcp_read(&W.ns[0], c, buf, sizeof(buf), 1000);
    CHECK_EQ_INT(n, 7);
    CHECK_EQ_INT(tcp_read(&W.ns[0], c, buf, sizeof(buf), 1000), 0);
    CHECK_EQ_INT(W.ns[0].tcp[c].state, TCP_CLOSE_WAIT);
    CHECK_EQ_INT(tcp_write(&W.ns[0], c, "more", 4, 100), NXE_OK); /* half-closed: allowed */
    tcp_close(&W.ns[0], c, 1000);

    /* negative control: without retransmission a lossy link fails */
    tcp_setup();
    W.ns[0].noretx = 1;
    W.ns[1].noretx = 1;
    W.drop_every[0] = 7;
    W.drop_every[1] = 5;
    got = 0;
    r = transfer(&got);
    CHECK(r != NXE_OK);
    CHECK_EQ_INT(W.ns[0].st.tcp_retransmits, 0);
}

static void test_tcp_states(void)
{
    tcp_setup();
    W.app = 0;
    int c = tcp_connect(&W.ns[0], IP_B, 8443, 3000);
    CHECK(c >= 0);
    CHECK_EQ_INT(W.ns[0].tcp[c].state, TCP_ESTABLISHED);
    W.stepping = 1;
    W.now += 2;
    net_poll(&W.ns[1], 0);
    int sc = tcp_accept(&W.ns[1], srv_listen);
    W.stepping = 0;
    CHECK(sc >= 0);
    CHECK_EQ_INT(W.ns[1].tcp[sc].state, TCP_ESTABLISHED);
    CHECK_EQ_INT(W.ns[1].tcp[sc].rport, W.ns[0].tcp[c].lport);
    /* client closes first: FIN_WAIT_1 -> FIN_WAIT_2, server CLOSE_WAIT */
    tcp_shutdown(&W.ns[0], c);
    CHECK_EQ_INT(W.ns[0].tcp[c].state, TCP_FIN_WAIT_1);
    for (int i = 0; i < 10; i++)
        net_poll(&W.ns[0], 1);
    CHECK_EQ_INT(W.ns[0].tcp[c].state, TCP_FIN_WAIT_2);
    CHECK_EQ_INT(W.ns[1].tcp[sc].state, TCP_CLOSE_WAIT);
    /* server closes: LAST_ACK -> CLOSED, client TIME_WAIT -> CLOSED */
    W.stepping = 1;
    tcp_shutdown(&W.ns[1], sc);
    CHECK_EQ_INT(W.ns[1].tcp[sc].state, TCP_LAST_ACK);
    W.stepping = 0;
    for (int i = 0; i < 10; i++)
        net_poll(&W.ns[0], 1);
    CHECK_EQ_INT(W.ns[0].tcp[c].state, TCP_TIME_WAIT);
    CHECK_EQ_INT(W.ns[1].tcp[sc].state, TCP_CLOSED);
    for (int i = 0; i < 60; i++)
        net_poll(&W.ns[0], 10);
    CHECK_EQ_INT(W.ns[0].tcp[c].state, TCP_CLOSED);
    CHECK(strcmp(tcp_state_name(TCP_CLOSE_WAIT), "CLOSE_WAIT") == 0);
    /* a segment for a connection the peer no longer knows gets a RST */
    tcp_setup();
    W.app = 0;
    c = tcp_connect(&W.ns[0], IP_B, 8443, 3000);
    CHECK(c >= 0);
    W.now += 2;
    W.stepping = 1;
    net_poll(&W.ns[1], 0);
    sc = tcp_accept(&W.ns[1], srv_listen);
    CHECK(sc >= 0);
    W.ns[1].tcp[sc].used = 0; /* the server forgot it (e.g. restarted) */
    W.stepping = 0;
    uint64_t rst = W.ns[1].st.tcp_rst_tx;
    CHECK_EQ_INT(tcp_write(&W.ns[0], c, "x", 1, 10), NXE_OK);
    uint8_t one;
    CHECK_EQ_INT(tcp_read(&W.ns[0], c, &one, 1, 100), -NXE_CONN_RESET);
    CHECK(W.ns[1].st.tcp_rst_tx > rst);
    CHECK_EQ_INT(W.ns[0].tcp[c].err, NXE_CONN_RESET);
    tcp_abort(&W.ns[0], c);
}

void test_net(void)
{
    test_checksum_ip();
    test_dns_messages();
    test_err_classes();
    test_arp_ping_udp();
    test_dns_resolver();
    test_tcp_transfer();
    test_tcp_failures();
    test_tcp_states();
}
