/*
 * IPC endpoint message queue (M2).  Pure ring buffer; the blocking and the
 * handle installation are done by the syscall layer (kernel/syscall.c).
 */
#ifndef NANOX_KERNEL_OBJ_IPC_H
#define NANOX_KERNEL_OBJ_IPC_H

#include <stdint.h>

#include "handle.h"

#define NX_IPC_QUEUE 8u

struct nx_ipc_msg {
    uint32_t len;
    uint32_t sender_task;
    struct nx_object *obj; /* transferred object (reference owned by the message) or NULL */
    uint32_t rights;
    uint8_t data[NX_IPC_MSG_MAX];
};

struct nx_endpoint {
    struct nx_object base;
    struct nx_ipc_msg q[NX_IPC_QUEUE];
    uint32_t head, count;
    void *waiter; /* task blocked in receive, or NULL */
};

void nx_ep_init(struct nx_endpoint *ep);
/* Enqueues a copy; NX_EFULL when the queue is full. */
int nx_ep_push(struct nx_endpoint *ep, const struct nx_ipc_msg *m);
/* Oldest message or NULL; nx_ep_pop removes it (without dropping obj). */
struct nx_ipc_msg *nx_ep_peek(struct nx_endpoint *ep);
void nx_ep_pop(struct nx_endpoint *ep);
/* Drops every queued message, releasing transferred objects. */
void nx_ep_drain(struct nx_endpoint *ep);

#endif
