/*
 * Host tests of the M5 provider protocol code: JSON (lib/http/json.c),
 * HTTP/1.1 responses and the event stream (lib/http/http.c), and the
 * streamed Messages response, conversation and request body
 * (lib/http/messages.c).  Every stream is also fed split at every byte
 * boundary, since TCP may deliver it in pieces of any size.
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <nanox/http.h>
#include <nanox/json.h>
#include <nanox/messages.h>
#include <nanox/nerr.h>

#include "test.h"

static struct jspan S(const char *s)
{
    struct jspan v = {s, (uint32_t)strlen(s)};
    return v;
}

/* ---- JSON ------------------------------------------------------------------------ */

static void test_json_valid(void)
{
    static const char *good[] = {
        "{}", "[]", "0", "-0.5e+10", "\"a\\u00e9\\n\"", " true ", "null", "false",
        "{\"a\":[1,2,{\"b\":null}],\"c\":\"x\"}", "[\"\\ud83d\\ude00\"]", "1E5", "\"\xd0\xb0\"",
    };
    static const char *bad[] = {
        "", "{", "[1,]", "{\"a\"}", "{\"a\":}", "01", "1.", "-", "\"\\x\"", "\"a", "tru",
        "{\"a\":1,}", "[1 2]", "\"\x01\"", "{a:1}", "1 2", "\"\\u12G4\"", "+1", ".5",
    };
    for (unsigned i = 0; i < sizeof(good) / sizeof(good[0]); i++)
        CHECK(json_valid(good[i], (uint32_t)strlen(good[i])));
    for (unsigned i = 0; i < sizeof(bad) / sizeof(bad[0]); i++) {
        int v = json_valid(bad[i], (uint32_t)strlen(bad[i]));
        CHECK(!v);
        if (v)
            fprintf(stderr, "  accepted: %s\n", bad[i]);
    }
    /* nesting limit */
    char deep[200];
    int n = 0;
    for (int i = 0; i < 40; i++)
        deep[n++] = '[';
    for (int i = 0; i < 40; i++)
        deep[n++] = ']';
    CHECK(!json_valid(deep, (uint32_t)n));
    CHECK(json_valid(deep + 10, (uint32_t)(n - 20)));
}

static void test_json_read(void)
{
    const char *doc = "{ \"type\" : \"x\", \"n\":42, \"esc\\u0061pe\":\"q\\\"\\\\\\/\\t\","
                      " \"arr\":[1,\"two\",{\"k\":3}], \"u\":\"\\ud83d\\ude00\\u00e9\\ud800\" }";
    struct jspan d = S(doc), v;
    char out[64];
    uint64_t u;
    CHECK(json_valid(doc, (uint32_t)strlen(doc)));
    CHECK_EQ_INT(json_type(d), JSON_OBJECT);
    CHECK_EQ_INT(json_member(d, "type", &v), 1);
    CHECK_EQ_INT(json_string(v, out, sizeof(out)), 1);
    CHECK(strcmp(out, "x") == 0);
    CHECK_EQ_INT(json_member(d, "n", &v), 1);
    CHECK(json_u64(v, &u) && u == 42);
    CHECK_EQ_INT(json_member(d, "missing", &v), 0);
    CHECK(json_member_str(d, "escape", out, sizeof(out))); /* escaped key */
    CHECK(strcmp(out, "q\"\\/\t") == 0);
    CHECK(json_member_str(d, "u", out, sizeof(out)));
    CHECK(strcmp(out, "\xf0\x9f\x98\x80\xc3\xa9\xef\xbf\xbd") == 0);
    CHECK_EQ_INT(json_member(d, "arr", &v), 1);
    uint32_t count = 0;
    struct jspan e;
    CHECK_EQ_INT(json_element(v, 1, &count, &e), 1);
    CHECK_EQ_INT(count, 3);
    CHECK(json_string(e, out, sizeof(out)) == 3 && strcmp(out, "two") == 0);
    CHECK_EQ_INT(json_element(v, 3, &count, &e), 0);
    CHECK_EQ_INT(json_element(v, 2, 0, &e), 1);
    CHECK_EQ_INT(json_type(e), JSON_OBJECT);
    {
        uint32_t pos = 0, nm = 0;
        char key[16];
        int r;
        while ((r = json_next_member(d, &pos, key, sizeof(key), &v)) == 1)
            nm++;
        CHECK(r == 0 && nm == 5);
        pos = 0;
        CHECK_EQ_INT(json_next_member(S(" { } "), &pos, key, sizeof(key), &v), 0);
        pos = 0;
        CHECK_EQ_INT(json_next_member(S("{\"a\":1,}"), &pos, key, sizeof(key), &v), 1);
        CHECK_EQ_INT(json_next_member(S("{\"a\":1,}"), &pos, key, sizeof(key), &v), -1);
        pos = 0;
        CHECK_EQ_INT(json_next_member(S("{\"a_very_long_key_name\":1}"), &pos, key, 8, &v),
                     -1);
    }
    CHECK_EQ_INT(json_member(S("[1]"), "a", &v), -1);
    CHECK_EQ_INT(json_member(S("{\"a\" 1}"), "a", &v), -1);
    CHECK_EQ_INT(json_string(S("\"a\\u0000b\""), out, sizeof(out)), -1); /* NUL refused */
    CHECK_EQ_INT(json_string(S("\"abcdef\""), out, 4), -1);                /* does not fit */
    CHECK_EQ_INT(json_string(S("12"), out, sizeof(out)), -1);
    CHECK(!json_u64(S("-1"), &u) && !json_u64(S("1.5"), &u) && !json_u64(S("01"), &u));
}

static void test_json_write(void)
{
    char mem[256], back[256];
    struct jw w;
    jw_init(&w, mem, sizeof(mem));
    const char in[] = "q\"\\\n\r\t\x01\x7f \xd0\xb0 \xff \xe2\x82";
    jw_str(&w, in, sizeof(in) - 1);
    CHECK(!w.overflow);
    CHECK(json_valid(w.p, w.len));
    CHECK(strcmp(w.p, "\"q\\\"\\\\\\n\\r\\t\\u0001\\u007f \xd0\xb0 \\ufffd \\ufffd\\ufffd\"") == 0);
    CHECK(json_string(S(w.p), back, sizeof(back)) > 0);
    CHECK(memcmp(back, "q\"\\\n\r\t\x01\x7f \xd0\xb0 ", 12) == 0);
    jw_init(&w, mem, 8);
    jw_rawz(&w, "1234567");
    CHECK(!w.overflow);
    jw_rawz(&w, "8");
    CHECK(w.overflow); /* no room for the terminating NUL */
    jw_init(&w, mem, sizeof(mem));
    jw_u64(&w, 0);
    jw_rawz(&w, ",");
    jw_u64(&w, 18446744073709551615ull);
    CHECK(strcmp(mem, "0,18446744073709551615") == 0);
}

/* ---- HTTP -------------------------------------------------------------------------- */

static char body[16384];
static uint32_t body_len;

static int collect(void *ctx, const char *data, uint32_t len)
{
    (void)ctx;
    if (body_len + len > sizeof(body))
        return NXE_PRV_TOO_LARGE;
    memcpy(body + body_len, data, len);
    body_len += len;
    return NXE_OK;
}

static struct http_resp resp;

/* Feeds msg in pieces of `step` bytes; returns the final result and sets *used_total. */
static int feed_steps(const char *msg, uint32_t len, uint32_t step, uint32_t *used_total)
{
    http_resp_init(&resp);
    body_len = 0;
    uint32_t off = 0;
    int r = HTTP_MORE;
    while (off < len && r == HTTP_MORE) {
        uint32_t k = len - off < step ? len - off : step, used = 0;
        r = http_feed(&resp, msg + off, k, &used, collect, 0);
        off += used;
        if (r == HTTP_MORE && used != k)
            return -1000;
    }
    *used_total = off;
    return r;
}

static void test_http_responses(void)
{
    const char *cl = "HTTP/1.1 200 OK\r\nContent-Type: Application/JSON; charset=utf-8\r\n"
                     "Content-Length: 11\r\nrequest-id: req_1\r\n\r\nhello worldEXTRA";
    uint32_t used;
    for (uint32_t step = 1; step <= 40; step++) {
        int r = feed_steps(cl, (uint32_t)strlen(cl), step, &used);
        CHECK_EQ_INT(r, HTTP_DONE);
        CHECK_EQ_INT(used, strlen(cl) - 5);
        CHECK(body_len == 11 && memcmp(body, "hello world", 11) == 0);
    }
    CHECK_EQ_INT(resp.status, 200);
    CHECK(strcmp(resp.content_type, "application/json") == 0);
    CHECK(strcmp(resp.request_id, "req_1") == 0);
    CHECK(!resp.conn_close && !resp.chunked);

    const char *ch = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n"
                     "Content-Type: text/event-stream\r\n\r\n"
                     "5;ext=1\r\nhello\r\n1A\r\n abcdefghijklmnopqrstuvwxy\r\n0\r\n"
                     "Trailer: x\r\n\r\nNEXT";
    for (uint32_t step = 1; step <= 64; step++) {
        int r = feed_steps(ch, (uint32_t)strlen(ch), step, &used);
        CHECK_EQ_INT(r, HTTP_DONE);
        CHECK_EQ_INT(used, strlen(ch) - 4);
        CHECK(body_len == 31 && memcmp(body, "hello abcdefghijklmnopqrstuvwxy", 31) == 0);
    }
    CHECK(resp.chunked);

    /* interim 100, then an error with Retry-After and Connection: close */
    const char *e = "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 429 Too Many Requests\r\n"
                    "retry-after: 3\r\nconnection: close\r\ncontent-length: 2\r\n\r\n{}";
    CHECK_EQ_INT(feed_steps(e, (uint32_t)strlen(e), 7, &used), HTTP_DONE);
    CHECK(resp.status == 429 && resp.retry_after_s == 3 && resp.conn_close);
    CHECK_EQ_INT(nx_err_from_http(resp.status), NXE_PRV_RATE_LIMIT);

    /* body delimited by the end of the connection */
    const char *c = "HTTP/1.1 500 Oops\r\nConnection: close\r\n\r\npartial";
    CHECK_EQ_INT(feed_steps(c, (uint32_t)strlen(c), 3, &used), HTTP_MORE);
    CHECK(http_eof_completes(&resp) && body_len == 7);
    /* a Content-Length body cut short is not complete */
    CHECK_EQ_INT(feed_steps(cl, (uint32_t)strlen(cl) - 10, 5, &used), HTTP_MORE);
    CHECK(http_head_done(&resp) && !http_eof_completes(&resp));
    /* 204: no body */
    CHECK_EQ_INT(feed_steps("HTTP/1.1 204 No\r\n\r\n", 19, 1, &used), HTTP_DONE);

    static const char *bad[] = {
        "HTTX/1.1 200 OK\r\n\r\n", "HTTP/1.1 2x0 OK\r\n\r\n", "HTTP/1.1 200 OK\r\nNoColon\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 1x\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n",
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nZZ\r\n",
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nabX",
        "HTTP/1.1 200 OK\r\n folded: x\r\n\r\n", "HTTP/1.1 101 Switching\r\n\r\n",
    };
    for (unsigned i = 0; i < sizeof(bad) / sizeof(bad[0]); i++) {
        int r = feed_steps(bad[i], (uint32_t)strlen(bad[i]), 1, &used);
        CHECK_EQ_INT(r, -NXE_PRV_MALFORMED_HTTP);
    }
    /* header section larger than the buffer */
    static char big[HTTP_HEAD_MAX + 64];
    memcpy(big, "HTTP/1.1 200 OK\r\nX: ", 20);
    memset(big + 20, 'a', sizeof(big) - 20);
    CHECK_EQ_INT(feed_steps(big, sizeof(big), 512, &used), -NXE_PRV_TOO_LARGE);
    /* a sink error stops the parser */
    static char huge[200 + sizeof(body)];
    int hl = snprintf(huge, 200, "HTTP/1.1 200 OK\r\nContent-Length: %u\r\n\r\n",
                      (unsigned)sizeof(body) + 1);
    memset(huge + hl, 'b', sizeof(body) + 1);
    CHECK_EQ_INT(feed_steps(huge, (uint32_t)hl + (uint32_t)sizeof(body) + 1, 1000, &used),
                 -NXE_PRV_TOO_LARGE);
}

/* ---- SSE --------------------------------------------------------------------------- */

static char ev_log[4096];

static int log_event(void *ctx, const char *event, const char *data, uint32_t len)
{
    (void)ctx;
    size_t n = strlen(ev_log);
    snprintf(ev_log + n, sizeof(ev_log) - n, "[%s|%.*s]", event, (int)len, data);
    return NXE_OK;
}

static struct sse sse;

static void test_sse(void)
{
    const char *stream = ": comment\r\nevent: a\r\ndata: one\r\ndata:two\r\n\r\n"
                         "data: plain\n\nevent: b\rdata: x\r\rid: 7\nretry: 5\nevent: c\n\n"
                         "event: d\ndata\n\n";
    const char *want = "[a|one\ntwo][message|plain][b|x][d|]";
    for (uint32_t step = 1; step <= 20; step++) {
        sse_init(&sse);
        ev_log[0] = 0;
        uint32_t len = (uint32_t)strlen(stream);
        for (uint32_t off = 0; off < len; off += step)
            CHECK_EQ_INT(sse_feed(&sse, stream + off, len - off < step ? len - off : step,
                                  log_event, 0),
                         NXE_OK);
        CHECK(strcmp(ev_log, want) == 0);
    }
    CHECK_EQ_INT(sse.events, 4);
    /* a line longer than the buffer */
    static char longline[SSE_LINE_MAX + 16];
    memcpy(longline, "data: ", 6);
    memset(longline + 6, 'x', sizeof(longline) - 6);
    sse_init(&sse);
    CHECK_EQ_INT(sse_feed(&sse, longline, sizeof(longline), log_event, 0), NXE_PRV_TOO_LARGE);
}

/* ---- Messages stream ----------------------------------------------------------------- */

static const char *STREAM_OK =
    "event: message_start\n"
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\","
    "\"role\":\"assistant\",\"model\":\"nanox-mock-model\",\"content\":[],\"stop_reason\":null,"
    "\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n"
    "event: ping\ndata: {\"type\": \"ping\"}\n\n"
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,"
    "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n"
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,"
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"Lis\\u0074ing \"}}\n\n"
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,"
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"tasks.\"}}\n\n"
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n"
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,"
    "\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"list_tasks\","
    "\"input\":{}}}\n\n"
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,"
    "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"filter\\\": \"}}\n\n"
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,"
    "\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"all\\\"}\"}}\n\n"
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n"
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":"
    "\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":15}}\n\n"
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

static struct msg_stream ms;

static int to_msg(void *ctx, const char *event, const char *data, uint32_t len)
{
    return msg_event((struct msg_stream *)ctx, event, data, len);
}

static int run_stream(const char *text, uint32_t step)
{
    msg_stream_init(&ms);
    sse_init(&sse);
    uint32_t len = (uint32_t)strlen(text);
    for (uint32_t off = 0; off < len; off += step) {
        int e = sse_feed(&sse, text + off, len - off < step ? len - off : step, to_msg, &ms);
        if (e)
            return e;
    }
    return msg_finish(&ms);
}

/* The stream with `from` replaced by `to` (first occurrence). */
static const char *patched(const char *from, const char *to)
{
    static char out[8192];
    const char *p = strstr(STREAM_OK, from);
    if (!p)
        return "missing";
    size_t a = (size_t)(p - STREAM_OK);
    snprintf(out, sizeof(out), "%.*s%s%s", (int)a, STREAM_OK, to, p + strlen(from));
    return out;
}

static void test_messages_stream(void)
{
    for (uint32_t step = 1; step <= 97; step += 3) {
        CHECK_EQ_INT(run_stream(STREAM_OK, step), NXE_OK);
        CHECK_EQ_INT(ms.nblocks, 2);
    }
    CHECK(strcmp(ms.stop_reason, "tool_use") == 0);
    CHECK(strcmp(ms.model, "nanox-mock-model") == 0);
    CHECK(ms.in_tokens == 25 && ms.out_tokens == 15);
    struct jspan t = msg_block_span(&ms, 0), in = msg_block_span(&ms, 1);
    CHECK(t.len == 14 && memcmp(t.p, "Listing tasks.", 14) == 0);
    CHECK(strcmp(ms.b[1].name, "list_tasks") == 0 && strcmp(ms.b[1].id, "toolu_1") == 0);
    CHECK(in.len == 17 && memcmp(in.p, "{\"filter\": \"all\"}", 17) == 0);

    /* the end of the stream never came */
    char cut[8192];
    snprintf(cut, sizeof(cut), "%.*s", (int)(strstr(STREAM_OK, "event: message_stop") - STREAM_OK),
             STREAM_OK);
    CHECK_EQ_INT(run_stream(cut, 50), NXE_PRV_TRUNCATED);
    /* an error event in the middle */
    char err[8192];
    snprintf(err, sizeof(err), "%sevent: error\ndata: {\"type\":\"error\",\"error\":{\"type\":"
             "\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n", cut);
    CHECK_EQ_INT(run_stream(err, 50), NXE_PRV_OVERLOADED);
    CHECK(strcmp(ms.error_type, "overloaded_error") == 0);
    /* malformed payloads and orders */
    CHECK_EQ_INT(run_stream(patched("\"index\":1,\"delta\"", "\"index\":0,\"delta\""), 64),
                 NXE_PRV_MALFORMED);                               /* delta for a closed block */
    CHECK_EQ_INT(run_stream(patched("{\"type\": \"ping\"}", "{\"type\": \"ping\""), 64),
                 NXE_PRV_MALFORMED);                               /* not JSON */
    CHECK_EQ_INT(run_stream(patched("event: ping", "event: surprise"), 64), NXE_PRV_MALFORMED);
    CHECK_EQ_INT(run_stream(patched("\"text_delta\",\"text\":\"tasks.\"",
                                    "\"input_json_delta\",\"partial_json\":\"x\""), 64),
                 NXE_PRV_MALFORMED);                               /* delta type mismatch */
    CHECK_EQ_INT(run_stream(patched("\\\"all\\\"}", "\\\"all\\\""), 64), NXE_PRV_BAD_TOOL);
    CHECK_EQ_INT(run_stream(patched(",\"name\":\"list_tasks\"", ""), 64), NXE_PRV_MALFORMED);
    CHECK_EQ_INT(run_stream(patched("\"stop_reason\":\"tool_use\"", "\"stop_reason\":null"), 64),
                 NXE_PRV_MALFORMED);                               /* no stop reason */
    CHECK_EQ_INT(run_stream(patched("event: content_block_stop\ndata: {\"type\":"
                                    "\"content_block_stop\",\"index\":1}\n\n", ""), 64),
                 NXE_PRV_MALFORMED);                               /* block never closed */
    /* unknown block types (thinking) are skipped */
    CHECK_EQ_INT(run_stream(patched("{\"type\":\"text\",\"text\":\"\"}",
                                    "{\"type\":\"thinking\",\"thinking\":\"\"}"), 64),
                 NXE_OK);
    CHECK_EQ_INT(ms.b[0].type, MB_OTHER);

    char et[48];
    const char *eb = "{\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"m\"}}";
    msg_error_type(eb, (uint32_t)strlen(eb), et, sizeof(et));
    CHECK(strcmp(et, "rate_limit_error") == 0);
    CHECK_EQ_INT(msg_error_code(et), NXE_PRV_RATE_LIMIT);
    msg_error_type("<html>", 6, et, sizeof(et));
    CHECK(strcmp(et, "-") == 0);
    CHECK_EQ_INT(msg_error_code("api_error"), NXE_PRV_SERVER);
    CHECK_EQ_INT(msg_error_code("something_new"), NXE_PRV_STREAM_ERROR);
}

static void test_messages_body(void)
{
    static char cmem[4096], bmem[8192];
    struct msg_conv c;
    struct jw w;
    conv_init(&c, cmem, sizeof(cmem));
    conv_user_text(&c, "Покажи задачи", (uint32_t)strlen("Покажи задачи"));
    CHECK_EQ_INT(run_stream(STREAM_OK, 1000), NXE_OK);
    conv_assistant(&c, &ms);
    conv_tool_result(&c, "toolu_1", "RES a SUCCEEDED\nEND a\n", 22, 0);
    conv_tool_result(&c, "toolu_2", "bad \"x\"", 7, 1);
    jw_init(&w, bmem, sizeof(bmem));
    CHECK_EQ_INT(msg_request_body(&w, "nanox-mock-model", 1024, "sys", "[{\"name\":\"t\"}]", &c),
                 0);
    CHECK(conv_ok(&c));
    CHECK(json_valid(w.p, w.len));
    struct jspan b = {w.p, w.len}, msgs, m, content, blk, v;
    char s[128];
    uint32_t count = 0;
    CHECK(json_member_str(b, "model", s, sizeof(s)) && strcmp(s, "nanox-mock-model") == 0);
    CHECK(json_member(b, "stream", &v) == 1 && json_type(v) == JSON_TRUE);
    CHECK(json_member(b, "messages", &msgs) == 1);
    CHECK(json_element(msgs, 0, &count, &m) == 1 && count == 3);
    CHECK(json_member_str(m, "content", s, sizeof(s)) && strcmp(s, "Покажи задачи") == 0);
    CHECK(json_element(msgs, 1, 0, &m) == 1 && json_member_str(m, "role", s, sizeof(s)) &&
          strcmp(s, "assistant") == 0);
    CHECK(json_member(m, "content", &content) == 1);
    CHECK(json_element(content, 1, &count, &blk) == 1 && count == 2);
    CHECK(json_member(blk, "input", &v) == 1 && json_type(v) == JSON_OBJECT);
    CHECK(json_member_str(v, "filter", s, sizeof(s)) && strcmp(s, "all") == 0);
    CHECK(json_element(msgs, 2, 0, &m) == 1 && json_member(m, "content", &content) == 1);
    CHECK(json_element(content, 1, &count, &blk) == 1 && count == 2); /* one user message */
    CHECK(json_member(blk, "is_error", &v) == 1 && json_type(v) == JSON_TRUE);
    CHECK(json_member_str(blk, "content", s, sizeof(s)) && strcmp(s, "bad \"x\"") == 0);
    /* too small */
    jw_init(&w, bmem, 100);
    CHECK_EQ_INT(msg_request_body(&w, "m", 1, "", "", &c), -1);
}

void test_http(void)
{
    test_json_valid();
    test_json_read();
    test_json_write();
    test_http_responses();
    test_sse();
    test_messages_stream();
    test_messages_body();
}
