/*
 * TCP of bin/core's network stack (RFC 793/9293 core, RFC 6298 timer).
 * See nanox/net.h and docs/m5-net.md §4 for the deliberate
 * simplifications: no congestion control (a fixed cap of
 * NET_TCP_CWND_SEGS segments in flight), no window scaling, SACK or
 * timestamps, out-of-order segments are dropped (the peer retransmits),
 * every segment is acknowledged at once (no delayed ACK), TIME_WAIT is
 * shortened, go-back-N retransmission from the oldest unacknowledged byte.
 */
#include <nanox/net.h>
#include <nanox/string.h>

#include "inet.h"

const char *tcp_state_name(int s)
{
    static const char *const names[] = {"CLOSED",     "LISTEN",     "SYN_SENT",   "SYN_RCVD",
                                        "ESTABLISHED", "FIN_WAIT_1", "FIN_WAIT_2", "CLOSING",
                                        "TIME_WAIT",  "CLOSE_WAIT", "LAST_ACK"};
    return s >= 0 && s <= TCP_LAST_ACK ? names[s] : "?";
}

static struct tcp_conn *conn(struct netstack *ns, int c)
{
    if (c < 0 || c >= (int)NET_TCP_CONNS || !ns->tcp[c].used)
        return 0;
    return &ns->tcp[c];
}

static uint32_t rcv_window(const struct tcp_conn *t)
{
    uint32_t free = NET_TCP_RCVBUF - t->rbuf_len;
    return free > 65535u ? 65535u : free;
}

/* Builds and sends one segment.  seq: its sequence number; data: from the
 * send buffer at seq (len bytes). */
static int send_segment(struct netstack *ns, struct tcp_conn *t, uint32_t seq, uint8_t flags,
                        uint32_t len)
{
    uint8_t *h = ns->tx + ETH_HDR + IP_HDR;
    uint32_t hl = TCP_HDR + ((flags & TCP_SYN) ? 4u : 0u);
    wr16(h, t->lport);
    wr16(h + 2, t->rport);
    wr32(h + 4, seq);
    wr32(h + 8, (flags & TCP_ACK) ? t->rcv_nxt : 0);
    h[12] = (uint8_t)((hl / 4u) << 4);
    h[13] = flags;
    wr16(h + 14, (uint16_t)rcv_window(t));
    wr16(h + 16, 0);
    wr16(h + 18, 0);
    if (flags & TCP_SYN) { /* MSS option */
        h[20] = 2;
        h[21] = 4;
        wr16(h + 22, NET_TCP_MSS);
    }
    if (len) {
        uint32_t off = seq - t->sbuf_seq;
        memcpy(h + hl, t->sbuf + off, len);
    }
    uint32_t total = hl + len;
    wr16(h + 16, net_checksum(h, total, pseudo_sum(ns->cfg.ip, t->rip, IP_TCP, total)));
    ns->st.tcp_tx++;
    if (flags & TCP_RST)
        ns->st.tcp_rst_tx++;
    return ip_output(ns, t->rip, IP_TCP, total);
}

/* RST answering a segment that has no connection (RFC 793 "reset
 * generation"). */
static void send_reset(struct netstack *ns, uint32_t dst, uint16_t sport, uint16_t dport,
                       uint32_t seq, uint32_t ack, int with_ack)
{
    uint8_t *h = ns->tx + ETH_HDR + IP_HDR;
    wr16(h, sport);
    wr16(h + 2, dport);
    wr32(h + 4, seq);
    wr32(h + 8, ack);
    h[12] = 5u << 4;
    h[13] = (uint8_t)(TCP_RST | (with_ack ? TCP_ACK : 0));
    wr16(h + 14, 0);
    wr16(h + 16, 0);
    wr16(h + 18, 0);
    wr16(h + 16, net_checksum(h, TCP_HDR, pseudo_sum(ns->cfg.ip, dst, IP_TCP, TCP_HDR)));
    ns->st.tcp_tx++;
    ns->st.tcp_rst_tx++;
    ip_output(ns, dst, IP_TCP, TCP_HDR);
}

static void arm_timer(struct netstack *ns, struct tcp_conn *t)
{
    if (!t->retx_at)
        t->retx_at = net_now(ns) + t->rto;
}

static void fail_conn(struct tcp_conn *t, int err)
{
    if (!t->err)
        t->err = err;
    t->state = TCP_CLOSED;
    t->retx_at = 0;
}

/* Sends what the window allows: SYN, data, FIN; a pure ACK if `ack` and
 * nothing else went out. */
static void output(struct netstack *ns, struct tcp_conn *t, int ack)
{
    int sent = 0;
    if (t->state == TCP_SYN_SENT || t->state == TCP_SYN_RCVD) {
        if (seq_lt(t->snd_nxt, t->iss + 1)) {
            uint8_t fl = t->state == TCP_SYN_SENT ? TCP_SYN : (uint8_t)(TCP_SYN | TCP_ACK);
            send_segment(ns, t, t->iss, fl, 0);
            t->snd_nxt = t->iss + 1;
            if (seq_lt(t->snd_max, t->snd_nxt))
                t->snd_max = t->snd_nxt;
            arm_timer(ns, t);
        }
        return;
    }
    if (t->state != TCP_ESTABLISHED && t->state != TCP_CLOSE_WAIT &&
        t->state != TCP_FIN_WAIT_1 && t->state != TCP_CLOSING && t->state != TCP_LAST_ACK) {
        if (ack)
            send_segment(ns, t, t->snd_nxt, TCP_ACK, 0);
        return;
    }
    uint32_t cap = NET_TCP_CWND_SEGS * t->mss;
    uint32_t wnd = t->snd_wnd < cap ? t->snd_wnd : cap;
    uint32_t data_end = t->sbuf_seq + t->sbuf_len;
    for (;;) {
        uint32_t in_flight = t->snd_nxt - t->snd_una;
        if (seq_lt(t->snd_nxt, data_end)) {
            uint32_t avail = data_end - t->snd_nxt;
            uint32_t room = wnd > in_flight ? wnd - in_flight : 0;
            if (room == 0) {
                if (t->snd_wnd == 0 && in_flight == 0) {
                    /* zero window: a one-byte probe, retransmitted by the timer */
                    avail = 1;
                    room = 1;
                } else
                    break;
            }
            uint32_t n = avail < room ? avail : room;
            if (n > t->mss)
                n = t->mss;
            if (t->snd_wnd == 0) {
                /* zero-window probe: the byte goes out, but snd_nxt stays, so
                 * later segments keep the sequence number the peer expects;
                 * an ACK that covers it is valid through snd_max */
                send_segment(ns, t, t->snd_nxt, TCP_ACK | TCP_PSH, 1);
                if (seq_lt(t->snd_max, t->snd_nxt + 1))
                    t->snd_max = t->snd_nxt + 1;
                arm_timer(ns, t);
                sent = 1;
                break;
            }
            if (!t->rtt_active && t->snd_nxt == t->snd_max) {
                t->rtt_active = 1;
                t->rtt_seq = t->snd_nxt + n;
                t->rtt_start = net_now(ns);
            }
            send_segment(ns, t, t->snd_nxt, TCP_ACK | TCP_PSH, n);
            t->bytes_out += n;
            t->snd_nxt += n;
            if (seq_lt(t->snd_max, t->snd_nxt))
                t->snd_max = t->snd_nxt;
            arm_timer(ns, t);
            sent = 1;
            continue;
        }
        if (t->fin_queued && !t->fin_sent && t->snd_nxt == data_end) {
            send_segment(ns, t, t->snd_nxt, TCP_ACK | TCP_FIN, 0);
            t->fin_sent = 1;
            t->fin_seq = t->snd_nxt;
            t->fin_seq_valid = 1;
            t->snd_nxt++;
            if (seq_lt(t->snd_max, t->snd_nxt))
                t->snd_max = t->snd_nxt;
            if (t->state == TCP_ESTABLISHED)
                t->state = TCP_FIN_WAIT_1;
            else if (t->state == TCP_CLOSE_WAIT)
                t->state = TCP_LAST_ACK;
            arm_timer(ns, t);
            sent = 1;
        }
        break;
    }
    if (ack && !sent)
        send_segment(ns, t, t->snd_nxt, TCP_ACK, 0);
}

static void rtt_sample(struct tcp_conn *t, uint32_t r)
{
    if (!t->srtt) {
        t->srtt = r ? r : 1;
        t->rttvar = r / 2;
    } else {
        uint32_t d = t->srtt > r ? t->srtt - r : r - t->srtt;
        t->rttvar = (3u * t->rttvar + d) / 4u;
        t->srtt = (7u * t->srtt + r) / 8u;
    }
    uint32_t rto = t->srtt + (4u * t->rttvar > 10u ? 4u * t->rttvar : 10u);
    t->rto = rto < NET_TCP_RTO_MIN ? NET_TCP_RTO_MIN : rto > NET_TCP_RTO_MAX ? NET_TCP_RTO_MAX : rto;
}

/* Retransmits the segment at snd_una at once (fast retransmit, NewReno
 * partial acknowledgement), without touching snd_nxt. */
static void resend_first(struct netstack *ns, struct tcp_conn *t)
{
    if (ns->noretx)
        return;
    uint32_t data_end = t->sbuf_seq + t->sbuf_len;
    if (seq_lt(t->snd_una, data_end) && t->snd_una == t->sbuf_seq) {
        uint32_t n = data_end - t->snd_una;
        if (n > t->mss)
            n = t->mss;
        send_segment(ns, t, t->snd_una, TCP_ACK | TCP_PSH, n);
    } else if (t->fin_seq_valid && t->snd_una == t->fin_seq) {
        send_segment(ns, t, t->fin_seq, TCP_ACK | TCP_FIN, 0);
    } else {
        return;
    }
    ns->st.tcp_retransmits++;
    t->retransmits++;
    t->rtt_active = 0; /* Karn */
    t->retx_at = 0;
    arm_timer(ns, t);
}

/* Processes the ACK field: drops acknowledged data, notes an acknowledged
 * FIN, takes an RTT sample, restarts the timer; counts duplicate ACKs
 * (`pure`: the segment carried nothing else) for fast retransmit. */
static void process_ack(struct netstack *ns, struct tcp_conn *t, uint32_t ack, uint32_t wnd,
                        int pure)
{
    if (seq_lt(t->snd_una, ack) && seq_le(ack, t->snd_max)) {
        if (seq_lt(t->sbuf_seq, ack)) {
            uint32_t d = ack - t->sbuf_seq;
            if (d > t->sbuf_len)
                d = t->sbuf_len;
            memmove(t->sbuf, t->sbuf + d, t->sbuf_len - d);
            t->sbuf_len -= d;
            t->sbuf_seq += d;
        }
        if (t->fin_seq_valid && seq_lt(t->fin_seq, ack))
            t->fin_acked = 1;
        t->snd_una = ack;
        if (seq_lt(t->snd_nxt, t->snd_una))
            t->snd_nxt = t->snd_una;
        if (t->rtt_active && seq_le(t->rtt_seq, ack)) {
            rtt_sample(t, (uint32_t)(net_now(ns) - t->rtt_start));
            t->rtt_active = 0;
        } else if (t->srtt) {
            rtt_sample(t, t->srtt); /* new data acknowledged: undo the backoff */
        }
        t->retries = 0;
        t->retx_at = 0;
        t->dupacks = 0;
        if (t->snd_una != t->snd_max)
            arm_timer(ns, t);
        if (t->in_recovery) {
            if (seq_lt(ack, t->recover))
                resend_first(ns, t); /* partial ACK: the next hole */
            else
                t->in_recovery = 0;
        }
    } else if (ack == t->snd_una && t->snd_una != t->snd_max && pure) {
        ns->st.tcp_dup_acks++;
        if (wnd == 0)
            t->retries = 0; /* a live peer answering window probes */
        else if (++t->dupacks == 3 && !t->in_recovery) {
            t->in_recovery = 1;
            t->recover = t->snd_max;
            resend_first(ns, t);
        }
    }
    t->snd_wnd = wnd;
}

static void enter_time_wait(struct netstack *ns, struct tcp_conn *t)
{
    t->state = TCP_TIME_WAIT;
    t->retx_at = 0;
    t->tw_until = net_now(ns) + NET_TCP_TIME_WAIT;
}

static void rbuf_append(struct tcp_conn *t, const uint8_t *data, uint32_t n)
{
    for (uint32_t i = 0; i < n; i++)
        t->rbuf[(t->rbuf_head + t->rbuf_len + i) % NET_TCP_RCVBUF] = data[i];
    t->rbuf_len += n;
    t->rcv_nxt += n;
    t->bytes_in += n;
}

/* Keeps an out-of-order segment (beyond rcv_nxt) for later. */
static void ooo_store(struct tcp_conn *t, uint32_t seq, const uint8_t *data, uint32_t len)
{
    if (!seq_lt(t->rcv_nxt, seq) || len > NET_TCP_MSS)
        return;
    int free_slot = -1;
    for (int i = 0; i < (int)NET_TCP_OOO; i++) {
        if (t->ooo[i].len && t->ooo[i].seq == seq)
            return; /* have it */
        if (!t->ooo[i].len && free_slot < 0)
            free_slot = i;
    }
    if (free_slot < 0)
        return;
    t->ooo[free_slot].seq = seq;
    t->ooo[free_slot].len = len;
    memcpy(t->ooo[free_slot].data, data, len);
}

/* Appends kept segments that became contiguous with rcv_nxt. */
static void ooo_merge(struct tcp_conn *t)
{
    for (int progress = 1; progress;) {
        progress = 0;
        for (uint32_t i = 0; i < NET_TCP_OOO; i++) {
            if (!t->ooo[i].len)
                continue;
            uint32_t s0 = t->ooo[i].seq, len = t->ooo[i].len;
            if (seq_le(s0 + len, t->rcv_nxt)) {
                t->ooo[i].len = 0; /* already have all of it */
                continue;
            }
            if (seq_le(s0, t->rcv_nxt)) {
                uint32_t off = t->rcv_nxt - s0, n = len - off;
                uint32_t space = NET_TCP_RCVBUF - t->rbuf_len;
                if (n > space)
                    return;
                rbuf_append(t, t->ooo[i].data + off, n);
                t->ooo[i].len = 0;
                progress = 1;
            }
        }
    }
}

/* State changes once our FIN is acknowledged; 1 if the connection closed. */
static int after_ack(struct netstack *ns, struct tcp_conn *t)
{
    if (!t->fin_acked)
        return 0;
    if (t->state == TCP_FIN_WAIT_1)
        t->state = TCP_FIN_WAIT_2;
    else if (t->state == TCP_CLOSING)
        enter_time_wait(ns, t);
    else if (t->state == TCP_LAST_ACK) {
        t->state = TCP_CLOSED;
        t->retx_at = 0;
        return 1;
    }
    return 0;
}

static struct tcp_conn *find(struct netstack *ns, uint32_t src, uint16_t sport, uint16_t dport)
{
    for (uint32_t i = 0; i < NET_TCP_CONNS; i++) {
        struct tcp_conn *t = &ns->tcp[i];
        if (t->used && !t->listener && t->state != TCP_CLOSED && t->rip == src &&
            t->rport == sport && t->lport == dport)
            return t;
    }
    return 0;
}

static int alloc_slot(struct netstack *ns)
{
    for (int i = 0; i < (int)NET_TCP_CONNS; i++)
        if (!ns->tcp[i].used)
            return i;
    /* an orphan (closed by its owner, still finishing) gives way */
    for (int i = 0; i < (int)NET_TCP_CONNS; i++)
        if (ns->tcp[i].orphan) {
            ns->tcp[i].used = 0;
            return i;
        }
    return -1;
}

static void init_conn(struct netstack *ns, struct tcp_conn *t)
{
    memset(t, 0, sizeof(*t));
    t->used = 1;
    t->mss = 536; /* until the peer's MSS option says otherwise */
    t->rto = NET_TCP_RTO_INIT;
    t->iss = ns->ops.random32(ns->ops.ctx);
    t->snd_una = t->iss;
    t->snd_nxt = t->iss;
    t->snd_max = t->iss;
    t->sbuf_seq = t->iss + 1;
}

static uint16_t parse_mss(const uint8_t *h, uint32_t hl)
{
    uint32_t i = TCP_HDR;
    while (i < hl) {
        uint8_t kind = h[i];
        if (kind == 0)
            break;
        if (kind == 1) {
            i++;
            continue;
        }
        if (i + 1 >= hl || h[i + 1] < 2 || i + h[i + 1] > hl)
            break;
        if (kind == 2 && h[i + 1] == 4) {
            uint16_t m = rd16(h + i + 2);
            return m < 64 ? 64 : m > NET_TCP_MSS ? NET_TCP_MSS : m;
        }
        i += h[i + 1];
    }
    return 536;
}

void tcp_input(struct netstack *ns, uint32_t src, uint32_t dst, const uint8_t *h, uint32_t len)
{
    if (len < TCP_HDR)
        return;
    uint32_t hl = (uint32_t)(h[12] >> 4) * 4u;
    if (hl < TCP_HDR || hl > len)
        return;
    if (net_checksum(h, len, pseudo_sum(src, dst, IP_TCP, len)) != 0) {
        ns->st.tcp_bad_csum++;
        return;
    }
    ns->st.tcp_rx++;
    uint16_t sport = rd16(h), dport = rd16(h + 2);
    uint32_t seq = rd32(h + 4), ack = rd32(h + 8);
    uint8_t fl = h[13];
    uint32_t wnd = rd16(h + 14);
    const uint8_t *data = h + hl;
    uint32_t dlen = len - hl;
    if (fl & TCP_RST)
        ns->st.tcp_rst_rx++;

    struct tcp_conn *t = find(ns, src, sport, dport);
    if (!t) {
        /* a listener (host tests) takes a SYN */
        for (uint32_t i = 0; i < NET_TCP_CONNS && (fl & TCP_SYN) && !(fl & (TCP_ACK | TCP_RST));
             i++) {
            struct tcp_conn *l = &ns->tcp[i];
            if (!l->used || !l->listener || l->lport != dport)
                continue;
            int s = alloc_slot(ns);
            if (s < 0)
                break;
            struct tcp_conn *c = &ns->tcp[s];
            init_conn(ns, c);
            c->accepted = 1;
            c->rip = src;
            c->rport = sport;
            c->lport = dport;
            c->irs = seq;
            c->rcv_nxt = seq + 1;
            c->snd_wnd = wnd;
            c->mss = parse_mss(h, hl);
            c->state = TCP_SYN_RCVD;
            ns->st.tcp_conns_opened++;
            output(ns, c, 0);
            return;
        }
        if (!(fl & TCP_RST)) {
            if (fl & TCP_ACK)
                send_reset(ns, src, dport, sport, ack, 0, 0);
            else
                send_reset(ns, src, dport, sport, 0,
                           seq + dlen + ((fl & TCP_SYN) ? 1u : 0u) + ((fl & TCP_FIN) ? 1u : 0u),
                           1);
        }
        return;
    }

    if (t->state == TCP_SYN_SENT) {
        if ((fl & TCP_ACK) && ack != t->iss + 1) {
            if (!(fl & TCP_RST))
                send_reset(ns, src, dport, sport, ack, 0, 0);
            return;
        }
        if (fl & TCP_RST) {
            if (fl & TCP_ACK)
                fail_conn(t, NXE_CONN_REFUSED);
            return;
        }
        if ((fl & TCP_SYN) && (fl & TCP_ACK)) {
            t->irs = seq;
            t->rcv_nxt = seq + 1;
            t->mss = parse_mss(h, hl);
            t->snd_una = ack;
            t->snd_wnd = wnd;
            t->retries = 0;
            t->retx_at = 0;
            if (t->rtt_active) /* Karn: not after a retransmitted SYN */
                rtt_sample(t, (uint32_t)(net_now(ns) - t->rtt_start));
            t->rtt_active = 0;
            t->state = TCP_ESTABLISHED;
            output(ns, t, 1);
        }
        return;
    }

    /* Sequence acceptability (RFC 793 §3.9): the segment (SYN and FIN
     * count) must overlap the receive window. */
    uint32_t rwnd = rcv_window(t);
    uint32_t seglen = dlen + ((fl & TCP_SYN) ? 1u : 0u) + ((fl & TCP_FIN) ? 1u : 0u);
    int in_window;
    if (seglen == 0)
        in_window = rwnd ? seq_le(t->rcv_nxt, seq) && seq_lt(seq, t->rcv_nxt + rwnd)
                         : seq == t->rcv_nxt;
    else
        in_window = rwnd && ((seq_le(t->rcv_nxt, seq) && seq_lt(seq, t->rcv_nxt + rwnd)) ||
                             (seq_le(t->rcv_nxt, seq + seglen - 1) &&
                              seq_lt(seq + seglen - 1, t->rcv_nxt + rwnd)));
    if (fl & TCP_RST) {
        if (in_window || seq == t->rcv_nxt) {
            if (t->state == TCP_SYN_RCVD)
                fail_conn(t, NXE_CONN_REFUSED);
            else if (t->state == TCP_TIME_WAIT || t->state == TCP_LAST_ACK ||
                     t->state == TCP_CLOSING)
                t->state = TCP_CLOSED;
            else
                fail_conn(t, NXE_CONN_RESET);
        }
        return;
    }
    if (!in_window) {
        ns->st.tcp_out_of_order++;
        /* RFC 9293 §3.10.7.4: valid ACKs are taken even from segments
         * outside the window (a zero window accepts nothing else) */
        if ((fl & TCP_ACK) && t->state != TCP_SYN_RCVD) {
            process_ack(ns, t, ack, wnd, seglen == 0);
            if (after_ack(ns, t))
                return;
        }
        /* a duplicate ACK tells the peer where we are; not for pure ACKs,
         * which would make two peers acknowledge each other forever */
        output(ns, t, seglen != 0);
        return;
    }
    if (fl & TCP_SYN) { /* SYN in the window: the peer restarted (RFC 5961 would challenge) */
        fail_conn(t, NXE_CONN_RESET);
        send_segment(ns, t, t->snd_nxt, TCP_RST, 0);
        return;
    }
    if (!(fl & TCP_ACK))
        return;
    if (t->state == TCP_SYN_RCVD) {
        if (ack != t->iss + 1) {
            send_reset(ns, src, dport, sport, ack, 0, 0);
            return;
        }
        t->state = TCP_ESTABLISHED;
    }
    process_ack(ns, t, ack, wnd, seglen == 0);
    if (after_ack(ns, t))
        return;

    int need_ack = 0;
    if (dlen && seq_lt(seq, t->rcv_nxt)) { /* starts with bytes we already have */
        uint32_t off = t->rcv_nxt - seq;
        if (off >= dlen) {
            dlen = 0;
        } else {
            data += off;
            dlen -= off;
        }
        seq = t->rcv_nxt;
        need_ack = 1;
    }
    if (dlen) {
        if (seq == t->rcv_nxt && (t->state == TCP_ESTABLISHED || t->state == TCP_FIN_WAIT_1 ||
                                  t->state == TCP_FIN_WAIT_2)) {
            uint32_t space = NET_TCP_RCVBUF - t->rbuf_len;
            uint32_t n = dlen < space ? dlen : space;
            rbuf_append(t, data, n);
            if (n < dlen)
                fl &= (uint8_t)~TCP_FIN; /* the FIN lies beyond what we took */
            ooo_merge(t);
        } else {
            ns->st.tcp_out_of_order++;
            if (t->state == TCP_ESTABLISHED || t->state == TCP_FIN_WAIT_1 ||
                t->state == TCP_FIN_WAIT_2)
                ooo_store(t, seq, data, dlen);
            fl &= (uint8_t)~TCP_FIN; /* the peer sends it again */
        }
        need_ack = 1;
    }
    if ((fl & TCP_FIN) && seq + dlen == t->rcv_nxt && !t->fin_received) {
        t->fin_received = 1;
        t->rcv_nxt++;
        need_ack = 1;
        if (t->state == TCP_ESTABLISHED)
            t->state = TCP_CLOSE_WAIT;
        else if (t->state == TCP_FIN_WAIT_1)
            t->state = t->fin_acked ? TCP_TIME_WAIT : TCP_CLOSING;
        else if (t->state == TCP_FIN_WAIT_2)
            t->state = TCP_TIME_WAIT;
        if (t->state == TCP_TIME_WAIT)
            enter_time_wait(ns, t);
    } else if ((fl & TCP_FIN) && t->fin_received) {
        need_ack = 1; /* retransmitted FIN */
    }
    output(ns, t, need_ack);
}

void tcp_timers(struct netstack *ns)
{
    uint64_t now = net_now(ns);
    for (uint32_t i = 0; i < NET_TCP_CONNS; i++) {
        struct tcp_conn *t = &ns->tcp[i];
        if (!t->used)
            continue;
        if (t->state == TCP_TIME_WAIT && now >= t->tw_until)
            t->state = TCP_CLOSED;
        if (t->orphan && (t->state == TCP_CLOSED || now >= t->orphan_until)) {
            t->used = 0; /* closed by its owner, finished (or given up) */
            continue;
        }
        if (!t->retx_at || now < t->retx_at)
            continue;
        t->retx_at = 0;
        uint32_t limit = (t->state == TCP_SYN_SENT || t->state == TCP_SYN_RCVD)
                             ? NET_TCP_SYN_RETRIES
                             : NET_TCP_DATA_RETRIES;
        if (ns->noretx || ++t->retries > limit) {
            int err = t->state == TCP_SYN_SENT ? NXE_CONN_TIMEOUT : NXE_UNACKED_TIMEOUT;
            if (!ns->ops.link_up(ns->ops.ctx))
                err = NXE_LINK_DOWN;
            if (ns->noretx && t->retries <= limit) {
                /* negative control: the segment is not sent again; the
                 * connection just waits until the caller's deadline */
                t->retx_at = now + NET_TCP_RTO_MAX;
                continue;
            }
            fail_conn(t, err);
            continue;
        }
        /* go back N: resend from the oldest unacknowledged byte */
        t->in_recovery = 0;
        t->dupacks = 0;
        ns->st.tcp_retransmits++;
        t->retransmits++;
        t->rtt_active = 0; /* Karn: no sample from retransmitted data */
        t->rto = t->rto * 2 > NET_TCP_RTO_MAX ? NET_TCP_RTO_MAX : t->rto * 2;
        t->snd_nxt = t->snd_una;
        if (t->fin_sent && !t->fin_acked)
            t->fin_sent = 0; /* output() sends it again after the data */
        output(ns, t, 0);
        arm_timer(ns, t);
    }
}

static int wait_step(struct netstack *ns, uint64_t deadline)
{
    uint64_t now = net_now(ns);
    if (now >= deadline)
        return 0;
    uint64_t left = deadline - now;
    net_poll(ns, left < 20 ? (uint32_t)left : 20);
    return 1;
}

int tcp_connect(struct netstack *ns, uint32_t ip, uint16_t port, uint32_t timeout_ms)
{
    uint64_t deadline = net_now(ns) + timeout_ms;
    if (!ns->ops.link_up(ns->ops.ctx))
        return -NXE_LINK_DOWN;
    uint8_t mac[6];
    int r = net_arp_resolve(ns, net_next_hop(ns, ip), timeout_ms, mac);
    if (r != NXE_OK)
        return -r;
    int s = alloc_slot(ns);
    if (s < 0)
        return -NXE_LOC_RESOURCES;
    struct tcp_conn *t = &ns->tcp[s];
    init_conn(ns, t);
    t->rip = ip;
    t->rport = port;
    for (;;) { /* an ephemeral port no other slot uses with this peer */
        t->lport = ns->next_port++;
        if (ns->next_port < 49152u)
            ns->next_port = 49152u;
        if (!find(ns, ip, port, t->lport) || find(ns, ip, port, t->lport) == t)
            break;
    }
    t->state = TCP_SYN_SENT;
    t->rtt_start = net_now(ns);
    t->rtt_active = 1;
    ns->st.tcp_conns_opened++;
    output(ns, t, 0);
    while (t->state == TCP_SYN_SENT) {
        if (!wait_step(ns, deadline)) {
            int err = ns->ops.link_up(ns->ops.ctx) ? NXE_CONN_TIMEOUT : NXE_LINK_DOWN;
            t->used = 0;
            return -err;
        }
    }
    if (t->state != TCP_ESTABLISHED) {
        int err = t->err ? t->err : NXE_CONN_RESET;
        t->used = 0;
        return -err;
    }
    return s;
}

int tcp_listen(struct netstack *ns, uint16_t lport)
{
    int s = alloc_slot(ns);
    if (s < 0)
        return -NXE_LOC_RESOURCES;
    struct tcp_conn *t = &ns->tcp[s];
    init_conn(ns, t);
    t->listener = 1;
    t->state = TCP_LISTEN;
    t->lport = lport;
    return s;
}

int tcp_accept(struct netstack *ns, int l)
{
    struct tcp_conn *lt = conn(ns, l);
    if (!lt || !lt->listener)
        return -1;
    for (int i = 0; i < (int)NET_TCP_CONNS; i++) {
        struct tcp_conn *t = &ns->tcp[i];
        if (t->used && t->accepted && t->lport == lt->lport && t->state != TCP_SYN_RCVD &&
            t->state != TCP_CLOSED) {
            t->accepted = 0;
            return i;
        }
    }
    return -1;
}

static int can_send(const struct tcp_conn *t)
{
    return (t->state == TCP_ESTABLISHED || t->state == TCP_CLOSE_WAIT) && !t->fin_queued;
}

int tcp_write(struct netstack *ns, int c, const void *buf, uint32_t len, uint32_t timeout_ms)
{
    struct tcp_conn *t = conn(ns, c);
    if (!t)
        return NXE_LOC_BAD_ARG;
    const uint8_t *p = buf;
    uint64_t deadline = net_now(ns) + timeout_ms;
    while (len) {
        if (t->err)
            return t->err;
        if (!can_send(t))
            return NXE_PEER_CLOSED;
        uint32_t space = NET_TCP_SNDBUF - t->sbuf_len;
        if (space) {
            uint32_t n = len < space ? len : space;
            memcpy(t->sbuf + t->sbuf_len, p, n);
            t->sbuf_len += n;
            p += n;
            len -= n;
            output(ns, t, 0);
            continue;
        }
        if (!wait_step(ns, deadline))
            return NXE_NET_TIMEOUT;
    }
    return t->err ? t->err : NXE_OK;
}

int tcp_read(struct netstack *ns, int c, void *buf, uint32_t cap, uint32_t timeout_ms)
{
    struct tcp_conn *t = conn(ns, c);
    if (!t)
        return -NXE_LOC_BAD_ARG;
    uint64_t deadline = net_now(ns) + timeout_ms;
    for (;;) {
        if (t->rbuf_len) {
            uint32_t n = t->rbuf_len < cap ? t->rbuf_len : cap;
            uint8_t *out = buf;
            int opened = rcv_window(t) < t->mss; /* window update after a nearly full buffer */
            for (uint32_t i = 0; i < n; i++)
                out[i] = t->rbuf[(t->rbuf_head + i) % NET_TCP_RCVBUF];
            t->rbuf_head = (t->rbuf_head + n) % NET_TCP_RCVBUF;
            t->rbuf_len -= n;
            if (opened && t->state != TCP_CLOSED)
                output(ns, t, 1);
            return (int)n;
        }
        if (t->err)
            return -t->err;
        if (t->fin_received)
            return 0;
        if (t->state == TCP_CLOSED)
            return -NXE_CONN_RESET;
        if (!wait_step(ns, deadline))
            return -NXE_NET_TIMEOUT;
    }
}

void tcp_shutdown(struct netstack *ns, int c)
{
    struct tcp_conn *t = conn(ns, c);
    if (!t || (t->state != TCP_ESTABLISHED && t->state != TCP_CLOSE_WAIT) || t->fin_queued)
        return;
    t->fin_queued = 1;
    output(ns, t, 0);
}

void tcp_close(struct netstack *ns, int c, uint32_t timeout_ms)
{
    struct tcp_conn *t = conn(ns, c);
    if (!t)
        return;
    uint64_t deadline = net_now(ns) + timeout_ms;
    if (t->state == TCP_ESTABLISHED || t->state == TCP_CLOSE_WAIT) {
        tcp_shutdown(ns, c);
        while (t->state != TCP_CLOSED && t->state != TCP_TIME_WAIT &&
               t->state != TCP_FIN_WAIT_2 && !t->err)
            if (!wait_step(ns, deadline))
                break;
    }
    if (t->state == TCP_FIN_WAIT_2 || t->state == TCP_TIME_WAIT) {
        /* orderly: stay to acknowledge the peer's FIN, then free the slot */
        t->orphan = 1;
        t->orphan_until = net_now(ns) + 2000u;
        return;
    }
    if (t->state != TCP_CLOSED && !t->err)
        send_segment(ns, t, t->snd_nxt, TCP_RST | TCP_ACK, 0);
    t->used = 0;
}

void tcp_abort(struct netstack *ns, int c)
{
    struct tcp_conn *t = conn(ns, c);
    if (!t)
        return;
    if (t->state != TCP_CLOSED && t->state != TCP_LISTEN && t->state != TCP_TIME_WAIT)
        send_segment(ns, t, t->snd_nxt, TCP_RST | TCP_ACK, 0);
    t->used = 0;
}

uint32_t tcp_unacked(const struct netstack *ns, int c)
{
    if (c < 0 || c >= (int)NET_TCP_CONNS || !ns->tcp[c].used)
        return 0;
    const struct tcp_conn *t = &ns->tcp[c];
    return t->sbuf_len + (t->fin_queued && !t->fin_acked ? 1u : 0u);
}
