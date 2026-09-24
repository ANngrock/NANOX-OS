/*
 * Provider protocol: streamed response assembly, conversation and request
 * body (nanox/messages.h).
 */
#include <nanox/messages.h>
#include <nanox/nerr.h>

static int streq(const char *a, const char *b)
{
    while (*a && *a == *b) {
        a++;
        b++;
    }
    return *a == *b;
}

static void copyz(char *out, uint32_t cap, const char *s)
{
    uint32_t i = 0;
    for (; s[i] && i + 1 < cap; i++)
        out[i] = s[i];
    out[i] = 0;
}

void msg_stream_init(struct msg_stream *m)
{
    char *p = (char *)m;
    for (uint32_t i = 0; i < sizeof(*m); i++)
        p[i] = 0;
    m->open = -1;
}

struct jspan msg_block_span(const struct msg_stream *m, uint32_t i)
{
    struct jspan s = {m->arena + m->b[i].off, m->b[i].len};
    return s;
}

int msg_error_code(const char *type)
{
    if (streq(type, "overloaded_error"))
        return NXE_PRV_OVERLOADED;
    if (streq(type, "rate_limit_error"))
        return NXE_PRV_RATE_LIMIT;
    if (streq(type, "api_error"))
        return NXE_PRV_SERVER;
    if (streq(type, "authentication_error") || streq(type, "permission_error"))
        return NXE_PRV_AUTH;
    if (streq(type, "invalid_request_error") || streq(type, "request_too_large"))
        return NXE_PRV_BAD_REQUEST;
    if (streq(type, "not_found_error"))
        return NXE_PRV_NOT_FOUND;
    return NXE_PRV_STREAM_ERROR;
}

void msg_error_type(const char *body, uint32_t len, char *out, uint32_t cap)
{
    struct jspan b = {body, len}, e;
    copyz(out, cap, "-");
    if (!json_valid(body, len) || json_member(b, "error", &e) != 1 ||
        !json_member_str(e, "type", out, cap))
        copyz(out, cap, "-");
}

static int append(struct msg_stream *m, struct jspan v)
{
    /* v is a JSON string: decode it straight into the arena */
    if (m->arena_len >= MSG_ARENA - 1)
        return NXE_PRV_TOO_LARGE;
    int n = json_string(v, m->arena + m->arena_len, MSG_ARENA - m->arena_len);
    if (n < 0)
        return json_type(v) == JSON_STRING ? NXE_PRV_TOO_LARGE : NXE_PRV_MALFORMED;
    m->arena_len += (uint32_t)n;
    m->b[m->open].len += (uint32_t)n;
    return NXE_OK;
}

static int index_is(struct jspan d, int want)
{
    struct jspan v;
    uint64_t i;
    return json_member(d, "index", &v) == 1 && json_u64(v, &i) && want >= 0 &&
           i == (uint64_t)want;
}

int msg_event(struct msg_stream *m, const char *event, const char *data, uint32_t len)
{
    struct jspan d = {data, len}, v, cb, delta;
    char type[40];
    m->events++;
    if (!json_valid(data, len) || json_type(d) != JSON_OBJECT)
        return NXE_PRV_MALFORMED;
    if (json_member_str(d, "type", type, sizeof(type)) && !streq(type, event))
        return NXE_PRV_MALFORMED; /* the payload names another event */
    if (streq(event, "ping"))
        return NXE_OK;
    if (streq(event, "error")) {
        if (json_member(d, "error", &v) != 1 ||
            !json_member_str(v, "type", m->error_type, sizeof(m->error_type)))
            copyz(m->error_type, sizeof(m->error_type), "-");
        return msg_error_code(m->error_type);
    }
    if (streq(event, "message_start")) {
        if (m->started || json_member(d, "message", &v) != 1 || json_type(v) != JSON_OBJECT)
            return NXE_PRV_MALFORMED;
        m->started = 1;
        json_member_str(v, "id", m->msg_id, sizeof(m->msg_id));
        json_member_str(v, "model", m->model, sizeof(m->model));
        struct jspan u, t;
        if (json_member(v, "usage", &u) == 1 && json_member(u, "input_tokens", &t) == 1)
            json_u64(t, &m->in_tokens);
        return NXE_OK;
    }
    if (!m->started || m->stopped)
        return NXE_PRV_MALFORMED;
    if (streq(event, "content_block_start")) {
        if (m->open != -1 || !index_is(d, (int)m->nblocks) ||
            json_member(d, "content_block", &cb) != 1)
            return NXE_PRV_MALFORMED;
        if (m->nblocks >= MSG_BLOCKS_MAX)
            return NXE_PRV_TOO_LARGE;
        struct msg_block *b = &m->b[m->nblocks];
        if (!json_member_str(cb, "type", type, sizeof(type)))
            return NXE_PRV_MALFORMED;
        b->off = m->arena_len;
        b->len = 0;
        m->open = (int)m->nblocks++;
        if (streq(type, "text")) {
            b->type = MB_TEXT;
            if (json_member(cb, "text", &v) == 1)
                return append(m, v);
        } else if (streq(type, "tool_use")) {
            b->type = MB_TOOL_USE;
            if (!json_member_str(cb, "id", b->id, sizeof(b->id)) ||
                !json_member_str(cb, "name", b->name, sizeof(b->name)) || !b->id[0] ||
                !b->name[0])
                return NXE_PRV_MALFORMED;
        } else {
            b->type = MB_OTHER;
        }
        return NXE_OK;
    }
    if (streq(event, "content_block_delta")) {
        if (m->open < 0 || !index_is(d, m->open) || json_member(d, "delta", &delta) != 1 ||
            !json_member_str(delta, "type", type, sizeof(type)))
            return NXE_PRV_MALFORMED;
        int bt = m->b[m->open].type;
        if (streq(type, "text_delta") && bt == MB_TEXT) {
            if (json_member(delta, "text", &v) != 1)
                return NXE_PRV_MALFORMED;
            return append(m, v);
        }
        if (streq(type, "input_json_delta") && bt == MB_TOOL_USE) {
            if (json_member(delta, "partial_json", &v) != 1)
                return NXE_PRV_MALFORMED;
            return append(m, v);
        }
        return bt == MB_OTHER ? NXE_OK : NXE_PRV_MALFORMED;
    }
    if (streq(event, "content_block_stop")) {
        if (m->open < 0 || !index_is(d, m->open))
            return NXE_PRV_MALFORMED;
        m->b[m->open].closed = 1;
        m->open = -1;
        return NXE_OK;
    }
    if (streq(event, "message_delta")) {
        if (json_member(d, "delta", &delta) != 1)
            return NXE_PRV_MALFORMED;
        if (json_member(delta, "stop_reason", &v) == 1 && json_type(v) == JSON_STRING &&
            json_string(v, m->stop_reason, sizeof(m->stop_reason)) < 0)
            return NXE_PRV_MALFORMED;
        struct jspan u, t;
        if (json_member(d, "usage", &u) == 1 && json_member(u, "output_tokens", &t) == 1)
            json_u64(t, &m->out_tokens);
        return NXE_OK;
    }
    if (streq(event, "message_stop")) {
        if (m->open != -1)
            return NXE_PRV_MALFORMED;
        m->stopped = 1;
        return NXE_OK;
    }
    return NXE_PRV_MALFORMED; /* an event the protocol does not have */
}

int msg_finish(struct msg_stream *m)
{
    if (!m->stopped)
        return NXE_PRV_TRUNCATED;
    if (!m->stop_reason[0])
        return NXE_PRV_MALFORMED;
    uint32_t tools = 0;
    for (uint32_t i = 0; i < m->nblocks; i++) {
        if (!m->b[i].closed)
            return NXE_PRV_MALFORMED;
        if (m->b[i].type != MB_TOOL_USE)
            continue;
        tools++;
        struct jspan in = msg_block_span(m, i);
        if (in.len && json_type(in) != JSON_OBJECT)
            return NXE_PRV_BAD_TOOL;
    }
    if (streq(m->stop_reason, "tool_use") && !tools)
        return NXE_PRV_MALFORMED;
    return NXE_OK;
}

/* ---- conversation ------------------------------------------------------------------ */

void conv_init(struct msg_conv *c, char *mem, uint32_t cap)
{
    jw_init(&c->w, mem, cap);
    c->nmsg = 0;
    c->last_role = 0;
    c->pending_user = 0;
}

static void close_pending(struct msg_conv *c)
{
    if (c->pending_user) {
        jw_rawz(&c->w, "]}");
        c->pending_user = 0;
    }
}

static void begin_msg(struct msg_conv *c, const char *role)
{
    close_pending(c);
    if (c->nmsg++)
        jw_rawz(&c->w, ",");
    jw_rawz(&c->w, "{\"role\":\"");
    jw_rawz(&c->w, role);
    jw_rawz(&c->w, "\",\"content\":");
}

void conv_user_text(struct msg_conv *c, const char *text, uint32_t len)
{
    begin_msg(c, "user");
    jw_str(&c->w, text, len);
    jw_rawz(&c->w, "}");
    c->last_role = 1;
}

void conv_assistant(struct msg_conv *c, const struct msg_stream *m)
{
    begin_msg(c, "assistant");
    jw_rawz(&c->w, "[");
    int n = 0;
    for (uint32_t i = 0; i < m->nblocks; i++) {
        const struct msg_block *b = &m->b[i];
        struct jspan s = msg_block_span(m, i);
        if (b->type == MB_TEXT && s.len) {
            jw_rawz(&c->w, n++ ? ",{\"type\":\"text\",\"text\":" : "{\"type\":\"text\",\"text\":");
            jw_str(&c->w, s.p, s.len);
            jw_rawz(&c->w, "}");
        } else if (b->type == MB_TOOL_USE) {
            jw_rawz(&c->w, n++ ? ",{\"type\":\"tool_use\",\"id\":" : "{\"type\":\"tool_use\",\"id\":");
            jw_strz(&c->w, b->id);
            jw_rawz(&c->w, ",\"name\":");
            jw_strz(&c->w, b->name);
            jw_rawz(&c->w, ",\"input\":");
            if (s.len)
                jw_raw(&c->w, s.p, s.len);
            else
                jw_rawz(&c->w, "{}");
            jw_rawz(&c->w, "}");
        }
    }
    if (!n)
        jw_rawz(&c->w, "{\"type\":\"text\",\"text\":\"-\"}");
    jw_rawz(&c->w, "]}");
    c->last_role = 2;
}

void conv_tool_result(struct msg_conv *c, const char *tool_use_id, const char *text,
                      uint32_t len, int is_error)
{
    if (c->pending_user) {
        jw_rawz(&c->w, ",");
    } else {
        begin_msg(c, "user");
        jw_rawz(&c->w, "[");
        c->pending_user = 1;
    }
    jw_rawz(&c->w, "{\"type\":\"tool_result\",\"tool_use_id\":");
    jw_strz(&c->w, tool_use_id);
    jw_rawz(&c->w, ",\"content\":");
    jw_str(&c->w, text, len);
    if (is_error)
        jw_rawz(&c->w, ",\"is_error\":true");
    jw_rawz(&c->w, "}");
    c->last_role = 1;
}

int conv_ok(struct msg_conv *c)
{
    return !c->w.overflow;
}

int msg_request_body(struct jw *out, const char *model, uint32_t max_tokens, const char *system,
                     const char *tools_json, struct msg_conv *c)
{
    close_pending(c);
    if (c->w.overflow)
        return -1;
    jw_rawz(out, "{\"model\":");
    jw_strz(out, model);
    jw_rawz(out, ",\"max_tokens\":");
    jw_u64(out, max_tokens);
    if (system && system[0]) {
        jw_rawz(out, ",\"system\":");
        jw_strz(out, system);
    }
    if (tools_json && tools_json[0]) {
        jw_rawz(out, ",\"tools\":");
        jw_rawz(out, tools_json);
    }
    jw_rawz(out, ",\"messages\":[");
    jw_raw(out, c->w.p, c->w.len);
    jw_rawz(out, "],\"stream\":true}");
    return out->overflow ? -1 : 0;
}
