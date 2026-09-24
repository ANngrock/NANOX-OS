/*
 * The provider protocol of M5 (docs/m5-net.md §8): request bodies and the
 * streamed response of a Messages-API-style endpoint (POST /v1/messages
 * with "stream": true), as documented for the Anthropic API:
 *
 *   message_start        {"message":{"id","model","usage":{"input_tokens"}}}
 *   content_block_start  {"index":i,"content_block":{"type":"text"|"tool_use",...}}
 *   content_block_delta  {"index":i,"delta":{"type":"text_delta","text"}
 *                                         |{"type":"input_json_delta","partial_json"}}
 *   content_block_stop   {"index":i}
 *   message_delta        {"delta":{"stop_reason"},"usage":{"output_tokens"}}
 *   message_stop
 *   ping                 (ignored)
 *   error                {"error":{"type","message"}}
 *
 * The assembler checks the order of events and the shape of each payload;
 * anything else is a malformed response (provider class).  Content blocks
 * of other types (thinking and the like) are skipped and not sent back.
 *
 * The conversation is kept as serialized JSON message objects, so the
 * request body is built without a tree.  Pure code: linked into bin/core
 * and compiled into the host tests.
 */
#ifndef NANOX_LIB_MESSAGES_H
#define NANOX_LIB_MESSAGES_H

#include <stdint.h>

#include <nanox/json.h>

#define MSG_BLOCKS_MAX 8u
#define MSG_ARENA 8192u
#define MSG_NAME_MAX 64u

enum msg_block_type { MB_TEXT = 1, MB_TOOL_USE, MB_OTHER };

struct msg_block {
    int type;
    char id[MSG_NAME_MAX];   /* tool_use */
    char name[MSG_NAME_MAX]; /* tool_use */
    uint32_t off, len;       /* text, or the tool input JSON, in the arena */
    int closed;
};

struct msg_stream {
    int started, stopped;   /* message_start / message_stop seen */
    int open;               /* index of the open block, -1 none */
    char msg_id[MSG_NAME_MAX];
    char model[MSG_NAME_MAX];
    char stop_reason[32];
    char error_type[48];    /* of an "error" event */
    uint64_t in_tokens, out_tokens;
    uint32_t nblocks;
    struct msg_block b[MSG_BLOCKS_MAX];
    char arena[MSG_ARENA];
    uint32_t arena_len;
    uint64_t events;
};

void msg_stream_init(struct msg_stream *m);
/* One event of the stream: NXE_OK, NXE_PRV_MALFORMED (unexpected event or
 * payload), NXE_PRV_TOO_LARGE, or for an "error" event NXE_PRV_OVERLOADED /
 * NXE_PRV_RATE_LIMIT / NXE_PRV_SERVER / NXE_PRV_STREAM_ERROR by its type. */
int msg_event(struct msg_stream *m, const char *event, const char *data, uint32_t len);
/* After message_stop: checks the result (every block closed, a stop
 * reason, tool inputs are JSON objects): NXE_OK, NXE_PRV_MALFORMED or
 * NXE_PRV_BAD_TOOL; NXE_PRV_TRUNCATED if message_stop never came. */
int msg_finish(struct msg_stream *m);
/* The provider error of a non-2xx response body {"type":"error","error":
 * {"type":...}}: copies the type (or "-") into out. */
void msg_error_type(const char *body, uint32_t len, char *out, uint32_t cap);
/* The error an "error" event or body type maps to (see msg_event). */
int msg_error_code(const char *type);
/* Text of block i as a span of the arena. */
struct jspan msg_block_span(const struct msg_stream *m, uint32_t i);

/* ---- conversation ---- */

struct msg_conv {
    struct jw w;       /* message objects separated by commas */
    uint32_t nmsg;
    int last_role;     /* 0 none, 1 user, 2 assistant */
    int pending_user;  /* a user message is open (tool results are added) */
};

void conv_init(struct msg_conv *c, char *mem, uint32_t cap);
/* {"role":"user","content":"<text>"} */
void conv_user_text(struct msg_conv *c, const char *text, uint32_t len);
/* The assistant turn m as it came (text and tool_use blocks). */
void conv_assistant(struct msg_conv *c, const struct msg_stream *m);
/* A tool_result block; consecutive results share one user message. */
void conv_tool_result(struct msg_conv *c, const char *tool_use_id, const char *text,
                      uint32_t len, int is_error);
/* 1 if the conversation text is complete and fits. */
int conv_ok(struct msg_conv *c);

/* The request body: {"model","max_tokens","system","tools":<tools_json>,
 * "messages":[...],"stream":true}.  tools_json is a JSON array (built by
 * the caller).  0 ok, -1 does not fit. */
int msg_request_body(struct jw *out, const char *model, uint32_t max_tokens, const char *system,
                     const char *tools_json, struct msg_conv *c);

#endif
