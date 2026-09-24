#include "ipc.h"

void nx_ep_init(struct nx_endpoint *ep)
{
    ep->head = ep->count = 0;
    ep->waiter = 0;
}

int nx_ep_push(struct nx_endpoint *ep, const struct nx_ipc_msg *m)
{
    if (m->len > NX_IPC_MSG_MAX)
        return NX_EINVAL;
    if (ep->count == NX_IPC_QUEUE)
        return NX_EFULL;
    ep->q[(ep->head + ep->count) % NX_IPC_QUEUE] = *m;
    ep->count++;
    return NX_OK;
}

struct nx_ipc_msg *nx_ep_peek(struct nx_endpoint *ep)
{
    return ep->count ? &ep->q[ep->head] : 0;
}

void nx_ep_pop(struct nx_endpoint *ep)
{
    if (!ep->count)
        return;
    ep->head = (ep->head + 1) % NX_IPC_QUEUE;
    ep->count--;
}

void nx_ep_drain(struct nx_endpoint *ep)
{
    struct nx_ipc_msg *m;
    while ((m = nx_ep_peek(ep))) {
        if (m->obj)
            nx_obj_unref(m->obj);
        nx_ep_pop(ep);
    }
}
