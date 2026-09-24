/*
 * bin/core, M5: network stack, CSPRNG, TLS, provider client and agent loop
 * (docs/m5-net.md).  Shared declarations of m5net.c, m5tls.c, provider.c
 * and agent.c; not a library interface.
 */
#ifndef NANOX_CORE_M5_H
#define NANOX_CORE_M5_H

#include <stdint.h>

#include <nanox/drbg.h>
#include <nanox/net.h>
#include <nanox/nerr.h>

#include "core.h"

/* TCP connect timeout: QEMU's user networking repeats a lost SYN-ACK only
 * after about 6 s (its BSD-derived initial retransmission timer), and a
 * repeated SYN is answered with a bare ACK, so a shorter limit would turn
 * one lost frame into a failure (docs/m5-net.md §4). */
#define NET_CONNECT_TIMEOUT_MS 15000u

extern int m5_enabled;
extern struct netstack g_ns;
extern uint64_t m5_flags;

/* Called by umain with the network device handle and the flags (M5 modes
 * only): loads the network configuration from the store, seeds the
 * CSPRNG, brings the stack up. */
void m5_init(uint64_t net_h, uint64_t flags);
/* NCI operations of M5; NULL if `op` is not one of them. */
op_fn m5_op(const char *op);
extern const char M5_OPS[];

uint64_t m5_now_ms(void);
/* Unix seconds of the wall clock (0: unknown). */
uint64_t m5_unix_time(void);
/* Fills buf from the CSPRNG (reseeding from the entropy device when due):
 * NXE_OK or NXE_LOC_ENTROPY. */
int m5_random(void *buf, uint32_t len);
/* A configuration value "cfg/<key>" of the store, NUL-terminated; 1 found,
 * 0 absent (out = def or ""). */
int m5_cfg(const char *key, char *out, uint32_t cap, const char *def);

/* ---- m5tls.c: one TLS connection over the guest's TCP ---- */

struct m5_conn_info {
    uint32_t ip;
    uint16_t suite, sig;
    uint32_t chain, depth, connect_ms, handshake_ms;
    char detail[48];
};
extern struct m5_conn_info m5_conn;

/* Trust anchors from the store (tls/anchor0..3), loaded once. */
uint32_t m5_anchor_count(void);
/* DNS + TCP + TLS 1.3 to host:port (closes a previous connection):
 * NXE_OK or the classified failure. */
int m5_tls_open(const char *host, uint16_t port, uint32_t suites, uint32_t timeout_ms);
int m5_tls_is_open(void);
int m5_tls_write(const void *buf, uint32_t len);
/* as tls_read */
int m5_tls_read(void *buf, uint32_t cap, uint32_t timeout_ms);
void m5_tls_close(int graceful);
/* Bytes written on the connection the peer has not acknowledged yet. */
uint32_t m5_tls_unacked(void);
int net_link_ok(void);
op_fn m5_tls_op(const char *op);

/* ---- provider.c: the provider client ---- */

#define PRV_BODY_MAX 49152u

struct prv_cfg {
    char host[64];
    char model[64];
    char key_ref[32];
    uint32_t port, timeout_ms, attempts, max_tokens, key_len;
    int err; /* NXE_OK, NXE_LOC_CONFIG or NXE_LOC_NO_KEY */
};
extern struct prv_cfg prv;

/* One attempt. */
struct prv_info {
    uint32_t http, retry_after_s, connect_ms;
    int reconnected;
    char error_type[48];
};

/* One model request (all its attempts). */
struct prv_summary {
    uint32_t attempts, retries, reconnects, http;
    char error_type[48];
};

/* Telemetry counters of this boot (telemetry.status). */
struct m5_tel {
    uint64_t by_class[7]; /* indexed by enum nx_err_class; [0] = successes */
    uint64_t model_attempts, retries, reconnects, connects;
    uint64_t asks, asks_ok, actions, actions_failed;
    int last_err, last_class;
};
extern struct m5_tel m5_tel;

/* Loads the provider configuration and the key once: NXE_OK,
 * NXE_LOC_CONFIG or NXE_LOC_NO_KEY. */
int prv_load(void);
/* One model request with retries: NXE_OK with the response in *m. */
struct msg_stream;
int prv_request(const char *ask, uint32_t step, const char *body, uint32_t blen,
                struct msg_stream *m, struct prv_summary *sum);
const char *prv_key_state(void);
/* Counts an outcome in the telemetry counters by its class. */
void m5_tel_count(int err);

/* ---- agent.c: the agent loop and the provider/telemetry operations ---- */

op_fn m5_agent_op(const char *op);

/* Fails the action with the class and name of an M5 error:
 * "FAILED code=<CLASS>_ERROR detail=<name> class=<class>". */
void m5_fail(struct eng_action *a, struct nci_buf *b, int err, const char *effects);
/* The class of err as telemetry reports it (NXC_UNCLASSIFIED for every
 * failure under the negative control m5-flattel). */
int m5_class(int err);

#endif
