/* Host tests for the IPC endpoint queue (kernel/obj/ipc.c). */
#include <stdint.h>
#include <string.h>

#include "obj/ipc.h"
#include "test.h"

static int destroyed;

static void on_destroy(struct nx_object *o)
{
    (void)o;
    destroyed++;
}

static struct nx_ipc_msg msg(uint32_t seq, uint32_t len)
{
    struct nx_ipc_msg m;
    memset(&m, 0, sizeof(m));
    m.len = len;
    m.sender_task = seq;
    for (uint32_t i = 0; i < len; i++)
        m.data[i] = (uint8_t)(seq * 31u + i);
    return m;
}

static int msg_ok(const struct nx_ipc_msg *m, uint32_t seq, uint32_t len)
{
    if (!m || m->sender_task != seq || m->len != len)
        return 0;
    for (uint32_t i = 0; i < len; i++)
        if (m->data[i] != (uint8_t)(seq * 31u + i))
            return 0;
    return 1;
}

static void test_empty_full(void)
{
    static struct nx_endpoint ep;
    nx_ep_init(&ep);
    CHECK(nx_ep_peek(&ep) == 0);
    nx_ep_pop(&ep); /* popping an empty queue does nothing */
    CHECK_EQ_INT(ep.count, 0);
    CHECK(ep.waiter == 0);

    for (uint32_t i = 0; i < NX_IPC_QUEUE; i++) {
        struct nx_ipc_msg m = msg(i + 1, i * 16);
        CHECK_EQ_INT(nx_ep_push(&ep, &m), NX_OK);
    }
    CHECK_EQ_INT(ep.count, NX_IPC_QUEUE);
    struct nx_ipc_msg extra = msg(99, 4);
    CHECK_EQ_INT(nx_ep_push(&ep, &extra), NX_EFULL);
    CHECK_EQ_INT(ep.count, NX_IPC_QUEUE);

    /* FIFO order; the full-queue refusal did not overwrite anything. */
    for (uint32_t i = 0; i < NX_IPC_QUEUE; i++) {
        CHECK(msg_ok(nx_ep_peek(&ep), i + 1, i * 16));
        nx_ep_pop(&ep);
    }
    CHECK(nx_ep_peek(&ep) == 0);
    CHECK_EQ_INT(ep.count, 0);

    /* Size limit: the maximum is accepted, one byte more is not. */
    struct nx_ipc_msg big = msg(7, NX_IPC_MSG_MAX);
    CHECK_EQ_INT(nx_ep_push(&ep, &big), NX_OK);
    big.len = NX_IPC_MSG_MAX + 1;
    CHECK_EQ_INT(nx_ep_push(&ep, &big), NX_EINVAL);
    big.len = UINT32_MAX;
    CHECK_EQ_INT(nx_ep_push(&ep, &big), NX_EINVAL);
    CHECK_EQ_INT(ep.count, 1);
    CHECK(msg_ok(nx_ep_peek(&ep), 7, NX_IPC_MSG_MAX));
    struct nx_ipc_msg empty = msg(8, 0);
    CHECK_EQ_INT(nx_ep_push(&ep, &empty), NX_OK); /* zero-length messages are fine */
    nx_ep_pop(&ep);
    CHECK(msg_ok(nx_ep_peek(&ep), 8, 0));
}

static void test_copy_semantics(void)
{
    static struct nx_endpoint ep;
    nx_ep_init(&ep);
    struct nx_ipc_msg m = msg(1, 32);
    CHECK_EQ_INT(nx_ep_push(&ep, &m), NX_OK);
    memset(m.data, 0xEE, sizeof(m.data)); /* the sender's buffer changes afterwards */
    m.len = 3;
    CHECK(msg_ok(nx_ep_peek(&ep), 1, 32));
}

/* Random pushes and pops against a reference FIFO, many times around the ring. */
static void test_wraparound(void)
{
    static struct nx_endpoint ep;
    nx_ep_init(&ep);
    uint32_t ref[NX_IPC_QUEUE * 4];
    uint32_t head = 0, tail = 0, next = 1, wraps = 0, bad = 0;
    uint64_t x = 0x2545F4914F6CDD1Dull;
    for (int step = 0; step < 20000; step++) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        uint32_t queued = tail - head;
        if (x & 1) {
            uint32_t len = (uint32_t)(x >> 8) % (NX_IPC_MSG_MAX + 1);
            struct nx_ipc_msg m = msg(next, len);
            int st = nx_ep_push(&ep, &m);
            if (queued == NX_IPC_QUEUE) {
                bad += st != NX_EFULL;
            } else {
                bad += st != NX_OK;
                ref[tail++ % (NX_IPC_QUEUE * 4)] = next << 8 | len % 256;
                next++;
            }
        } else {
            struct nx_ipc_msg *m = nx_ep_peek(&ep);
            if (queued == 0) {
                bad += m != 0;
            } else {
                uint32_t want = ref[head++ % (NX_IPC_QUEUE * 4)];
                bad += !m || m->sender_task != want >> 8 || m->len % 256 != (want & 0xFF) ||
                       !msg_ok(m, m->sender_task, m->len);
                uint32_t before = ep.head;
                nx_ep_pop(&ep);
                wraps += ep.head < before;
            }
        }
        bad += ep.count != tail - head || ep.head >= NX_IPC_QUEUE;
    }
    CHECK_EQ_INT(bad, 0);
    CHECK(wraps > 100);
    CHECK(next > 1000);
}

static void test_drain(void)
{
    static struct nx_endpoint ep;
    struct nx_object a = {NX_OBJ_VMO, 1, on_destroy}, b = {NX_OBJ_TASK, 2, on_destroy};
    nx_ep_init(&ep);
    destroyed = 0;
    /* Queued messages own a reference to their transferred object. */
    struct nx_ipc_msg m1 = msg(1, 4), m2 = msg(2, 0), m3 = msg(3, 8);
    m1.obj = &a;
    m1.rights = NX_RIGHT_READ;
    m3.obj = &b;
    CHECK_EQ_INT(nx_ep_push(&ep, &m1), NX_OK);
    CHECK_EQ_INT(nx_ep_push(&ep, &m2), NX_OK);
    CHECK_EQ_INT(nx_ep_push(&ep, &m3), NX_OK);
    CHECK(nx_ep_peek(&ep)->obj == &a);
    CHECK_EQ_INT(nx_ep_peek(&ep)->rights, NX_RIGHT_READ);
    /* nx_ep_pop does not touch the object (the receiver took it over). */
    nx_ep_pop(&ep);
    CHECK_EQ_INT(a.refs, 1);
    CHECK_EQ_INT(nx_ep_push(&ep, &m1), NX_OK);
    /* Endpoint destruction: every queued object is released exactly once. */
    nx_ep_drain(&ep);
    CHECK_EQ_INT(ep.count, 0);
    CHECK(nx_ep_peek(&ep) == 0);
    CHECK_EQ_INT(a.refs, 0);
    CHECK_EQ_INT(b.refs, 1);
    CHECK_EQ_INT(destroyed, 1);
    nx_ep_drain(&ep);
    CHECK_EQ_INT(destroyed, 1);
}

void test_ipc(void)
{
    test_empty_full();
    test_copy_semantics();
    test_wraparound();
    test_drain();
}
