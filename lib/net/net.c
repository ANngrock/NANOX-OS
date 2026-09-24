/*
 * Network stack of bin/core: Ethernet, ARP, IPv4, ICMP, UDP, DNS (TCP is in
 * tcp.c).  See nanox/net.h and docs/m5-net.md §4.
 */
#include <nanox/net.h>
#include <nanox/string.h>

#include "inet.h"

static const uint8_t BCAST[6] = {0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF};

uint64_t net_now(struct netstack *ns)
{
    return ns->ops.now_ms(ns->ops.ctx);
}

uint16_t net_checksum(const void *data, uint32_t len, uint32_t sum)
{
    const uint8_t *p = data;
    uint64_t s = sum;
    while (len > 1) {
        s += (uint32_t)p[0] << 8 | p[1];
        p += 2;
        len -= 2;
    }
    if (len)
        s += (uint32_t)p[0] << 8;
    while (s >> 16)
        s = (s & 0xFFFFu) + (s >> 16);
    return (uint16_t)~s;
}

void net_init(struct netstack *ns, const struct net_ops *ops, const struct net_config *cfg)
{
    memset(ns, 0, sizeof(*ns));
    ns->ops = *ops;
    ns->cfg = *cfg;
    ns->next_port = (uint16_t)(49152u + ops->random32(ops->ctx) % 16000u);
    ns->ip_id = (uint16_t)ops->random32(ops->ctx);
}

int net_parse_ip(const char *s, uint32_t *out)
{
    uint32_t ip = 0;
    for (int part = 0; part < 4; part++) {
        uint32_t v = 0, digits = 0;
        while (*s >= '0' && *s <= '9' && digits < 3) {
            v = v * 10 + (uint32_t)(*s++ - '0');
            digits++;
        }
        if (!digits || v > 255)
            return 0;
        ip = ip << 8 | v;
        if (part < 3) {
            if (*s != '.')
                return 0;
            s++;
        }
    }
    if (*s)
        return 0;
    *out = ip;
    return 1;
}

void net_format_ip(uint32_t ip, char out[16])
{
    uint32_t n = 0;
    for (int part = 3; part >= 0; part--) {
        uint32_t v = (ip >> (part * 8)) & 0xFFu;
        char tmp[3];
        uint32_t k = 0;
        do {
            tmp[k++] = (char)('0' + v % 10);
            v /= 10;
        } while (v);
        while (k)
            out[n++] = tmp[--k];
        if (part)
            out[n++] = '.';
    }
    out[n] = 0;
}

uint32_t net_next_hop(const struct netstack *ns, uint32_t ip)
{
    if ((ip & ns->cfg.mask) == (ns->cfg.ip & ns->cfg.mask))
        return ip;
    return ns->cfg.gw;
}

/* ---- Ethernet and ARP ------------------------------------------------------ */

static int eth_send(struct netstack *ns, uint8_t *frame, const uint8_t dst[6], uint16_t type,
                    uint32_t payload)
{
    memcpy(frame, dst, 6);
    memcpy(frame + 6, ns->cfg.mac, 6);
    wr16(frame + 12, type);
    uint32_t len = ETH_HDR + payload;
    if (len < 60) { /* minimum frame without FCS */
        memset(frame + len, 0, 60 - len);
        len = 60;
    }
    int r = ns->ops.send(ns->ops.ctx, frame, len);
    if (r == NXE_OK)
        ns->st.tx_frames++;
    else if (r == NXE_LINK_DOWN)
        ns->st.tx_link_down++;
    else
        ns->st.tx_fail++;
    return r;
}

static int arp_lookup(struct netstack *ns, uint32_t ip, uint8_t mac[6])
{
    uint64_t now = net_now(ns);
    for (uint32_t i = 0; i < NET_ARP_ENTRIES; i++)
        if (ns->arp[i].valid && ns->arp[i].ip == ip && now - ns->arp[i].t < NET_ARP_TTL_MS) {
            memcpy(mac, ns->arp[i].mac, 6);
            return 1;
        }
    return 0;
}

static void arp_learn(struct netstack *ns, uint32_t ip, const uint8_t mac[6])
{
    uint32_t slot = 0;
    uint64_t oldest = ~0ull;
    for (uint32_t i = 0; i < NET_ARP_ENTRIES; i++) {
        if (ns->arp[i].valid && ns->arp[i].ip == ip) {
            slot = i;
            break;
        }
        uint64_t t = ns->arp[i].valid ? ns->arp[i].t : 0;
        if (t < oldest) {
            oldest = t;
            slot = i;
        }
    }
    ns->arp[slot].valid = 1;
    ns->arp[slot].ip = ip;
    memcpy(ns->arp[slot].mac, mac, 6);
    ns->arp[slot].t = net_now(ns);
}

static int arp_send(struct netstack *ns, uint16_t op, const uint8_t tha[6], uint32_t tpa,
                    const uint8_t dst[6])
{
    uint8_t f[64];
    uint8_t *a = f + ETH_HDR;
    wr16(a, 1);          /* Ethernet */
    wr16(a + 2, 0x0800); /* IPv4 */
    a[4] = 6;
    a[5] = 4;
    wr16(a + 6, op);
    memcpy(a + 8, ns->cfg.mac, 6);
    wr32(a + 14, ns->cfg.ip);
    memcpy(a + 18, tha, 6);
    wr32(a + 24, tpa);
    ns->st.arp_tx++;
    return eth_send(ns, f, dst, ETH_ARP, 28);
}

static void arp_request(struct netstack *ns, uint32_t ip)
{
    static const uint8_t zero[6];
    uint64_t now = net_now(ns);
    /* at most one request per address per retry interval */
    if (ns->arp_last_ip == ip && now - ns->arp_last_req < NET_ARP_RETRY_MS && ns->arp_last_req)
        return;
    ns->arp_last_ip = ip;
    ns->arp_last_req = now ? now : 1;
    arp_send(ns, 1, zero, ip, BCAST);
}

static void arp_input(struct netstack *ns, const uint8_t *a, uint32_t len)
{
    if (len < 28 || rd16(a) != 1 || rd16(a + 2) != 0x0800 || a[4] != 6 || a[5] != 4)
        return;
    ns->st.arp_rx++;
    uint16_t op = rd16(a + 6);
    uint32_t spa = rd32(a + 14), tpa = rd32(a + 24);
    if (tpa != ns->cfg.ip)
        return;
    arp_learn(ns, spa, a + 8);
    if (op == 1) {
        ns->st.arp_replies++;
        arp_send(ns, 2, a + 8, spa, a + 8);
    }
}

int net_arp_resolve(struct netstack *ns, uint32_t ip, uint32_t timeout_ms, uint8_t mac[6])
{
    uint64_t deadline = net_now(ns) + timeout_ms;
    for (;;) {
        if (arp_lookup(ns, ip, mac))
            return NXE_OK;
        if (!ns->ops.link_up(ns->ops.ctx))
            return NXE_LINK_DOWN;
        uint64_t now = net_now(ns);
        if (now >= deadline) {
            ns->st.arp_unresolved++;
            return NXE_ARP_TIMEOUT;
        }
        arp_request(ns, ip);
        uint64_t left = deadline - now;
        net_poll(ns, left < 20 ? (uint32_t)left : 20);
    }
}

/* ---- IPv4 ----------------------------------------------------------------------- */

int ip_output(struct netstack *ns, uint32_t dst, uint8_t proto, uint32_t len)
{
    uint8_t mac[6];
    uint32_t hop = net_next_hop(ns, dst);
    if (!arp_lookup(ns, hop, mac)) {
        ns->st.arp_unresolved++;
        arp_request(ns, hop);
        return NXE_ARP_TIMEOUT;
    }
    uint8_t *ip = ns->tx + ETH_HDR;
    ip[0] = 0x45;
    ip[1] = 0;
    wr16(ip + 2, (uint16_t)(IP_HDR + len));
    wr16(ip + 4, ns->ip_id++);
    wr16(ip + 6, 0x4000); /* DF: no fragmentation */
    ip[8] = 64;
    ip[9] = proto;
    wr16(ip + 10, 0);
    wr32(ip + 12, ns->cfg.ip);
    wr32(ip + 16, dst);
    wr16(ip + 10, net_checksum(ip, IP_HDR, 0));
    ns->st.ip_tx++;
    return eth_send(ns, ns->tx, mac, ETH_IPV4, IP_HDR + len);
}

static void icmp_input(struct netstack *ns, uint32_t src, const uint8_t *m, uint32_t len)
{
    if (len < 8 || net_checksum(m, len, 0) != 0)
        return;
    ns->st.icmp_rx++;
    if (m[0] == 8 && len <= NET_MTU - IP_HDR) { /* echo request */
        uint8_t *out = ns->tx + ETH_HDR + IP_HDR;
        memmove(out, m, len);
        out[0] = 0;
        wr16(out + 2, 0);
        wr16(out + 2, net_checksum(out, len, 0));
        if (ip_output(ns, src, IP_ICMP, len) == NXE_OK)
            ns->st.icmp_echo_replied++;
    } else if (m[0] == 0 && rd16(m + 4) == ns->ping.id && rd16(m + 6) == ns->ping.seq) {
        ns->ping.got = 1;
        ns->ping.from = src;
    }
}

static void udp_input(struct netstack *ns, uint32_t src, uint32_t dst, const uint8_t *u,
                      uint32_t len)
{
    if (len < UDP_HDR || rd16(u + 4) < UDP_HDR || rd16(u + 4) > len)
        return;
    len = rd16(u + 4);
    if (rd16(u + 6) != 0 && net_checksum(u, len, pseudo_sum(src, dst, IP_UDP, len)) != 0)
        return;
    ns->st.udp_rx++;
    uint16_t dport = rd16(u + 2);
    for (uint32_t i = 0; i < NET_UDP_SOCKS; i++) {
        struct udp_sock *s = &ns->udp[i];
        if (!s->used || s->lport != dport)
            continue;
        s->have = 1; /* one datagram buffered: a newer one replaces it */
        s->from_ip = src;
        s->from_port = rd16(u);
        s->len = len - UDP_HDR;
        memcpy(s->data, u + UDP_HDR, s->len);
        return;
    }
    ns->st.udp_no_socket++;
}

static void ip_input(struct netstack *ns, const uint8_t *ip, uint32_t len)
{
    if (len < IP_HDR || (ip[0] >> 4) != 4)
        return;
    uint32_t ihl = (ip[0] & 0x0Fu) * 4u, total = rd16(ip + 2);
    if (ihl < IP_HDR || total < ihl || total > len) {
        ns->st.rx_bad++;
        return;
    }
    if (net_checksum(ip, ihl, 0) != 0) {
        ns->st.ip_bad_csum++;
        return;
    }
    if (rd16(ip + 6) & 0x3FFFu) { /* MF or an offset: fragments are not reassembled */
        ns->st.ip_frag_dropped++;
        return;
    }
    uint32_t src = rd32(ip + 12), dst = rd32(ip + 16);
    if (dst != ns->cfg.ip) {
        ns->st.ip_not_ours++;
        return;
    }
    ns->st.ip_rx++;
    const uint8_t *p = ip + ihl;
    uint32_t plen = total - ihl;
    switch (ip[9]) {
    case IP_ICMP: icmp_input(ns, src, p, plen); break;
    case IP_UDP: udp_input(ns, src, dst, p, plen); break;
    case IP_TCP: tcp_input(ns, src, dst, p, plen); break;
    default: break;
    }
}

void net_poll(struct netstack *ns, uint32_t wait_ms)
{
    uint32_t len = ns->ops.recv(ns->ops.ctx, ns->rx, wait_ms);
    if (len >= ETH_HDR) {
        ns->st.rx_frames++;
        const uint8_t *f = ns->rx;
        int for_us = memcmp(f, ns->cfg.mac, 6) == 0 || memcmp(f, BCAST, 6) == 0;
        uint16_t type = rd16(f + 12);
        if (for_us && type == ETH_ARP)
            arp_input(ns, f + ETH_HDR, len - ETH_HDR);
        else if (for_us && type == ETH_IPV4)
            ip_input(ns, f + ETH_HDR, len - ETH_HDR);
    }
    tcp_timers(ns);
}

int net_ping(struct netstack *ns, uint32_t ip, uint32_t timeout_ms, uint32_t *rtt_ms)
{
    uint8_t mac[6];
    uint64_t t0 = net_now(ns), deadline = t0 + timeout_ms;
    int r = net_arp_resolve(ns, net_next_hop(ns, ip), timeout_ms, mac);
    if (r != NXE_OK)
        return r;
    ns->ping.id = (uint16_t)ns->ops.random32(ns->ops.ctx);
    ns->ping.seq++;
    ns->ping.got = 0;
    uint8_t *m = ns->tx + ETH_HDR + IP_HDR;
    m[0] = 8;
    m[1] = 0;
    wr16(m + 2, 0);
    wr16(m + 4, ns->ping.id);
    wr16(m + 6, ns->ping.seq);
    for (uint32_t i = 0; i < 32; i++)
        m[8 + i] = (uint8_t)('a' + i % 26);
    wr16(m + 2, net_checksum(m, 40, 0));
    uint64_t sent = net_now(ns);
    r = ip_output(ns, ip, IP_ICMP, 40);
    if (r != NXE_OK)
        return r;
    while (!ns->ping.got) {
        uint64_t now = net_now(ns);
        if (now >= deadline)
            return NXE_NET_TIMEOUT;
        uint64_t left = deadline - now;
        net_poll(ns, left < 20 ? (uint32_t)left : 20);
    }
    *rtt_ms = (uint32_t)(net_now(ns) - sent);
    return NXE_OK;
}

/* ---- UDP ---------------------------------------------------------------------- */

static uint16_t ephemeral_port(struct netstack *ns)
{
    uint16_t p = ns->next_port++;
    if (ns->next_port < 49152u)
        ns->next_port = 49152u;
    return p;
}

int udp_open(struct netstack *ns, uint16_t lport)
{
    for (int i = 0; i < (int)NET_UDP_SOCKS; i++) {
        if (ns->udp[i].used)
            continue;
        memset(&ns->udp[i], 0, sizeof(ns->udp[i]));
        ns->udp[i].used = 1;
        ns->udp[i].lport = lport ? lport : ephemeral_port(ns);
        return i;
    }
    return -1;
}

void udp_close(struct netstack *ns, int s)
{
    if (s >= 0 && s < (int)NET_UDP_SOCKS)
        ns->udp[s].used = 0;
}

int udp_sendto(struct netstack *ns, int s, uint32_t ip, uint16_t port, const void *data,
               uint32_t len)
{
    if (s < 0 || s >= (int)NET_UDP_SOCKS || !ns->udp[s].used || len > NET_MTU - IP_HDR - UDP_HDR)
        return NXE_LOC_BAD_ARG;
    uint8_t *u = ns->tx + ETH_HDR + IP_HDR;
    wr16(u, ns->udp[s].lport);
    wr16(u + 2, port);
    wr16(u + 4, (uint16_t)(UDP_HDR + len));
    wr16(u + 6, 0);
    memcpy(u + UDP_HDR, data, len);
    uint16_t c = net_checksum(u, UDP_HDR + len,
                              pseudo_sum(ns->cfg.ip, ip, IP_UDP, UDP_HDR + len));
    wr16(u + 6, c ? c : 0xFFFFu);
    ns->st.udp_tx++;
    return ip_output(ns, ip, IP_UDP, UDP_HDR + len);
}

uint32_t udp_recv(struct netstack *ns, int s, void *buf, uint32_t cap, uint32_t timeout_ms,
                  uint32_t *from_ip, uint16_t *from_port)
{
    if (s < 0 || s >= (int)NET_UDP_SOCKS || !ns->udp[s].used)
        return 0;
    struct udp_sock *u = &ns->udp[s];
    uint64_t deadline = net_now(ns) + timeout_ms;
    while (!u->have) {
        uint64_t now = net_now(ns);
        if (now >= deadline)
            return 0;
        uint64_t left = deadline - now;
        net_poll(ns, left < 20 ? (uint32_t)left : 20);
    }
    u->have = 0;
    uint32_t n = u->len < cap ? u->len : cap;
    memcpy(buf, u->data, n);
    if (from_ip)
        *from_ip = u->from_ip;
    if (from_port)
        *from_port = u->from_port;
    return n;
}

/* ---- DNS ---------------------------------------------------------------------- */

uint32_t dns_build_query(uint16_t id, const char *name, uint8_t *out, uint32_t cap)
{
    uint32_t n = 12;
    if (cap < 12 + 2 + 4)
        return 0;
    memset(out, 0, 12);
    wr16(out, id);
    out[2] = 0x01; /* RD */
    wr16(out + 4, 1);
    const char *p = name;
    if (!*p)
        return 0;
    while (*p) {
        const char *dot = p;
        while (*dot && *dot != '.')
            dot++;
        uint32_t l = (uint32_t)(dot - p);
        if (l == 0 || l > 63 || n + 1 + l + 1 + 4 > cap || n + l > 12 + 253)
            return 0;
        out[n++] = (uint8_t)l;
        for (uint32_t i = 0; i < l; i++) {
            char c = p[i];
            int ok = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
                     c == '-' || c == '_';
            if (!ok)
                return 0;
            out[n++] = (uint8_t)c;
        }
        p = *dot ? dot + 1 : dot;
        if (*dot && !*p)
            break; /* trailing dot */
    }
    out[n++] = 0;
    wr16(out + n, 1); /* A */
    wr16(out + n + 2, 1); /* IN */
    return n + 4;
}

/* Skips a (possibly compressed) name at off; returns the offset after it,
 * or 0 if malformed. */
static uint32_t dns_skip_name(const uint8_t *m, uint32_t len, uint32_t off)
{
    for (uint32_t guard = 0; guard < 128; guard++) {
        if (off >= len)
            return 0;
        uint8_t l = m[off];
        if (l == 0)
            return off + 1;
        if ((l & 0xC0u) == 0xC0u)
            return off + 2 <= len ? off + 2 : 0;
        if (l & 0xC0u)
            return 0;
        off += 1u + l;
    }
    return 0;
}

int dns_parse_response(const uint8_t *m, uint32_t len, uint16_t id, uint32_t *ip)
{
    if (len < 12 || rd16(m) != id || !(m[2] & 0x80u))
        return NXE_DNS_BAD_REPLY;
    uint32_t rcode = m[3] & 0x0Fu;
    if (rcode == 3)
        return NXE_DNS_NOTFOUND;
    if (rcode != 0 || (m[2] & 0x02u)) /* error or truncated */
        return NXE_DNS_BAD_REPLY;
    uint32_t qd = rd16(m + 4), an = rd16(m + 6), off = 12;
    for (uint32_t i = 0; i < qd; i++) {
        off = dns_skip_name(m, len, off);
        if (!off || off + 4 > len)
            return NXE_DNS_BAD_REPLY;
        off += 4;
    }
    for (uint32_t i = 0; i < an; i++) {
        off = dns_skip_name(m, len, off);
        if (!off || off + 10 > len)
            return NXE_DNS_BAD_REPLY;
        uint16_t type = rd16(m + off), cls = rd16(m + off + 2), rdlen = rd16(m + off + 8);
        off += 10;
        if (off + rdlen > len)
            return NXE_DNS_BAD_REPLY;
        if (type == 1 && cls == 1 && rdlen == 4) {
            *ip = rd32(m + off);
            return NXE_OK;
        }
        off += rdlen;
    }
    return NXE_DNS_NOTFOUND;
}

int net_dns_a(struct netstack *ns, const char *name, uint32_t timeout_ms, uint32_t *ip)
{
    if (net_parse_ip(name, ip))
        return NXE_OK;
    if (!ns->cfg.dns_ip)
        return NXE_LOC_CONFIG;
    uint8_t q[300], r[NET_MTU];
    uint16_t id = (uint16_t)ns->ops.random32(ns->ops.ctx);
    uint32_t qlen = dns_build_query(id, name, q, sizeof(q));
    if (!qlen)
        return NXE_LOC_BAD_ARG;
    int s = udp_open(ns, 0);
    if (s < 0)
        return NXE_LOC_RESOURCES;
    uint64_t deadline = net_now(ns) + timeout_ms;
    uint32_t per_try = timeout_ms / 3 ? timeout_ms / 3 : 1;
    int result = NXE_DNS_TIMEOUT;
    for (int attempt = 0; attempt < 3; attempt++) {
        uint64_t now = net_now(ns);
        if (now >= deadline)
            break;
        if (!ns->ops.link_up(ns->ops.ctx)) {
            result = NXE_LINK_DOWN;
            break;
        }
        uint8_t mac[6];
        uint64_t left = deadline - now;
        int ar = net_arp_resolve(ns, net_next_hop(ns, ns->cfg.dns_ip),
                                 left < per_try ? (uint32_t)left : per_try, mac);
        if (ar != NXE_OK) {
            result = ar;
            continue;
        }
        if (attempt)
            ns->st.dns_retries++;
        ns->st.dns_queries++;
        int sr = udp_sendto(ns, s, ns->cfg.dns_ip, ns->cfg.dns_port, q, qlen);
        if (sr != NXE_OK && sr != NXE_ARP_TIMEOUT) {
            result = sr;
            break;
        }
        uint64_t until = net_now(ns) + per_try;
        if (until > deadline)
            until = deadline;
        for (;;) {
            now = net_now(ns);
            if (now >= until)
                break;
            uint32_t from;
            uint16_t fport;
            uint32_t n = udp_recv(ns, s, r, sizeof(r), (uint32_t)(until - now), &from, &fport);
            if (!n)
                break;
            if (from != ns->cfg.dns_ip || fport != ns->cfg.dns_port)
                continue;
            int pr = dns_parse_response(r, n, id, ip);
            if (pr == NXE_DNS_BAD_REPLY && n >= 2 && rd16(r) != id)
                continue; /* an answer to an older query */
            udp_close(ns, s);
            return pr;
        }
        result = NXE_DNS_TIMEOUT;
    }
    udp_close(ns, s);
    return result;
}
