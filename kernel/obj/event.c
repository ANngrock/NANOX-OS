#include "event.h"

void nx_evlog_init(struct nx_evlog *l)
{
    l->next = 1;
    l->disabled = 0;
}

uint64_t nx_evlog_push(struct nx_evlog *l, uint64_t tick, uint32_t type, uint32_t task,
                       int64_t arg)
{
    if (l->disabled)
        return 0;
    struct nx_event *e = &l->ev[l->next % NX_EVLOG_SIZE];
    e->seq = l->next;
    e->tick = tick;
    e->type = type;
    e->task = task;
    e->arg = arg;
    return l->next++;
}

uint64_t nx_evlog_oldest(const struct nx_evlog *l)
{
    return l->next > NX_EVLOG_SIZE + 1 ? l->next - NX_EVLOG_SIZE : 1;
}

uint32_t nx_evlog_read(const struct nx_evlog *l, uint64_t since, struct nx_event *out,
                       uint32_t cap)
{
    uint64_t seq = since < nx_evlog_oldest(l) ? nx_evlog_oldest(l) : since;
    uint32_t n = 0;
    for (; seq < l->next && n < cap; seq++, n++)
        out[n] = l->ev[seq % NX_EVLOG_SIZE];
    return n;
}
