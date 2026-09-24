/*
 * bin/core, M5: TLS over the guest's own TCP, trust anchors from the store,
 * and the diagnostic NCI operation (docs/m5-net.md §6-7):
 *
 *   tls.probe host= port= [suites=1|2|3]
 *        DNS, TCP, TLS 1.3 handshake with certificate verification against
 *        the provisioned anchors, "hello\n" out, one line back; reports the
 *        suite, signature scheme, chain depth, or the classified failure
 *
 * One TLS connection at a time (the provider client keeps it open between
 * requests: HTTP/1.1 keep-alive).
 */
#include <nanox/m4.h>
#include <nanox/store.h>
#include <nanox/string.h>
#include <nanox/tls.h>

#include "m5.h"
#include "nanox_user.h"

#define ANCHORS_MAX 4u
#define ANCHOR_BYTES 4096u

static struct nx_cert anchors[ANCHORS_MAX];
static uint8_t anchor_der[ANCHORS_MAX][ANCHOR_BYTES];
static uint32_t nanchors;
static int anchors_loaded;

static struct tls_conn tls;
static int tcp_slot = -1;
static int tls_open;
struct m5_conn_info m5_conn;

uint32_t m5_anchor_count(void)
{
    if (!anchors_loaded) {
        anchors_loaded = 1;
        for (uint32_t i = 0; i < ANCHORS_MAX; i++) {
            char name[16] = "tls/anchor0";
            name[10] = (char)('0' + i);
            uint32_t len = 0;
            if (ps_read_obj(name, anchor_der[nanchors], ANCHOR_BYTES, &len) != ST_OK)
                continue;
            if (nx_x509_parse(anchor_der[nanchors], len, &anchors[nanchors]) != NXE_OK) {
                u_printf("tls anchor %s: not a parsable certificate, ignored\n", name);
                continue;
            }
            nanchors++;
        }
        u_printf("tls anchors=%u (store objects tls/anchor0..%u)\n", nanchors, ANCHORS_MAX - 1);
    }
    return nanchors;
}

static int io_send(void *ctx, const uint8_t *buf, uint32_t len)
{
    (void)ctx;
    return tcp_write(&g_ns, tcp_slot, buf, len, 10000);
}

static int io_recv(void *ctx, uint8_t *buf, uint32_t cap, uint32_t timeout_ms)
{
    (void)ctx;
    return tcp_read(&g_ns, tcp_slot, buf, cap, timeout_ms);
}

static int io_random(void *ctx, void *buf, uint32_t len)
{
    (void)ctx;
    return m5_random(buf, len);
}

void m5_tls_close(int graceful)
{
    if (tls_open && graceful)
        tls_close(&tls);
    tls_open = 0;
    if (tcp_slot >= 0) {
        if (graceful)
            tcp_close(&g_ns, tcp_slot, 1000);
        else
            tcp_abort(&g_ns, tcp_slot);
    }
    tcp_slot = -1;
}

int m5_tls_is_open(void)
{
    return tls_open && tcp_slot >= 0 && !tls.err && !tls.peer_closed &&
           g_ns.tcp[tcp_slot].used && g_ns.tcp[tcp_slot].state == TCP_ESTABLISHED;
}

int m5_tls_open(const char *host, uint16_t port, uint32_t suites, uint32_t timeout_ms)
{
    m5_tls_close(0);
    memset(&m5_conn, 0, sizeof(m5_conn));
    if (!m5_anchor_count())
        return NXE_LOC_CONFIG;
    uint32_t ip = 0;
    uint64_t t0 = m5_now_ms();
    int e = net_dns_a(&g_ns, host, 3000, &ip);
    if (e != NXE_OK)
        return e;
    m5_conn.ip = ip;
    int c = tcp_connect(&g_ns, ip, port, timeout_ms);
    if (c < 0)
        return -c;
    tcp_slot = c;
    m5_conn.connect_ms = (uint32_t)(m5_now_ms() - t0);
    struct tls_io io = {0, io_send, io_recv};
    struct tls_config cfg;
    memset(&cfg, 0, sizeof(cfg));
    cfg.host = host;
    cfg.anchors = anchors;
    cfg.nanchors = nanchors;
    cfg.now = (int64_t)m5_unix_time();
    cfg.suites = suites;
    cfg.random = io_random;
    cfg.timeout_ms = timeout_ms;
    if (!cfg.now) {
        m5_tls_close(0);
        return NXE_LOC_CONFIG; /* no clock: validity cannot be checked */
    }
    e = tls_connect(&tls, &io, &cfg);
    m5_conn.suite = tls.suite;
    m5_conn.sig = tls.sig_scheme;
    m5_conn.chain = tls.chain_len;
    m5_conn.depth = tls.chain_depth;
    memcpy(m5_conn.detail, tls.chain_detail, sizeof(m5_conn.detail));
    m5_conn.handshake_ms = (uint32_t)(m5_now_ms() - t0);
    if (e != NXE_OK) {
        /* a transport timeout while the handshake data is acknowledged is
         * the server not answering; unacknowledged data is the network */
        if (e == NXE_NET_TIMEOUT)
            e = tcp_unacked(&g_ns, tcp_slot) ? NXE_UNACKED_TIMEOUT : NXE_PRV_TIMEOUT;
        if (!net_link_ok() && nx_err_class(e) == NXC_NET)
            e = NXE_LINK_DOWN;
        m5_tls_close(0);
        return e;
    }
    tls_open = 1;
    return NXE_OK;
}

int m5_tls_write(const void *buf, uint32_t len)
{
    if (!tls_open)
        return NXE_PEER_CLOSED;
    return tls_write(&tls, buf, len);
}

int m5_tls_read(void *buf, uint32_t cap, uint32_t timeout_ms)
{
    if (!tls_open)
        return -NXE_PEER_CLOSED;
    return tls_read(&tls, buf, cap, timeout_ms);
}

uint32_t m5_tls_unacked(void)
{
    return tcp_slot >= 0 ? tcp_unacked(&g_ns, tcp_slot) : 0;
}

int net_link_ok(void)
{
    return g_ns.ops.link_up(g_ns.ops.ctx);
}

/* ---- tls.probe --------------------------------------------------------------------------- */

static void op_tls_probe(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    step(a, ACT_OBSERVING, "");
    if (!m5_enabled) {
        fail_action(a, b, "NO_NETWORK", 0, "none");
        return;
    }
    const char *host = nci_get(r, "host");
    uint64_t port = 0, suites = 3;
    if (!host || nci_get_u64(r, "port", &port) != 1 || port == 0 || port > 65535 ||
        nci_get_u64(r, "suites", &suites) < 0 || suites < 1 || suites > 3) {
        fail_action(a, b, "BAD_REQUEST", "host_port_suites", "none");
        return;
    }
    step(a, ACT_PLANNED, "tls=1.3");
    step(a, ACT_RUNNING, "");
    int e = m5_tls_open(host, (uint16_t)port, (uint32_t)suites, NET_CONNECT_TIMEOUT_MS);
    char got[32];
    uint32_t have = 0;
    if (e == NXE_OK) {
        e = m5_tls_write("hello\n", 6);
        while (e == NXE_OK && have < sizeof(got) - 1 && (have == 0 || got[have - 1] != '\n')) {
            int n = m5_tls_read(got + have, (uint32_t)sizeof(got) - 1 - have, 5000);
            if (n == 0)
                e = NXE_PRV_TRUNCATED;
            else if (n < 0)
                e = -n;
            else
                have += (uint32_t)n;
        }
        m5_tls_close(1);
    }
    u_printf("tls probe host=%s port=%lu result=%s class=%s suite=%s sig=%s chain=%u depth=%u"
             " detail=%s ms=%u\n",
             host, port, nx_err_name(e), nx_err_class_name(m5_class(e)),
             tls_suite_name(m5_conn.suite), tls_sig_name(m5_conn.sig), m5_conn.chain,
             m5_conn.depth, m5_conn.detail[0] ? m5_conn.detail : "-", m5_conn.handshake_ms);
    if (e != NXE_OK) {
        m5_fail(a, b, e, "none");
        return;
    }
    got[have] = 0;
    int same = have == 6 && memcmp(got, "hello\n", 6) == 0;
    step(a, ACT_VERIFYING, same ? "echo=same" : "echo=differs");
    if (!same) {
        fail_action(a, b, "VERIFY_FAILED", "echo_differs", "none");
        return;
    }
    step(a, ACT_SUCCEEDED, "");
    res_begin(b, a->id, "SUCCEEDED");
    nb_kv(b, "suite", tls_suite_name(m5_conn.suite));
    nb_kv(b, "sig", tls_sig_name(m5_conn.sig));
    nb_kv_u64(b, "chain", m5_conn.chain);
    nb_kv_u64(b, "depth", m5_conn.depth);
    nb_kv_u64(b, "anchors", nanchors);
    nb_kv_u64(b, "ms", m5_conn.handshake_ms);
    nb_kv(b, "verify", "ok");
    nb_kv(b, "checks", "certificate_chain,host_name,validity,handshake_signature,finished,echo");
    res_end(b, a->id);
}

op_fn m5_tls_op(const char *op)
{
    return nci_streq(op, "tls.probe") ? op_tls_probe : 0;
}
