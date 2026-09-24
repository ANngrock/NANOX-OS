/* Host tests for the executor's task engine (user/core/engine.c). */
#include <stdint.h>
#include <string.h>

#include "engine.h"
#include "test.h"

static struct engine eng;

static void run_to(struct eng_action *a, int final)
{
    eng_advance(a, ACT_OBSERVING);
    eng_advance(a, ACT_PLANNED);
    eng_advance(a, ACT_RUNNING);
    eng_advance(a, ACT_VERIFYING);
    eng_advance(a, final);
}

static void test_lifecycle(void)
{
    struct eng_action a;
    memset(&a, 0, sizeof(a));
    a.state = ACT_CREATED;
    CHECK_EQ_INT(eng_advance(&a, ACT_RUNNING), -1); /* no skipping observation */
    CHECK_EQ_INT(eng_advance(&a, ACT_OBSERVING), 0);
    CHECK_EQ_INT(eng_advance(&a, ACT_SUCCEEDED), -1); /* never succeed unverified */
    CHECK_EQ_INT(eng_advance(&a, ACT_PLANNED), 0);
    CHECK_EQ_INT(eng_advance(&a, ACT_VERIFYING), -1);
    CHECK_EQ_INT(eng_advance(&a, ACT_RUNNING), 0);
    CHECK_EQ_INT(eng_advance(&a, ACT_SUCCEEDED), -1); /* RUNNING -> SUCCEEDED forbidden */
    CHECK_EQ_INT(eng_advance(&a, ACT_VERIFYING), 0);
    CHECK_EQ_INT(eng_advance(&a, ACT_SUCCEEDED), 0);
    CHECK(eng_is_final(a.state));
    CHECK_EQ_INT(eng_advance(&a, ACT_FAILED), -1); /* final states are final */
    CHECK_EQ_INT(a.state, ACT_SUCCEEDED);

    a.state = ACT_OBSERVING;
    CHECK_EQ_INT(eng_advance(&a, ACT_FAILED), 0); /* precondition failed */
    a.state = ACT_RUNNING;
    CHECK_EQ_INT(eng_advance(&a, ACT_OUTCOME_UNKNOWN), 0);
    a.state = ACT_PLANNED;
    CHECK_EQ_INT(eng_advance(&a, ACT_CANCELLED), 0);
    a.state = ACT_RUNNING;
    CHECK_EQ_INT(eng_advance(&a, ACT_CANCELLED), -1); /* running actions are not cancelled */
    CHECK(strcmp(eng_state_name(ACT_OUTCOME_UNKNOWN), "OUTCOME_UNKNOWN") == 0);
}

static void test_dedup(void)
{
    struct eng_action *a, *b;
    eng_init(&eng, 0);
    CHECK_EQ_INT(eng_begin(&eng, "r1", "task.spawn", 111, &a), ENG_NEW);
    CHECK_EQ_INT(a->state, ACT_CREATED);
    /* Same id while still running. */
    CHECK_EQ_INT(eng_begin(&eng, "r1", "task.spawn", 111, &b), ENG_IN_PROGRESS);
    CHECK(b == a);
    run_to(a, ACT_SUCCEEDED);
    CHECK_EQ_INT(eng_store(a, "RES r1 SUCCEEDED\nEND r1\n", 24), 0);
    CHECK_EQ_INT(eng_begin(&eng, "r1", "task.spawn", 111, &b), ENG_REPLAY);
    CHECK(b == a);
    CHECK_EQ_INT(b->result_len, 24);
    CHECK(strcmp(b->result, "RES r1 SUCCEEDED\nEND r1\n") == 0);
    CHECK_EQ_INT(eng_begin(&eng, "r1", "task.spawn", 222, &b), ENG_ID_REUSED);
    CHECK_EQ_INT(eng.started, 1);
    CHECK_EQ_INT(eng.replayed, 1);
    CHECK(eng_find(&eng, "r1") == a);
    CHECK(eng_find(&eng, "r2") == 0);
    /* A failed action is replayed too (its outcome is known). */
    CHECK_EQ_INT(eng_begin(&eng, "r2", "task.terminate", 5, &a), ENG_NEW);
    eng_advance(a, ACT_OBSERVING);
    eng_advance(a, ACT_FAILED);
    CHECK_EQ_INT(eng_begin(&eng, "r2", "task.terminate", 5, &b), ENG_REPLAY);
    char big[ENG_RESULT_MAX];
    memset(big, 'x', sizeof(big));
    CHECK_EQ_INT(eng_store(a, big, ENG_RESULT_MAX), -1);
}

static void test_eviction(void)
{
    struct eng_action *a;
    char id[16];
    eng_init(&eng, 0);
    for (uint32_t i = 0; i < ENG_SLOTS; i++) {
        snprintf(id, sizeof(id), "e%u", i);
        CHECK_EQ_INT(eng_begin(&eng, id, "task.list", i, &a), ENG_NEW);
        if (i != 3)
            run_to(a, ACT_SUCCEEDED);
    }
    /* Full: the oldest finished record (e0) is evicted, not e3 (running). */
    CHECK_EQ_INT(eng_begin(&eng, "new", "task.list", 99, &a), ENG_NEW);
    CHECK_EQ_INT(eng.evicted, 1);
    CHECK(eng_find(&eng, "e0") == 0);
    CHECK(eng_find(&eng, "e3") != 0);
    CHECK(eng_find(&eng, "e1") != 0);
    /* An evicted id is new again. */
    CHECK_EQ_INT(eng_begin(&eng, "e0", "task.list", 0, &a), ENG_NEW);
    CHECK(eng_find(&eng, "e1") == 0);
    /* Everything in progress: nothing can be evicted. */
    eng_init(&eng, 0);
    for (uint32_t i = 0; i < ENG_SLOTS; i++) {
        snprintf(id, sizeof(id), "p%u", i);
        eng_begin(&eng, id, "task.list", i, &a);
    }
    CHECK_EQ_INT(eng_begin(&eng, "x", "task.list", 1, &a), ENG_FULL);
    CHECK(a == 0);
}

static void test_nodedup(void)
{
    struct eng_action *a, *b;
    eng_init(&eng, 1);
    CHECK_EQ_INT(eng_begin(&eng, "r1", "task.spawn", 1, &a), ENG_NEW);
    run_to(a, ACT_SUCCEEDED);
    /* Negative-control mode: the repeat is executed again. */
    CHECK_EQ_INT(eng_begin(&eng, "r1", "task.spawn", 1, &b), ENG_NEW);
    CHECK(b != a);
    CHECK(eng_find(&eng, "r1") == b); /* status reports the newest record */
    CHECK_EQ_INT(eng.started, 2);
}

/* M4: records read back from the store. */
static void test_restore(void)
{
    struct eng_action *a, *b;
    eng_init(&eng, 0);
    a = eng_restore(&eng, "old1", "task.spawn", 7, ACT_SUCCEEDED, "RES old1 SUCCEEDED\nEND old1\n",
                    30, 0xB00Du);
    CHECK(a != NULL && a->persist && a->restored && a->boot_id == 0xB00Du);
    CHECK_EQ_INT(a->result_len, 30);
    /* Non-final states are refused (the caller converts them first). */
    CHECK(eng_restore(&eng, "old2", "task.spawn", 8, ACT_RUNNING, "", 0, 1) == NULL);
    CHECK(eng_restore(&eng, "old3", "task.spawn", 8, ACT_OUTCOME_UNKNOWN, "x", 1, 1) != NULL);
    /* The id is known: a second restore is refused. */
    CHECK(eng_restore(&eng, "old1", "task.spawn", 7, ACT_SUCCEEDED, "", 0, 1) == NULL);
    /* A repeated request of an earlier boot is answered from the record. */
    CHECK_EQ_INT(eng_begin(&eng, "old1", "task.spawn", 7, &b), ENG_REPLAY);
    CHECK(b == a);
    CHECK_EQ_INT(eng_begin(&eng, "old1", "task.spawn", 9, &b), ENG_ID_REUSED);
    CHECK_EQ_INT(eng.started, 0); /* restoring is not starting */
    /* A new record is not persisted until the persistence layer says so. */
    CHECK_EQ_INT(eng_begin(&eng, "new1", "config.set", 3, &b), ENG_NEW);
    CHECK(!b->persist && !b->restored && b->boot_id == 0);
    /* Records kept in the store are evicted only when no other finished
     * record is left: the read-only n* records go first. */
    char id[8];
    for (uint32_t i = 0; i < ENG_SLOTS; i++) {
        snprintf(id, sizeof(id), "n%u", i);
        if (eng_begin(&eng, id, "task.list", i, &b) == ENG_NEW)
            run_to(b, ACT_SUCCEEDED);
    }
    CHECK(eng_find(&eng, "old1") != NULL && eng_find(&eng, "old3") != NULL);
    CHECK(eng_find(&eng, "n0") == NULL && eng_find(&eng, "n31") != NULL);
    run_to(eng_find(&eng, "new1"), ACT_SUCCEEDED);
    /* With only persisted records finished, the oldest of them goes. */
    for (uint32_t i = 0; i < ENG_SLOTS; i++)
        if (eng.a[i].state != ACT_NONE)
            eng.a[i].persist = 1;
    CHECK_EQ_INT(eng_begin(&eng, "last", "task.list", 1, &b), ENG_NEW);
    CHECK(eng_find(&eng, "old1") == NULL && eng_find(&eng, "old3") != NULL);
}

void test_engine(void)
{
    test_lifecycle();
    test_dedup();
    test_eviction();
    test_nodedup();
    test_restore();
}
