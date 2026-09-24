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

void test_engine(void)
{
    test_lifecycle();
    test_dedup();
    test_eviction();
    test_nodedup();
}
