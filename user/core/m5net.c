/*
 * bin/core, M5: the network stack on the virtio-net system calls, the
 * CSPRNG on the entropy system call, the network configuration from the
 * store, and the diagnostic NCI operations (docs/m5-net.md §4-§7):
 *
 *   net.status                 link, addresses, counters, TCP slots, clock
 *   net.ping target=<ip>       ICMP echo (the next hop is resolved by ARP)
 *   net.resolve name=<host>    DNS A query to the configured server
 *   net.probe host= port= text=
 *                              TCP connection: sends "<text>\n", expects the
 *                              same line back (echo server of the bench)
 *   rng.status                 entropy source, health tests, DRBG state
 */
#include <nanox/m5.h>
#include <nanox/hash.h>
#include <nanox/store.h>
#include <nanox/string.h>

#include "m5.h"
#include "nanox_user.h"

int m5_enabled;
uint64_t m5_flags;
struct netstack g_ns;
static uint64_t net_h;
static struct nx_drbg drbg;
static int rng_ok;
static uint64_t rng_health_failures, rng_bytes;
static char cfg_err[48];

/* ---- clock, entropy -------------------------------------------------------------- */

uint64_t m5_now_ms(void)
{
    struct nx_clock c;
    if (nx_clock(&c) < 0 || !c.hz)
        return 0;
    return c.ticks * 1000u / c.hz;
}

uint64_t m5_unix_time(void)
{
    struct nx_clock c;
    if (nx_clock(&c) < 0)
        return 0;
    return c.unix_s;
}

/* 48 bytes of raw entropy that passed the health tests; 0 ok. */
static int gather(uint8_t out[NX_DRBG_SEED_LEN])
{
    for (int tries = 0; tries < 3; tries++) {
        if (nx_entropy(out, NX_DRBG_SEED_LEN) != NX_DRBG_SEED_LEN)
            return -1;
        rng_bytes += NX_DRBG_SEED_LEN;
        if (nx_entropy_health(out, NX_DRBG_SEED_LEN) == 0)
            return 0;
        rng_health_failures++;
    }
    return -1;
}

static void seed(void)
{
    uint8_t e[NX_DRBG_SEED_LEN];
    struct {
        uint64_t boot, ticks, unix_s;
        char tag[16];
    } pers;
    memset(&pers, 0, sizeof(pers));
    pers.boot = boot_id;
    pers.ticks = m5_now_ms();
    pers.unix_s = m5_unix_time();
    memcpy(pers.tag, "nanox-core-drbg", 15);
    if (gather(e) != 0) {
        rng_ok = 0;
        return;
    }
    nx_drbg_instantiate(&drbg, e, sizeof(e), (const uint8_t *)&pers, sizeof(pers));
    nx_wipe(e, sizeof(e));
    rng_ok = 1;
}

int m5_random(void *buf, uint32_t len)
{
    if (!rng_ok)
        return NXE_LOC_ENTROPY;
    int r = nx_drbg_generate(&drbg, buf, len, 0, 0);
    if (r == 1) {
        uint8_t e[NX_DRBG_SEED_LEN];
        if (gather(e) != 0) {
            rng_ok = 0;
            return NXE_LOC_ENTROPY;
        }
        nx_drbg_reseed(&drbg, e, sizeof(e), 0, 0);
        nx_wipe(e, sizeof(e));
        r = nx_drbg_generate(&drbg, buf, len, 0, 0);
    }
    return r == 0 ? NXE_OK : NXE_LOC_ENTROPY;
}

/* ---- device operations of the stack --------------------------------------------- */

static int dev_send(void *ctx, const uint8_t *f, uint32_t len)
{
    (void)ctx;
    int64_t r = nx_net_send(net_h, f, len);
    if (r == (int64_t)len)
        return NXE_OK;
    return r == -NX_ENOLINK ? NXE_LINK_DOWN : NXE_NET_IO;
}

static uint32_t dev_recv(void *ctx, uint8_t *buf, uint32_t wait_ms)
{
    (void)ctx;
    uint32_t ticks = (wait_ms + 9u) / 10u;
    if (ticks > NX_NET_TIMEOUT_MAX)
        ticks = NX_NET_TIMEOUT_MAX;
    int64_t r = nx_net_recv(net_h, buf, NET_FRAME_MAX, ticks);
    return r > 0 ? (uint32_t)r : 0;
}

static uint64_t dev_now(void *ctx)
{
    (void)ctx;
    return m5_now_ms();
}

static int dev_link(void *ctx)
{
    (void)ctx;
    struct nx_net_info ni;
    return nx_net_info(net_h, &ni) == 0 && (ni.flags & NX_NET_INFO_LINK_UP);
}

static uint32_t dev_random(void *ctx)
{
    (void)ctx;
    uint32_t v = 0;
    if (m5_random(&v, sizeof(v)) != NXE_OK) {
        /* no CSPRNG: fall back to the clock so the stack still works; the
         * TLS layer refuses to run without the CSPRNG (NXE_LOC_ENTROPY) */
        v = (uint32_t)(m5_now_ms() * 2654435761u) ^ (uint32_t)boot_id;
    }
    return v;
}

/* ---- configuration --------------------------------------------------------------- */

int m5_cfg(const char *key, char *out, uint32_t cap, const char *def)
{
    char name[32] = "cfg/";
    uint32_t k = 0;
    while (key[k] && k < 23) {
        name[4 + k] = key[k];
        k++;
    }
    name[4 + k] = 0;
    uint32_t len = 0;
    if (cap > 1 && ps_read_obj(name, out, cap - 1, &len) == ST_OK) {
        out[len] = 0;
        return 1;
    }
    uint32_t i = 0;
    for (; def && def[i] && i + 1 < cap; i++)
        out[i] = def[i];
    out[i] = 0;
    return 0;
}

static int cfg_ip(const char *key, const char *def, uint32_t *ip)
{
    char v[NCI_VAL_MAX + 1];
    m5_cfg(key, v, sizeof(v), def);
    if (!net_parse_ip(v, ip)) {
        uint32_t n = 0;
        const char *pre = "bad cfg/";
        while (*pre && n < sizeof(cfg_err) - 1)
            cfg_err[n++] = *pre++;
        while (*key && n < sizeof(cfg_err) - 1)
            cfg_err[n++] = *key++;
        cfg_err[n] = 0;
        return 0;
    }
    return 1;
}

static uint32_t parse_u32(const char *s, int *ok)
{
    uint32_t v = 0, n = 0;
    while (*s >= '0' && *s <= '9' && n < 9) {
        v = v * 10 + (uint32_t)(*s++ - '0');
        n++;
    }
    *ok = n > 0 && *s == 0;
    return v;
}

void m5_init(uint64_t h, uint64_t flags)
{
    net_h = h;
    m5_flags = flags;
    m5_enabled = 1;
    seed();
    struct nx_net_info ni;
    memset(&ni, 0, sizeof(ni));
    nx_net_info(net_h, &ni);
    struct net_config cfg;
    memset(&cfg, 0, sizeof(cfg));
    memcpy(cfg.mac, ni.mac, 6);
    /* defaults: QEMU user networking (slirp) */
    int ok = cfg_ip("net.ip", "10.0.2.15", &cfg.ip) && cfg_ip("net.mask", "255.255.255.0",
                                                                 &cfg.mask) &&
             cfg_ip("net.gw", "10.0.2.2", &cfg.gw) && cfg_ip("net.dns", "10.0.2.3", &cfg.dns_ip);
    char v[16];
    m5_cfg("net.dns_port", v, sizeof(v), "53");
    int pok;
    uint32_t port = parse_u32(v, &pok);
    if (!pok || port == 0 || port > 65535) {
        ok = 0;
        memcpy(cfg_err, "bad cfg/net.dns_port", 21);
    }
    cfg.dns_port = (uint16_t)port;
    struct net_ops ops = {0, dev_send, dev_recv, dev_now, dev_link, dev_random};
    net_init(&g_ns, &ops, &cfg);
    g_ns.noretx = (flags & NX_M5_CORE_NORETX) != 0;
    char ip[16], gw[16], dns[16];
    net_format_ip(cfg.ip, ip);
    net_format_ip(cfg.gw, gw);
    net_format_ip(cfg.dns_ip, dns);
    u_printf("net up mac=%02x:%02x:%02x:%02x:%02x:%02x ip=%s gw=%s dns=%s:%u link=%s"
             " config=%s rng=%s tcp_retransmit=%s\n",
             ni.mac[0], ni.mac[1], ni.mac[2], ni.mac[3], ni.mac[4], ni.mac[5], ip, gw, dns,
             cfg.dns_port, ni.flags & NX_NET_INFO_LINK_UP ? "up" : "down",
             ok ? "ok" : cfg_err, rng_ok ? "ok" : "FAILED", g_ns.noretx ? "off" : "on");
}

/* ---- failures ---------------------------------------------------------------------- */

int m5_class(int err)
{
    if (err != NXE_OK && (m5_flags & NX_M5_CORE_FLATTEL))
        return NXC_UNCLASSIFIED;
    return nx_err_class(err);
}

void m5_fail(struct eng_action *a, struct nci_buf *b, int err, const char *effects)
{
    static const char *const codes[] = {"OK", "NET_ERROR", "TLS_ERROR", "PROVIDER_ERROR",
                                        "ACTION_ERROR", "LOCAL_ERROR", "UNCLASSIFIED_ERROR"};
    int cls = m5_class(err);
    char t[128];
    struct nci_buf tb;
    nb_init(&tb, t, sizeof(t));
    nb_str(&tb, "code=");
    nb_str(&tb, codes[cls]);
    nb_str(&tb, " detail=");
    nb_str(&tb, nx_err_name(err));
    nb_str(&tb, " class=");
    nb_str(&tb, nx_err_class_name(cls));
    step(a, ACT_FAILED, t);
    res_begin(b, a->id, "FAILED");
    nb_kv(b, "code", codes[cls]);
    nb_kv(b, "detail", nx_err_name(err));
    nb_kv(b, "class", nx_err_class_name(cls));
    nb_kv(b, "effects", effects);
    res_end(b, a->id);
}

/* ---- operations -------------------------------------------------------------------- */

static int net_ready(struct eng_action *a, struct nci_buf *b)
{
    if (m5_enabled)
        return 1;
    fail_action(a, b, "NO_NETWORK", 0, "none");
    return 0;
}

static void op_net_status(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    (void)r;
    step(a, ACT_OBSERVING, "");
    if (!net_ready(a, b))
        return;
    step(a, ACT_PLANNED, "call=NET_INFO");
    step(a, ACT_RUNNING, "");
    struct nx_net_info ni;
    int64_t st = nx_net_info(net_h, &ni);
    if (st < 0) {
        fail_action(a, b, "KERNEL_ERROR", u_err(st), "none");
        return;
    }
    step(a, ACT_VERIFYING, "read-only");
    step(a, ACT_SUCCEEDED, "");
    const struct net_stats *s = &g_ns.st;
    char ip[16];
    res_begin(b, a->id, "SUCCEEDED");
    nb_kv(b, "link", ni.flags & NX_NET_INFO_LINK_UP ? "up" : "down");
    net_format_ip(g_ns.cfg.ip, ip);
    nb_kv(b, "ip", ip);
    net_format_ip(g_ns.cfg.gw, ip);
    nb_kv(b, "gw", ip);
    net_format_ip(g_ns.cfg.dns_ip, ip);
    nb_kv(b, "dns", ip);
    nb_kv_u64(b, "dns_port", g_ns.cfg.dns_port);
    nb_kv_u64(b, "dev_rx", ni.rx_frames);
    nb_kv_u64(b, "dev_tx", ni.tx_frames);
    nb_kv_u64(b, "dev_rx_test_drops", ni.rx_test_drops);
    nb_kv_u64(b, "dev_tx_test_drops", ni.tx_test_drops);
    nb_kv_u64(b, "dev_link_down", ni.tx_link_down);
    nb_kv_u64(b, "arp_tx", s->arp_tx);
    nb_kv_u64(b, "arp_rx", s->arp_rx);
    nb_kv_u64(b, "ip_rx", s->ip_rx);
    nb_kv_u64(b, "ip_tx", s->ip_tx);
    nb_kv_u64(b, "ip_bad", s->ip_bad_csum + s->rx_bad);
    nb_kv_u64(b, "icmp_rx", s->icmp_rx);
    nb_kv_u64(b, "udp_rx", s->udp_rx);
    nb_kv_u64(b, "dns_queries", s->dns_queries);
    nb_kv_u64(b, "tcp_rx", s->tcp_rx);
    nb_kv_u64(b, "tcp_tx", s->tcp_tx);
    nb_kv_u64(b, "tcp_retransmits", s->tcp_retransmits);
    nb_kv_u64(b, "tcp_conns", s->tcp_conns_opened);
    nb_kv_u64(b, "tcp_rst_rx", s->tcp_rst_rx);
    nb_kv(b, "tcp_retransmit", g_ns.noretx ? "off" : "on");
    nb_kv_u64(b, "unix_time", m5_unix_time());
    nb_kv(b, "verify", "n/a");
    for (uint32_t i = 0; i < NET_TCP_CONNS; i++) {
        const struct tcp_conn *t = &g_ns.tcp[i];
        if (!t->used)
            continue;
        item_begin(b, a->id);
        nb_kv_u64(b, "slot", i);
        nb_kv(b, "state", tcp_state_name(t->state));
        net_format_ip(t->rip, ip);
        nb_kv(b, "peer", ip);
        nb_kv_u64(b, "port", t->rport);
        nb_kv_u64(b, "retransmits", t->retransmits);
    }
    res_end(b, a->id);
}

static void op_net_ping(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    step(a, ACT_OBSERVING, "");
    if (!net_ready(a, b))
        return;
    uint32_t ip;
    const char *t = nci_get(r, "target");
    if (!t || !net_parse_ip(t, &ip)) {
        fail_action(a, b, "BAD_REQUEST", "target", "none");
        return;
    }
    step(a, ACT_PLANNED, "icmp=echo");
    step(a, ACT_RUNNING, "");
    uint32_t rtt = 0;
    int e = net_ping(&g_ns, ip, 3000, &rtt);
    if (e != NXE_OK) {
        m5_fail(a, b, e, "none");
        return;
    }
    step(a, ACT_VERIFYING, "echo_reply=yes");
    step(a, ACT_SUCCEEDED, "");
    res_begin(b, a->id, "SUCCEEDED");
    nb_kv(b, "target", t);
    nb_kv_u64(b, "rtt_ms", rtt);
    nb_kv(b, "verify", "ok");
    nb_kv(b, "checks", "echo_reply_matches");
    res_end(b, a->id);
}

static void op_net_resolve(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    step(a, ACT_OBSERVING, "");
    if (!net_ready(a, b))
        return;
    const char *name = nci_get(r, "name");
    if (!name) {
        fail_action(a, b, "BAD_REQUEST", "name", "none");
        return;
    }
    step(a, ACT_PLANNED, "dns=A");
    step(a, ACT_RUNNING, "");
    uint32_t ip = 0;
    int e = net_dns_a(&g_ns, name, 3000, &ip);
    if (e != NXE_OK) {
        m5_fail(a, b, e, "none");
        return;
    }
    step(a, ACT_VERIFYING, "answer=A");
    step(a, ACT_SUCCEEDED, "");
    char s[16];
    net_format_ip(ip, s);
    res_begin(b, a->id, "SUCCEEDED");
    nb_kv(b, "name", name);
    nb_kv(b, "address", s);
    nb_kv(b, "verify", "n/a");
    res_end(b, a->id);
}

static void op_net_probe(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    step(a, ACT_OBSERVING, "");
    if (!net_ready(a, b))
        return;
    const char *host = nci_get(r, "host"), *text = nci_get(r, "text");
    uint64_t port = 0;
    if (!host || !text || nci_get_u64(r, "port", &port) != 1 || port == 0 || port > 65535) {
        fail_action(a, b, "BAD_REQUEST", "host_port_text", "none");
        return;
    }
    step(a, ACT_PLANNED, "tcp=echo");
    step(a, ACT_RUNNING, "");
    uint32_t ip;
    int e = net_dns_a(&g_ns, host, 3000, &ip);
    if (e != NXE_OK) {
        m5_fail(a, b, e, "none");
        return;
    }
    uint64_t t0 = m5_now_ms();
    int c = tcp_connect(&g_ns, ip, (uint16_t)port, NET_CONNECT_TIMEOUT_MS);
    if (c < 0) {
        m5_fail(a, b, -c, "none");
        return;
    }
    char line[NCI_VAL_MAX + 2];
    uint32_t n = 0;
    while (text[n]) {
        line[n] = text[n];
        n++;
    }
    line[n++] = '\n';
    e = tcp_write(&g_ns, c, line, n, 5000);
    char got[NCI_VAL_MAX + 2];
    uint32_t have = 0;
    while (e == NXE_OK && (have == 0 || got[have - 1] != '\n') && have < sizeof(got)) {
        int k = tcp_read(&g_ns, c, got + have, (uint32_t)sizeof(got) - have, 5000);
        if (k == 0)
            e = NXE_PEER_CLOSED;
        else if (k < 0)
            e = -k == NXE_NET_TIMEOUT && tcp_unacked(&g_ns, c) ? NXE_UNACKED_TIMEOUT : -k;
        else
            have += (uint32_t)k;
    }
    uint32_t retx = g_ns.tcp[c].retransmits;
    tcp_close(&g_ns, c, 2000);
    if (e != NXE_OK) {
        m5_fail(a, b, e, "none");
        return;
    }
    int same = have == n && memcmp(got, line, n) == 0;
    if (!same) {
        step(a, ACT_VERIFYING, "");
        fail_action(a, b, "VERIFY_FAILED", "echo_differs", "none");
        return;
    }
    step(a, ACT_VERIFYING, "echo=same");
    step(a, ACT_SUCCEEDED, "");
    char s[16];
    net_format_ip(ip, s);
    res_begin(b, a->id, "SUCCEEDED");
    nb_kv(b, "address", s);
    nb_kv_u64(b, "port", port);
    nb_kv_u64(b, "bytes", n);
    nb_kv_u64(b, "ms", m5_now_ms() - t0);
    nb_kv_u64(b, "retransmits", retx);
    nb_kv(b, "verify", "ok");
    nb_kv(b, "checks", "echo_same");
    res_end(b, a->id);
}

static void op_rng_status(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    (void)r;
    step(a, ACT_OBSERVING, "");
    if (!net_ready(a, b))
        return;
    step(a, ACT_PLANNED, "drbg=hmac-sha256");
    step(a, ACT_RUNNING, "");
    uint8_t x[16], y[16];
    int e1 = m5_random(x, sizeof(x)), e2 = m5_random(y, sizeof(y));
    int differ = memcmp(x, y, sizeof(x)) != 0;
    step(a, ACT_VERIFYING, "");
    if (e1 != NXE_OK || e2 != NXE_OK || !differ) {
        m5_fail(a, b, NXE_LOC_ENTROPY, "none");
        return;
    }
    step(a, ACT_SUCCEEDED, "");
    res_begin(b, a->id, "SUCCEEDED");
    nb_kv(b, "source", "virtio-rng");
    nb_kv(b, "drbg", "hmac-sha256");
    nb_kv(b, "health", rng_ok ? "ok" : "failed");
    nb_kv_u64(b, "health_failures", rng_health_failures);
    nb_kv_u64(b, "entropy_bytes", rng_bytes);
    nb_kv_u64(b, "reseeds", drbg.reseeds);
    nb_kv_u64(b, "generated", drbg.generated);
    nb_kv(b, "verify", "ok");
    nb_kv(b, "checks", "outputs_differ");
    res_end(b, a->id);
}

const char M5_OPS[] = ",net.status,net.ping,net.resolve,net.probe,rng.status,tls.probe,"
                      "agent.ask,provider.status,telemetry.status";

op_fn m5_op(const char *op)
{
    static const struct {
        const char *name;
        op_fn fn;
    } T[] = {
        {"net.status", op_net_status}, {"net.ping", op_net_ping},
        {"net.resolve", op_net_resolve}, {"net.probe", op_net_probe},
        {"rng.status", op_rng_status},
    };
    for (uint32_t i = 0; i < sizeof(T) / sizeof(T[0]); i++)
        if (nci_streq(op, T[i].name))
            return T[i].fn;
    op_fn fn = m5_tls_op(op);
    return fn ? fn : m5_agent_op(op);
}
