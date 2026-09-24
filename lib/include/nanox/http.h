/*
 * HTTP/1.1 client side of the M5 provider path (RFC 9112): an incremental
 * response parser (status line, headers, Content-Length, chunked transfer
 * coding, body until close) and a Server-Sent Events parser (WHATWG HTML,
 * "event stream" format) for the provider's streamed responses
 * (docs/m5-net.md §8).  Both take the received bytes in pieces of any size,
 * so a response split at any byte boundary is parsed the same way.
 *
 * Pure code: linked into bin/core and compiled into the host tests.
 */
#ifndef NANOX_LIB_HTTP_H
#define NANOX_LIB_HTTP_H

#include <stdint.h>

#define HTTP_HEAD_MAX 4096u
#define SSE_DATA_MAX 8192u
#define SSE_LINE_MAX 8192u

/* ---- response ---- */

enum http_result {
    HTTP_MORE = 0, /* all input consumed, the message is not complete */
    HTTP_DONE = 1, /* the message is complete (*used tells how far) */
    /* errors are returned as -NXE_* (nanox/nerr.h) */
};

/* Receives decoded body bytes; returns NXE_OK or an error that stops the
 * parser (and is returned by http_feed as its negative). */
typedef int (*http_sink)(void *ctx, const char *data, uint32_t len);

struct http_resp {
    int state; /* internal */
    char head[HTTP_HEAD_MAX];
    uint32_t head_len;
    uint32_t status;
    int chunked;
    int64_t content_length; /* -1: not given */
    int conn_close;         /* "Connection: close" */
    uint32_t retry_after_s; /* Retry-After in seconds (0: absent or not a number) */
    char content_type[48];  /* lower-cased media type without parameters */
    char request_id[48];    /* "request-id", for telemetry */
    uint64_t body_bytes;    /* decoded body bytes so far */
    /* chunked decoder */
    int cstate;
    uint64_t crem;
    uint32_t cline;
};

void http_resp_init(struct http_resp *r);
/* Feeds n received bytes.  Returns HTTP_MORE, HTTP_DONE (with *used the
 * bytes that belonged to this message) or -NXE_PRV_MALFORMED_HTTP (not a
 * valid HTTP/1.1 response), -NXE_PRV_TOO_LARGE (header section too long) or
 * the sink's error as a negative number. */
int http_feed(struct http_resp *r, const char *in, uint32_t n, uint32_t *used, http_sink sink,
              void *ctx);
/* The connection ended: 1 if that completes the message (a body delimited
 * by the end of the connection), else 0 (truncated). */
int http_eof_completes(const struct http_resp *r);
/* 1 once the status line and all headers were parsed. */
int http_head_done(const struct http_resp *r);

/* ---- event stream ---- */

typedef int (*sse_handler)(void *ctx, const char *event, const char *data, uint32_t len);

struct sse {
    char event[40];
    char data[SSE_DATA_MAX];
    uint32_t data_len;
    int have_data;
    char line[SSE_LINE_MAX];
    uint32_t line_len;
    int skip_lf;  /* the previous line ended with CR: ignore an LF next */
    int overflow; /* a line or an event was longer than the buffers */
    uint64_t events;
};

void sse_init(struct sse *s);
/* Feeds decoded body bytes; calls the handler for every complete event
 * (the data of several "data:" lines joined with '\n', NUL-terminated).
 * Returns NXE_OK, NXE_PRV_TOO_LARGE, or the handler's error. */
int sse_feed(struct sse *s, const char *in, uint32_t n, sse_handler h, void *ctx);

#endif
