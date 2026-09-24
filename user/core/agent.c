/*
 * bin/core, M5: the agent loop inside the guest (docs/m5-net.md §8-9).  The
 * Cognitive Core asks the model itself, over its own network stack, TLS
 * and provider client; the host is not involved beyond typing the request
 * on the operator console.
 *
 *   agent.ask qa=<base64url> [qb= qc=]
 *        The user's request (UTF-8, base64url without padding, split over
 *        up to three values: NCI keys are letters, a line is 256 bytes).  Up to AGENT_STEPS_MAX model requests; every
 *        tool call of the model is validated against the tool table and
 *        executed as an ordinary NCI action "<ask id>.<n>" through the task
 *        engine (lifecycle, verification, M4 write-ahead records), and its
 *        response goes back to the model as the tool result.
 *        SUCCEEDED: the model answered and every action succeeded.
 *        FAILED code=<CLASS>_ERROR detail=<name> class=<class>:
 *          net / tls / provider / local   no usable model response
 *          action                         an action failed (detail = its code)
 *          provider, detail=bad_tool      the model called an unknown tool or
 *                                         with bad arguments (refused, not run)
 *        The answer follows as ITEM lines "seq=<n> text=<base64url>".
 *   provider.status      configuration, key reference and length (never the
 *                        key), connection state
 *   telemetry.status     outcome counters by class since boot
 *
 * Telemetry lines on the serial log, one per model attempt (provider.c),
 * one per action and one per request:
 *   tel ask=<id> step=<n> phase=action tool=<name> request=<id> result=<STATE>
 *       code=<code> class=<ok|action> ms=<t>
 *   tel ask=<id> phase=ask result=<ok|name> class=<class> steps=<n> ...
 */
#include <nanox/json.h>
#include <nanox/m5.h>
#include <nanox/messages.h>
#include <nanox/string.h>

#include "m5.h"
#include "nanox_user.h"

#define AGENT_STEPS_MAX 10u
#define QUESTION_MAX 200u
#define ANSWER_MAX 600u

static const char SYSTEM_PROMPT[] =
    "You operate NANOX-OS through the tools given. Every tool call is executed inside the "
    "guest OS and verified there; only a SUCCEEDED result with verify=ok means the action "
    "happened. Call one tool at a time. When the user's request is fulfilled or cannot be, "
    "answer briefly in the user's language without calling a tool.";

struct tool_param {
    const char *name, *desc;
    int optional;
};

static const struct tool {
    const char *name, *op, *desc;
    int mutating;
    struct tool_param p[2];
} TOOLS[] = {
    {"describe_system", "system.describe",
     "Describe the running NANOX system: boot id, executor, clock, task count.", 0, {{0}}},
    {"list_tasks", "task.list",
     "List all tasks (kernel threads and user tasks) with state and CPU ticks.", 0, {{0}}},
    {"inspect_task", "task.inspect", "Show one task in detail.", 0,
     {{"target", "task reference task/<boot>/<id> from list_tasks", 0}}},
    {"spawn_task", "task.spawn",
     "Start a program from the initramfs (bin/<program>) as a new user task.", 1,
     {{"program", "program name, e.g. load (the test workload)", 0}}},
    {"measure_task", "task.measure", "Measure the CPU time a task gets during a window.", 0,
     {{"target", "task reference", 0}, {"window_ms", "window length, 1..5000 ms", 0}}},
    {"terminate_task", "task.terminate",
     "Stop a user task and verify that it ended and its resources were released.", 1,
     {{"target", "task reference", 0},
      {"expect_rev", "optional: revision the task must still have (else CONFLICT)", 1}}},
    {"memory_stats", "memory.stats", "Physical memory counters.", 0, {{0}}},
};
#define NTOOLS (sizeof(TOOLS) / sizeof(TOOLS[0]))

/* ---- base64url ------------------------------------------------------------------------ */

static const char B64[] = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

static int b64val(char c)
{
    for (int i = 0; i < 64; i++)
        if (B64[i] == c)
            return i;
    return -1;
}

/* Decodes n characters; the length, or -1. */
static int b64_decode(const char *s, uint32_t n, char *out, uint32_t cap)
{
    uint32_t o = 0, acc = 0, bits = 0;
    for (uint32_t i = 0; i < n; i++) {
        int v = b64val(s[i]);
        if (v < 0)
            return -1;
        acc = (acc << 6) | (uint32_t)v;
        bits += 6;
        if (bits >= 8) {
            bits -= 8;
            if (o + 1 >= cap)
                return -1;
            out[o++] = (char)((acc >> bits) & 0xFF);
        }
    }
    if (bits >= 6 || (acc & ((1u << bits) - 1)))
        return -1; /* a dangling character or non-zero padding bits */
    out[o] = 0;
    return (int)o;
}

/* Answer as ITEM lines of at most 64 base64url characters. */
static void answer_items(struct nci_buf *b, const char *id, const char *s, uint32_t n)
{
    uint32_t acc = 0, bits = 0, col = 0, seq = 0;
    for (uint32_t i = 0; i <= n; i++) {
        if (i < n) {
            acc = (acc << 8) | (uint8_t)s[i];
            bits += 8;
        } else if (bits) {
            acc <<= 6 - bits;
            bits = 6;
        }
        while (bits >= 6) {
            bits -= 6;
            if (col == 0) {
                item_begin(b, id);
                nb_kv_u64(b, "seq", seq++);
                nb_str(b, " text=");
            }
            nb_char(b, B64[(acc >> bits) & 63]);
            col = (col + 1) % 64;
        }
    }
}

/* ---- tools ---------------------------------------------------------------------------- */

static char tools_json[4096];

static const char *tools_def(void)
{
    if (tools_json[0])
        return tools_json;
    struct jw w;
    jw_init(&w, tools_json, sizeof(tools_json));
    jw_rawz(&w, "[");
    for (uint32_t t = 0; t < NTOOLS; t++) {
        const struct tool *T = &TOOLS[t];
        jw_rawz(&w, t ? ",{\"name\":" : "{\"name\":");
        jw_strz(&w, T->name);
        jw_rawz(&w, ",\"description\":");
        jw_strz(&w, T->desc);
        jw_rawz(&w, ",\"input_schema\":{\"type\":\"object\",\"properties\":{");
        int np = 0;
        for (uint32_t k = 0; k < 2 && T->p[k].name; k++) {
            jw_rawz(&w, np++ ? "," : "");
            jw_strz(&w, T->p[k].name);
            jw_rawz(&w, ":{\"type\":\"string\",\"description\":");
            jw_strz(&w, T->p[k].desc);
            jw_rawz(&w, "}");
        }
        jw_rawz(&w, "},\"required\":[");
        int nr = 0;
        for (uint32_t k = 0; k < 2 && T->p[k].name; k++) {
            if (T->p[k].optional)
                continue;
            jw_rawz(&w, nr++ ? "," : "");
            jw_strz(&w, T->p[k].name);
        }
        jw_rawz(&w, "],\"additionalProperties\":false}}");
    }
    jw_rawz(&w, "]");
    if (w.overflow)
        tools_json[0] = 0;
    return tools_json;
}

/* Validates a tool call and builds the NCI request line; NULL ok, else the
 * reason the call is refused (sent back to the model as an error). */
static const char *tool_line(const struct msg_block *blk, struct jspan input, const char *rid,
                             const struct tool **out, char *line, uint32_t cap)
{
    const struct tool *T = 0;
    for (uint32_t t = 0; t < NTOOLS; t++)
        if (nci_streq(blk->name, TOOLS[t].name))
            T = &TOOLS[t];
    if (!T)
        return "unknown tool";
    *out = T;
    struct nci_buf b;
    nb_init(&b, line, cap);
    nb_str(&b, "REQ ");
    nb_str(&b, rid);
    nb_char(&b, ' ');
    nb_str(&b, T->op);
    int seen[2] = {0, 0};
    if (input.len) {
        uint32_t pos = 0;
        char key[24], val[NCI_VAL_MAX + 1];
        struct jspan v;
        int r;
        while ((r = json_next_member(input, &pos, key, sizeof(key), &v)) == 1) {
            int k = -1;
            for (int j = 0; j < 2; j++)
                if (T->p[j].name && nci_streq(key, T->p[j].name))
                    k = j;
            if (k < 0)
                return "unexpected argument";
            if (seen[k]++)
                return "argument given twice";
            int jt = json_type(v);
            if (jt == JSON_STRING) {
                if (json_string(v, val, sizeof(val)) <= 0)
                    return "argument value too long or empty";
            } else if (jt == JSON_NUMBER && v.len < sizeof(val)) {
                uint64_t u;
                if (!json_u64(v, &u))
                    return "numeric argument not a non-negative integer";
                memcpy(val, v.p, v.len);
                val[v.len] = 0;
            } else {
                return "argument is not a string";
            }
            if (!nci_value_ok(val))
                return "argument has characters the executor does not accept";
            nb_kv(&b, key, val);
        }
        if (r < 0)
            return "input is not a JSON object";
    }
    for (int j = 0; j < 2; j++)
        if (T->p[j].name && !T->p[j].optional && !seen[j])
            return "missing argument";
    return b.overflow ? "request too long" : 0;
}

/* ---- agent.ask -------------------------------------------------------------------------- */

static char conv_mem[40960];
static char body_mem[PRV_BODY_MAX];
static struct msg_stream ms;
static char obs[ENG_RESULT_MAX + 1];
static char answer[ANSWER_MAX + 8];

struct ask_state {
    uint32_t steps, actions, actions_failed, bad_tools, answer_len, truncated;
    struct prv_summary sum;
    char failed_code[32], failed_request[NCI_ID_MAX + 1];
    char effects[64];
};

/* The code= value of an NCI response text (first line), or the state. */
static void response_code(const char *text, char *out, uint32_t cap)
{
    const char *p = text;
    while (*p && *p != '\n') {
        if (p[0] == ' ' && memcmp(p + 1, "code=", 5) == 0) {
            p += 6;
            uint32_t k = 0;
            while (p[k] && p[k] != ' ' && p[k] != '\n' && k + 1 < cap) {
                out[k] = p[k];
                k++;
            }
            out[k] = 0;
            return;
        }
        p++;
    }
    out[0] = '-';
    out[1] = 0;
}

static void add_effect(struct ask_state *st, const char *op)
{
    uint32_t len = (uint32_t)nx_strlen(st->effects), n = (uint32_t)nx_strlen(op);
    if (len + 1 + n >= sizeof(st->effects))
        return; /* the list is informative; the actions are in the engine */
    if (len)
        st->effects[len++] = ',';
    memcpy(st->effects + len, op, n + 1);
}

static void run_tools(struct eng_action *a, struct ask_state *st, struct msg_conv *conv)
{
    for (uint32_t i = 0; i < ms.nblocks; i++) {
        const struct msg_block *blk = &ms.b[i];
        if (blk->type != MB_TOOL_USE)
            continue;
        char rid[NCI_ID_MAX + 1], line[NCI_LINE_MAX + 1];
        struct nci_buf rb;
        nb_init(&rb, rid, sizeof(rid));
        uint32_t idl = (uint32_t)nx_strlen(a->id);
        char head[24];
        memcpy(head, a->id, idl > 20 ? 20 : idl);
        head[idl > 20 ? 20 : idl] = 0;
        nb_str(&rb, head);
        nb_char(&rb, '.');
        nb_u64(&rb, ++st->actions);
        const struct tool *T = 0;
        const char *why = tool_line(blk, msg_block_span(&ms, i), rid, &T, line, sizeof(line));
        if (why) {
            st->bad_tools++;
            m5_tel_count(NXE_PRV_BAD_TOOL);
            u_printf("tel ask=%s step=%u phase=action tool=%s request=%s result=bad_tool"
                     " class=%s reason=\"%s\"\n",
                     a->id, st->steps, blk->name, rid, nx_err_class_name(m5_class(NXE_PRV_BAD_TOOL)),
                     why);
            char msg[96];
            struct nci_buf mb;
            nb_init(&mb, msg, sizeof(msg));
            nb_str(&mb, "tool call refused by the executor: ");
            nb_str(&mb, why);
            conv_tool_result(conv, blk->id, msg, mb.len, 1);
            continue;
        }
        uint64_t t0 = m5_now_ms();
        struct nci_buf ob;
        int ex = core_exec(line, (uint32_t)nx_strlen(line), &ob);
        uint32_t n = ob.len < ENG_RESULT_MAX ? ob.len : ENG_RESULT_MAX;
        memcpy(obs, ob.p, n);
        obs[n] = 0;
        int ok = ex == 0 && memcmp(obs + 4 + nx_strlen(rid), " SUCCEEDED", 10) == 0;
        char code[32];
        response_code(obs, code, sizeof(code));
        const char *sp = obs + 5 + nx_strlen(rid);
        char state[24];
        uint32_t sl = 0;
        while (sp[sl] && sp[sl] != ' ' && sp[sl] != '\n' && sl + 1 < sizeof(state)) {
            state[sl] = sp[sl];
            sl++;
        }
        state[sl] = 0;
        m5_tel.actions++;
        if (ok) {
            m5_tel_count(NXE_OK);
            if (T->mutating)
                add_effect(st, T->op);
        } else {
            m5_tel.actions_failed++;
            st->actions_failed++;
            m5_tel_count(NXE_ACT_FAILED);
            if (!st->failed_code[0]) {
                memcpy(st->failed_code, code, sizeof(st->failed_code));
                memcpy(st->failed_request, rid, sizeof(rid));
            }
        }
        u_printf("tel ask=%s step=%u phase=action tool=%s request=%s result=%s code=%s class=%s"
                 " ms=%lu\n",
                 a->id, st->steps, T->name, rid, state, code,
                 ok ? "ok" : nx_err_class_name(m5_class(NXE_ACT_FAILED)), m5_now_ms() - t0);
        conv_tool_result(conv, blk->id, obs, n, !ok);
    }
}

static void collect_answer(struct ask_state *st)
{
    st->answer_len = 0;
    for (uint32_t i = 0; i < ms.nblocks; i++) {
        if (ms.b[i].type != MB_TEXT)
            continue;
        struct jspan t = msg_block_span(&ms, i);
        for (uint32_t k = 0; k < t.len; k++) {
            if (st->answer_len >= ANSWER_MAX) {
                st->truncated = 1;
                break;
            }
            answer[st->answer_len++] = t.p[k];
        }
    }
    /* never cut a UTF-8 sequence */
    while (st->truncated && st->answer_len &&
           ((uint8_t)answer[st->answer_len - 1] & 0xC0) == 0x80)
        st->answer_len--;
    if (st->truncated && st->answer_len && (uint8_t)answer[st->answer_len - 1] >= 0xC0)
        st->answer_len--;
    answer[st->answer_len] = 0;
}

/* Text of the user or the model on the serial log: in parts of at most 160
 * bytes, control characters replaced, so that it can never forge a line. */
static void print_text(const char *what, const char *id, const char *s, uint32_t n)
{
    char part[168];
    uint32_t i = 0, no = 0;
    while (i < n) {
        uint32_t k = 0;
        while (i < n && k < 160) {
            uint8_t c = (uint8_t)s[i];
            /* keep UTF-8 sequences whole within a part */
            if (k >= 156 && c >= 0xC0)
                break;
            part[k++] = c < 0x20 || c == 0x7F ? ' ' : (char)c;
            i++;
        }
        part[k] = 0;
        u_printf("%s ask=%s part=%u: %s\n", what, id, no++, part);
    }
}

static void ask_fields(struct nci_buf *b, const struct ask_state *st)
{
    nb_kv_u64(b, "steps", st->steps);
    nb_kv_u64(b, "attempts", st->sum.attempts);
    nb_kv_u64(b, "retries", st->sum.retries);
    nb_kv_u64(b, "reconnects", st->sum.reconnects);
    nb_kv_u64(b, "actions", st->actions);
    nb_kv_u64(b, "actions_failed", st->actions_failed);
    nb_kv_u64(b, "bad_tools", st->bad_tools);
    nb_kv(b, "effects", st->effects[0] ? st->effects : "none");
}

static void op_agent_ask(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    static struct ask_state st;
    static struct msg_conv conv;
    memset(&st, 0, sizeof(st));
    step(a, ACT_OBSERVING, "");
    if (!m5_enabled) {
        fail_action(a, b, "NO_NETWORK", 0, "none");
        return;
    }
    char q64[3 * NCI_VAL_MAX + 1], question[QUESTION_MAX + 1];
    uint32_t ql = 0;
    static const char *const parts[] = {"qa", "qb", "qc"};
    for (int i = 0; i < 3; i++) {
        const char *v = nci_get(r, parts[i]);
        if (!v)
            break;
        uint32_t n = (uint32_t)nx_strlen(v);
        memcpy(q64 + ql, v, n);
        ql += n;
    }
    int qn = ql ? b64_decode(q64, ql, question, sizeof(question)) : -1;
    if (qn <= 0) {
        fail_action(a, b, "BAD_REQUEST", "qa", "none");
        return;
    }
    m5_tel.asks++;
    int e = prv_load();
    if (e != NXE_OK) {
        m5_tel_count(e);
        u_printf("tel ask=%s phase=config result=%s class=%s\n", a->id, nx_err_name(e),
                 nx_err_class_name(m5_class(e)));
        m5_fail(a, b, e, "none");
        return;
    }
    step(a, ACT_PLANNED, "tools=7 steps_max=10");
    step(a, ACT_RUNNING, "");
    print_text("agent question", a->id, question, (uint32_t)qn);
    conv_init(&conv, conv_mem, sizeof(conv_mem));
    conv_user_text(&conv, question, (uint32_t)qn);
    uint64_t t0 = m5_now_ms();
    int answered = 0;
    e = NXE_OK;
    while (st.steps < AGENT_STEPS_MAX) {
        st.steps++;
        struct jw w;
        jw_init(&w, body_mem, sizeof(body_mem));
        if (msg_request_body(&w, prv.model, prv.max_tokens, SYSTEM_PROMPT, tools_def(), &conv)) {
            e = NXE_LOC_RESOURCES;
            break;
        }
        e = prv_request(a->id, st.steps, w.p, w.len, &ms, &st.sum);
        if (e != NXE_OK)
            break;
        conv_assistant(&conv, &ms);
        collect_answer(&st);
        uint32_t ntools = 0;
        for (uint32_t i = 0; i < ms.nblocks; i++)
            ntools += ms.b[i].type == MB_TOOL_USE;
        if (!ntools) {
            answered = 1;
            break;
        }
        run_tools(a, &st, &conv);
    }
    if (e == NXE_OK && !answered)
        e = NXE_PRV_NO_ANSWER;
    /* an action that failed outranks a refused tool call: it may have had effects */
    int final = e != NXE_OK           ? e
                : st.actions_failed ? NXE_ACT_FAILED
                : st.bad_tools      ? NXE_PRV_BAD_TOOL
                                    : NXE_OK;
    int cls = m5_class(final);
    const char *detail = final == NXE_ACT_FAILED ? st.failed_code : nx_err_name(final);
    u_printf("tel ask=%s phase=ask result=%s class=%s detail=%s steps=%u attempts=%u retries=%u"
             " reconnects=%u actions=%u actions_failed=%u bad_tools=%u effects=%s ms=%lu\n",
             a->id, final == NXE_OK ? "ok" : nx_err_name(final), nx_err_class_name(cls), detail,
             st.steps, st.sum.attempts, st.sum.retries, st.sum.reconnects, st.actions,
             st.actions_failed, st.bad_tools, st.effects[0] ? st.effects : "none",
             m5_now_ms() - t0);
    if (answered) {
        u_printf("agent answer ask=%s bytes=%u truncated=%s\n", a->id, st.answer_len,
                 st.truncated ? "yes" : "no");
        print_text("agent answer", a->id, answer, st.answer_len);
    }
    if (final == NXE_OK) {
        m5_tel.asks_ok++;
        step(a, ACT_VERIFYING, "actions=verified answer=present");
        step(a, ACT_SUCCEEDED, "");
        res_begin(b, a->id, "SUCCEEDED");
    } else {
        static const char *const codes[] = {"OK", "NET_ERROR", "TLS_ERROR", "PROVIDER_ERROR",
                                            "ACTION_ERROR", "LOCAL_ERROR", "UNCLASSIFIED_ERROR"};
        char t[128];
        struct nci_buf tb;
        nb_init(&tb, t, sizeof(t));
        nb_str(&tb, "code=");
        nb_str(&tb, codes[cls]);
        nb_str(&tb, " detail=");
        nb_str(&tb, detail);
        nb_str(&tb, " class=");
        nb_str(&tb, nx_err_class_name(cls));
        step(a, ACT_FAILED, t);
        res_begin(b, a->id, "FAILED");
        nb_kv(b, "code", codes[cls]);
        nb_kv(b, "detail", detail);
        nb_kv(b, "class", nx_err_class_name(cls));
        if (final == NXE_ACT_FAILED)
            nb_kv(b, "action", st.failed_request);
        if (st.sum.http) {
            nb_kv_u64(b, "http", st.sum.http);
            if (st.sum.error_type[0] && st.sum.error_type[0] != '-')
                nb_kv(b, "error_type", st.sum.error_type);
        }
    }
    ask_fields(b, &st);
    if (answered) {
        nb_kv_u64(b, "answer_bytes", st.answer_len);
        nb_kv(b, "truncated", st.truncated ? "yes" : "no");
        nb_kv_u64(b, "in_tokens", ms.in_tokens);
        nb_kv_u64(b, "out_tokens", ms.out_tokens);
        answer_items(b, a->id, answer, st.answer_len);
    }
    res_end(b, a->id);
}

/* ---- provider.status, telemetry.status --------------------------------------------------- */

static void op_provider_status(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    (void)r;
    step(a, ACT_OBSERVING, "");
    if (!m5_enabled) {
        fail_action(a, b, "NO_NETWORK", 0, "none");
        return;
    }
    int e = prv_load();
    step(a, ACT_PLANNED, "read=config");
    step(a, ACT_RUNNING, "");
    step(a, ACT_VERIFYING, "read-only");
    step(a, ACT_SUCCEEDED, "");
    res_begin(b, a->id, "SUCCEEDED");
    nb_kv(b, "config", e == NXE_OK ? "ok" : nx_err_name(e));
    nb_kv(b, "host", prv.host[0] ? prv.host : "-");
    nb_kv_u64(b, "port", prv.port);
    nb_kv(b, "model", prv.model[0] && nci_value_ok(prv.model) ? prv.model : "-");
    nb_kv(b, "key_ref", prv.key_ref);
    nb_kv(b, "key", prv_key_state());
    nb_kv_u64(b, "key_len", prv.key_len);
    nb_kv_u64(b, "timeout_ms", prv.timeout_ms);
    nb_kv_u64(b, "attempts", prv.attempts);
    nb_kv(b, "connection", m5_tls_is_open() ? "open" : "closed");
    nb_kv_u64(b, "anchors", m5_anchor_count());
    res_end(b, a->id);
}

static void op_telemetry_status(struct eng_action *a, const struct nci_req *r, struct nci_buf *b)
{
    (void)r;
    step(a, ACT_OBSERVING, "");
    step(a, ACT_PLANNED, "read=counters");
    step(a, ACT_RUNNING, "");
    step(a, ACT_VERIFYING, "read-only");
    step(a, ACT_SUCCEEDED, "");
    res_begin(b, a->id, "SUCCEEDED");
    static const char *const keys[] = {"ok", "net", "tls", "provider", "action", "local",
                                       "unclassified"};
    for (int c = 0; c < 7; c++)
        nb_kv_u64(b, keys[c], m5_tel.by_class[c]);
    nb_kv_u64(b, "model_attempts", m5_tel.model_attempts);
    nb_kv_u64(b, "retries", m5_tel.retries);
    nb_kv_u64(b, "reconnects", m5_tel.reconnects);
    nb_kv_u64(b, "connects", m5_tel.connects);
    nb_kv_u64(b, "asks", m5_tel.asks);
    nb_kv_u64(b, "asks_ok", m5_tel.asks_ok);
    nb_kv_u64(b, "actions", m5_tel.actions);
    nb_kv_u64(b, "actions_failed", m5_tel.actions_failed);
    nb_kv(b, "last", m5_tel.last_err ? nx_err_name(m5_tel.last_err) : "-");
    nb_kv(b, "last_class", m5_tel.last_err ? nx_err_class_name(m5_tel.last_class) : "-");
    res_end(b, a->id);
}

op_fn m5_agent_op(const char *op)
{
    if (nci_streq(op, "agent.ask"))
        return op_agent_ask;
    if (nci_streq(op, "provider.status"))
        return op_provider_status;
    if (nci_streq(op, "telemetry.status"))
        return op_telemetry_status;
    return 0;
}
