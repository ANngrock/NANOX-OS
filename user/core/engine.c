#include "engine.h"

static void copy_str(char *dst, const char *src, uint32_t cap)
{
    uint32_t i = 0;
    for (; src[i] && i + 1 < cap; i++)
        dst[i] = src[i];
    dst[i] = 0;
}

void eng_init(struct engine *e, int nodedup)
{
    for (uint32_t i = 0; i < ENG_SLOTS; i++)
        e->a[i].state = ACT_NONE;
    e->next_seq = 1;
    e->nodedup = nodedup;
    e->started = e->replayed = e->evicted = 0;
}

int eng_is_final(int state)
{
    return state == ACT_SUCCEEDED || state == ACT_FAILED || state == ACT_OUTCOME_UNKNOWN ||
           state == ACT_CANCELLED;
}

struct eng_action *eng_find(struct engine *e, const char *id)
{
    struct eng_action *found = 0;
    for (uint32_t i = 0; i < ENG_SLOTS; i++) /* the newest record wins (nodedup) */
        if (e->a[i].state != ACT_NONE && nci_streq(e->a[i].id, id) &&
            (!found || e->a[i].seq > found->seq))
            found = &e->a[i];
    return found;
}

int eng_begin(struct engine *e, const char *id, const char *op, uint64_t fp,
              struct eng_action **out)
{
    *out = 0;
    if (!e->nodedup) {
        struct eng_action *a = eng_find(e, id);
        if (a) {
            *out = a;
            if (a->fp != fp)
                return ENG_ID_REUSED;
            if (!eng_is_final(a->state))
                return ENG_IN_PROGRESS;
            e->replayed++;
            return ENG_REPLAY;
        }
    }
    /* A free record, else the oldest finished one. */
    struct eng_action *slot = 0;
    for (uint32_t i = 0; i < ENG_SLOTS && !slot; i++)
        if (e->a[i].state == ACT_NONE)
            slot = &e->a[i];
    if (!slot) {
        for (uint32_t i = 0; i < ENG_SLOTS; i++)
            if (eng_is_final(e->a[i].state) && (!slot || e->a[i].seq < slot->seq))
                slot = &e->a[i];
        if (!slot)
            return ENG_FULL;
        e->evicted++;
    }
    copy_str(slot->id, id, sizeof(slot->id));
    copy_str(slot->op, op, sizeof(slot->op));
    slot->fp = fp;
    slot->seq = e->next_seq++;
    slot->state = ACT_CREATED;
    slot->result_len = 0;
    slot->result[0] = 0;
    e->started++;
    *out = slot;
    return ENG_NEW;
}

int eng_advance(struct eng_action *a, int to)
{
    int ok = 0;
    switch (a->state) {
    case ACT_CREATED: ok = to == ACT_OBSERVING || to == ACT_FAILED; break;
    case ACT_OBSERVING: ok = to == ACT_PLANNED || to == ACT_FAILED; break;
    case ACT_PLANNED: ok = to == ACT_RUNNING || to == ACT_FAILED || to == ACT_CANCELLED; break;
    case ACT_RUNNING:
        ok = to == ACT_VERIFYING || to == ACT_FAILED || to == ACT_OUTCOME_UNKNOWN;
        break;
    case ACT_VERIFYING:
        ok = to == ACT_SUCCEEDED || to == ACT_FAILED || to == ACT_OUTCOME_UNKNOWN;
        break;
    default: ok = 0; break;
    }
    if (!ok)
        return -1;
    a->state = to;
    return 0;
}

int eng_store(struct eng_action *a, const char *text, uint32_t len)
{
    if (len >= ENG_RESULT_MAX)
        return -1;
    for (uint32_t i = 0; i < len; i++)
        a->result[i] = text[i];
    a->result[len] = 0;
    a->result_len = len;
    return 0;
}

const char *eng_state_name(int state)
{
    switch (state) {
    case ACT_CREATED: return "CREATED";
    case ACT_OBSERVING: return "OBSERVING";
    case ACT_PLANNED: return "PLANNED";
    case ACT_RUNNING: return "RUNNING";
    case ACT_VERIFYING: return "VERIFYING";
    case ACT_SUCCEEDED: return "SUCCEEDED";
    case ACT_FAILED: return "FAILED";
    case ACT_OUTCOME_UNKNOWN: return "OUTCOME_UNKNOWN";
    case ACT_CANCELLED: return "CANCELLED";
    default: return "NONE";
    }
}
