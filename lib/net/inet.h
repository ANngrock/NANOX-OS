/* Internal helpers of the network stack (lib/net). */
#ifndef NANOX_LIB_NET_INET_H
#define NANOX_LIB_NET_INET_H

#include <stdint.h>

#include <nanox/net.h>

#define ETH_HDR 14u
#define ETH_ARP 0x0806u
#define ETH_IPV4 0x0800u
#define IP_HDR 20u
#define IP_ICMP 1u
#define IP_TCP 6u
#define IP_UDP 17u
#define UDP_HDR 8u
#define TCP_HDR 20u

#define TCP_FIN 0x01u
#define TCP_SYN 0x02u
#define TCP_RST 0x04u
#define TCP_PSH 0x08u
#define TCP_ACK 0x10u

static inline uint16_t rd16(const uint8_t *p)
{
    return (uint16_t)((uint16_t)p[0] << 8 | p[1]);
}
static inline uint32_t rd32(const uint8_t *p)
{
    return (uint32_t)p[0] << 24 | (uint32_t)p[1] << 16 | (uint32_t)p[2] << 8 | p[3];
}
static inline void wr16(uint8_t *p, uint16_t v)
{
    p[0] = (uint8_t)(v >> 8);
    p[1] = (uint8_t)v;
}
static inline void wr32(uint8_t *p, uint32_t v)
{
    p[0] = (uint8_t)(v >> 24);
    p[1] = (uint8_t)(v >> 16);
    p[2] = (uint8_t)(v >> 8);
    p[3] = (uint8_t)v;
}

/* Sequence number comparisons (RFC 793, modulo 2^32). */
static inline int seq_lt(uint32_t a, uint32_t b)
{
    return (int32_t)(a - b) < 0;
}
static inline int seq_le(uint32_t a, uint32_t b)
{
    return (int32_t)(a - b) <= 0;
}

/* Sum of the TCP/UDP pseudo header (not folded). */
static inline uint32_t pseudo_sum(uint32_t src, uint32_t dst, uint8_t proto, uint32_t len)
{
    return (src >> 16) + (src & 0xFFFFu) + (dst >> 16) + (dst & 0xFFFFu) + proto + len;
}

/* Sends an IPv4 packet whose payload (len bytes) is already at
 * ns->tx + ETH_HDR + IP_HDR.  NXE_OK, NXE_ARP_TIMEOUT (next hop unknown:
 * an ARP request went out, the packet was dropped), NXE_LINK_DOWN,
 * NXE_NET_IO. */
int ip_output(struct netstack *ns, uint32_t dst, uint8_t proto, uint32_t len);
/* TCP input (tcp.c): segment of len bytes (header included) from src. */
void tcp_input(struct netstack *ns, uint32_t src, uint32_t dst, const uint8_t *seg, uint32_t len);
/* TCP timers (tcp.c). */
void tcp_timers(struct netstack *ns);
uint64_t net_now(struct netstack *ns);

#endif
