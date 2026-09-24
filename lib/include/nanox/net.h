/*
 * Network stack of bin/core (M5): Ethernet II, ARP, IPv4, ICMP echo, UDP,
 * DNS (A records) and TCP.  Pure code over an abstract frame device (no
 * system calls): linked into bin/core on top of the virtio-net system
 * calls, compiled into the host tests on top of simulated links.
 * Normative description and deliberate simplifications: docs/m5-net.md §4.
 *
 * Single-threaded and poll driven: every blocking helper loops over
 * net_poll() (receive one frame, dispatch it, run the timers) until its
 * condition holds or its deadline passes.  All times are milliseconds of
 * ops.now_ms.
 */
#ifndef NANOX_LIB_NET_H
#define NANOX_LIB_NET_H

#include <stdint.h>

#include <nanox/nerr.h>

#define NET_FRAME_MAX 1514u
#define NET_MTU 1500u
#define NET_TCP_MSS 1460u
#define NET_ARP_ENTRIES 8u
#define NET_UDP_SOCKS 4u
#define NET_TCP_CONNS 4u
#define NET_TCP_SNDBUF 16384u
#define NET_TCP_RCVBUF 32768u
#define NET_TCP_CWND_SEGS 10u   /* fixed send window cap, no congestion control */
#define NET_TCP_OOO 8u          /* out-of-order segments kept by the receiver */
#define NET_TCP_RTO_INIT 1000u
#define NET_TCP_RTO_MIN 200u
#define NET_TCP_RTO_MAX 8000u
#define NET_TCP_SYN_RETRIES 5u
#define NET_TCP_DATA_RETRIES 7u
#define NET_TCP_TIME_WAIT 500u  /* shortened 2*MSL (client ports are not reused soon) */
#define NET_ARP_RETRY_MS 500u
#define NET_ARP_TTL_MS 600000u

struct net_ops {
    void *ctx;
    /* NXE_OK, NXE_LINK_DOWN (nothing sent), NXE_NET_IO */
    int (*send)(void *ctx, const uint8_t *frame, uint32_t len);
    /* One frame into buf (NET_FRAME_MAX bytes): its length, 0 if none
     * arrived within wait_ms. */
    uint32_t (*recv)(void *ctx, uint8_t *buf, uint32_t wait_ms);
    uint64_t (*now_ms)(void *ctx);
    int (*link_up)(void *ctx);
    /* Unpredictable 32-bit values (TCP initial sequence numbers, ports,
     * DNS ids): the CSPRNG in bin/core. */
    uint32_t (*random32)(void *ctx);
};

struct net_config {
    uint8_t mac[6];
    uint32_t ip, mask, gw; /* host byte order */
    uint32_t dns_ip;
    uint16_t dns_port;
};

struct net_stats {
    uint64_t rx_frames, tx_frames, rx_bad, tx_fail, tx_link_down;
    uint64_t arp_rx, arp_tx, arp_replies, arp_unresolved;
    uint64_t ip_rx, ip_tx, ip_bad_csum, ip_frag_dropped, ip_not_ours;
    uint64_t icmp_rx, icmp_echo_replied;
    uint64_t udp_rx, udp_tx, udp_no_socket;
    uint64_t dns_queries, dns_retries;
    uint64_t tcp_rx, tcp_tx, tcp_bad_csum, tcp_retransmits, tcp_rst_rx, tcp_rst_tx;
    uint64_t tcp_out_of_order, tcp_dup_acks, tcp_conns_opened;
};

enum tcp_state {
    TCP_CLOSED = 0,
    TCP_LISTEN,
    TCP_SYN_SENT,
    TCP_SYN_RCVD,
    TCP_ESTABLISHED,
    TCP_FIN_WAIT_1,
    TCP_FIN_WAIT_2,
    TCP_CLOSING,
    TCP_TIME_WAIT,
    TCP_CLOSE_WAIT,
    TCP_LAST_ACK,
};

struct tcp_conn {
    int used;
    int state;
    int err;             /* sticky NXE_* once the connection failed */
    int listener;        /* a listening slot (passive open, host tests) */
    int accepted;        /* child of a listener, not yet taken by tcp_accept */
    uint32_t rip;
    uint16_t lport, rport;
    uint32_t iss, snd_una, snd_nxt, snd_max, snd_wnd;
    uint32_t irs, rcv_nxt;
    uint16_t mss;
    /* unacknowledged and unsent data, starting at sequence sbuf_seq */
    uint8_t sbuf[NET_TCP_SNDBUF];
    uint32_t sbuf_seq, sbuf_len;
    int fin_queued, fin_sent, fin_acked, fin_received;
    int fin_seq_valid;
    uint32_t fin_seq;
    int orphan;          /* closed by the owner, finishing the close */
    uint64_t orphan_until;
    uint8_t rbuf[NET_TCP_RCVBUF];
    uint32_t rbuf_head, rbuf_len;
    /* retransmission */
    uint32_t rto, retries;
    uint64_t retx_at;    /* 0: timer off */
    uint64_t tw_until;
    int rtt_active;
    uint32_t rtt_seq;
    uint64_t rtt_start;
    uint32_t srtt, rttvar; /* 0: no sample yet */
    uint32_t dupacks, recover;
    int in_recovery;       /* fast retransmit done, until snd_max at that time is acked */
    struct {
        uint32_t seq, len; /* len 0: free */
        uint8_t data[NET_TCP_MSS];
    } ooo[NET_TCP_OOO];
    uint64_t retransmits, bytes_in, bytes_out;
};

struct udp_sock {
    int used;
    uint16_t lport;
    int have;
    uint32_t from_ip;
    uint16_t from_port;
    uint32_t len;
    uint8_t data[NET_MTU];
};

struct netstack {
    struct net_ops ops;
    struct net_config cfg;
    int noretx; /* negative control: TCP never retransmits */
    struct {
        int valid;
        uint32_t ip;
        uint8_t mac[6];
        uint64_t t;
    } arp[NET_ARP_ENTRIES];
    uint64_t arp_last_req;
    uint32_t arp_last_ip;
    struct udp_sock udp[NET_UDP_SOCKS];
    struct tcp_conn tcp[NET_TCP_CONNS];
    uint16_t next_port;
    uint16_t ip_id;
    struct {
        uint16_t id, seq;
        int got;
        uint32_t from;
    } ping;
    struct net_stats st;
    uint8_t rx[NET_FRAME_MAX];
    uint8_t tx[NET_FRAME_MAX];
};

void net_init(struct netstack *ns, const struct net_ops *ops, const struct net_config *cfg);
/* Receives at most one frame (waiting up to wait_ms), dispatches it, runs
 * the TCP timers. */
void net_poll(struct netstack *ns, uint32_t wait_ms);

/* Dotted quad -> host order address; 1 ok, 0 malformed. */
int net_parse_ip(const char *s, uint32_t *out);
/* "a.b.c.d" into out (16 bytes). */
void net_format_ip(uint32_t ip, char out[16]);

/* Next hop of ip: ip itself on the local subnet, else the gateway. */
uint32_t net_next_hop(const struct netstack *ns, uint32_t ip);
/* Blocking ARP resolution (retries every NET_ARP_RETRY_MS). */
int net_arp_resolve(struct netstack *ns, uint32_t ip, uint32_t timeout_ms, uint8_t mac[6]);
/* ICMP echo; *rtt_ms set on success. */
int net_ping(struct netstack *ns, uint32_t ip, uint32_t timeout_ms, uint32_t *rtt_ms);

/* UDP: a socket on lport (0: an ephemeral port); -1 when none is free. */
int udp_open(struct netstack *ns, uint16_t lport);
void udp_close(struct netstack *ns, int s);
int udp_sendto(struct netstack *ns, int s, uint32_t ip, uint16_t port, const void *data,
               uint32_t len);
/* Datagram length, 0 on timeout. */
uint32_t udp_recv(struct netstack *ns, int s, void *buf, uint32_t cap, uint32_t timeout_ms,
                  uint32_t *from_ip, uint16_t *from_port);

/* DNS A query to cfg.dns_ip:dns_port (3 tries within timeout_ms). */
int net_dns_a(struct netstack *ns, const char *name, uint32_t timeout_ms, uint32_t *ip);
/* Pure DNS message code (host-tested): builds a query for `name` (RD set)
 * into out, returns its length or 0; parses a response for query `id`:
 * NXE_OK and the first A record, NXE_DNS_NOTFOUND, NXE_DNS_BAD_REPLY. */
uint32_t dns_build_query(uint16_t id, const char *name, uint8_t *out, uint32_t cap);
int dns_parse_response(const uint8_t *msg, uint32_t len, uint16_t id, uint32_t *ip);

/* TCP.  Connections are slots 0..NET_TCP_CONNS-1; a negative return is
 * -NXE_*. */
int tcp_connect(struct netstack *ns, uint32_t ip, uint16_t port, uint32_t timeout_ms);
/* Passive open (used by the host tests): a listening slot on lport. */
int tcp_listen(struct netstack *ns, uint16_t lport);
/* An established connection accepted on listener l, or -1. */
int tcp_accept(struct netstack *ns, int l);
/* Queues all of buf (waits for send-buffer space): NXE_OK or the
 * connection's error; NXE_NET_TIMEOUT if not all fitted in time. */
int tcp_write(struct netstack *ns, int c, const void *buf, uint32_t len, uint32_t timeout_ms);
/* Bytes read (> 0); 0 at the peer's FIN; -NXE_NET_TIMEOUT when nothing
 * arrived in time (the connection stays usable); -NXE_* when it failed. */
int tcp_read(struct netstack *ns, int c, void *buf, uint32_t cap, uint32_t timeout_ms);
/* Half close: queues our FIN after the data (reading stays possible). */
void tcp_shutdown(struct netstack *ns, int c);
/* Graceful close (FIN), waits until the FIN is acknowledged or timeout,
 * then releases the slot. */
void tcp_close(struct netstack *ns, int c, uint32_t timeout_ms);
/* Sends RST and releases the slot at once. */
void tcp_abort(struct netstack *ns, int c);
/* Sent bytes (and FIN) not acknowledged by the peer yet. */
uint32_t tcp_unacked(const struct netstack *ns, int c);
const char *tcp_state_name(int state);

/* Internet checksum (RFC 1071) over data, continuing `sum` (0 to start);
 * returns the folded, complemented 16-bit value. */
uint16_t net_checksum(const void *data, uint32_t len, uint32_t sum);

#endif
