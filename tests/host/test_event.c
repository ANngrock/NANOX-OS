/* Host tests for the kernel state event log (kernel/obj/event.c). */
#include <stdint.h>
#include <string.h>

#include "obj/event.h"
#include "test.h"

static struct nx_evlog log_;
static struct nx_event out[NX_EVLOG_SIZE + 8];

static void test_basic(void)
{
    nx_evlog_init(&log_);
    CHECK_EQ_INT(log_.next, 1);
    CHECK_EQ_INT(nx_evlog_oldest(&log_), 1);
    CHECK_EQ_INT(nx_evlog_read(&log_, 1, out, 8), 0);
    CHECK_EQ_INT(nx_evlog_push(&log_, 10, NX_EV_TASK_CREATED, 5, 0), 1);
    CHECK_EQ_INT(nx_evlog_push(&log_, 11, NX_EV_TASK_STARTED, 5, 0), 2);
    CHECK_EQ_INT(nx_evlog_push(&log_, 12, NX_EV_TASK_KILLED, 5, 4), 3);
    uint32_t n = nx_evlog_read(&log_, 1, out, 8);
    CHECK_EQ_INT(n, 3);
    CHECK_EQ_INT(out[0].seq, 1);
    CHECK_EQ_INT(out[0].type, NX_EV_TASK_CREATED);
    CHECK_EQ_INT(out[2].arg, 4);
    CHECK_EQ_INT(out[2].tick, 12);
    /* From the middle, limited by cap, and past the end. */
    n = nx_evlog_read(&log_, 2, out, 1);
    CHECK_EQ_INT(n, 1);
    CHECK_EQ_INT(out[0].seq, 2);
    CHECK_EQ_INT(nx_evlog_read(&log_, 4, out, 8), 0);
    CHECK_EQ_INT(nx_evlog_read(&log_, 1000, out, 8), 0);
    /* since = 0 is treated as "from the oldest". */
    CHECK_EQ_INT(nx_evlog_read(&log_, 0, out, 8), 3);
    CHECK_EQ_INT(out[0].seq, 1);
}

static void test_wrap(void)
{
    nx_evlog_init(&log_);
    for (uint32_t i = 0; i < NX_EVLOG_SIZE; i++)
        nx_evlog_push(&log_, i, NX_EV_TASK_CREATED, i, 0);
    CHECK_EQ_INT(nx_evlog_oldest(&log_), 1); /* exactly full: nothing lost */
    nx_evlog_push(&log_, 999, NX_EV_TASK_REAPED, 77, 0);
    CHECK_EQ_INT(nx_evlog_oldest(&log_), 2);
    /* Asking for the overwritten event yields the oldest kept one: the
     * reader sees the gap as out[0].seq > since. */
    uint32_t n = nx_evlog_read(&log_, 1, out, NX_EVLOG_SIZE + 8);
    CHECK_EQ_INT(n, NX_EVLOG_SIZE);
    CHECK_EQ_INT(out[0].seq, 2);
    int consecutive = 1;
    for (uint32_t i = 1; i < n; i++)
        consecutive &= out[i].seq == out[i - 1].seq + 1;
    CHECK(consecutive);
    CHECK_EQ_INT(out[n - 1].seq, NX_EVLOG_SIZE + 1);
    CHECK_EQ_INT(out[n - 1].task, 77);
    CHECK_EQ_INT(out[n - 1].type, NX_EV_TASK_REAPED);
    /* Content of every stored slot belongs to its own sequence number. */
    int match = 1;
    for (uint32_t i = 0; i + 1 < n; i++)
        match &= out[i].task == out[i].seq - 1;
    CHECK(match);
}

static void test_disabled(void)
{
    nx_evlog_init(&log_);
    log_.disabled = 1;
    CHECK_EQ_INT(nx_evlog_push(&log_, 1, NX_EV_TASK_CREATED, 1, 0), 0);
    CHECK_EQ_INT(log_.next, 1);
    CHECK_EQ_INT(nx_evlog_read(&log_, 1, out, 8), 0);
}

void test_event(void)
{
    test_basic();
    test_wrap();
    test_disabled();
}
