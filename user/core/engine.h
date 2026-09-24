/*
 * Task engine of the Cognitive Core executor (M3): one record per NCI
 * request id with the lifecycle of ARCHITECTURE.md §8.2 and the stored
 * response, so that a repeated request is answered from the record instead
 * of being executed again.  Pure code (no system calls), linked into
 * bin/core and compiled into tests/host.  Rules: docs/m3-core.md §5.
 *
 *   CREATED -> OBSERVING -> PLANNED -> RUNNING -> VERIFYING -> SUCCEEDED
 *                  |           |          |           |
 *                  +-----------+----------+-----------+----> FAILED
 *                                         +-----------+----> OUTCOME_UNKNOWN
 *                              PLANNED ------------------> CANCELLED
 */
#ifndef NANOX_CORE_ENGINE_H
#define NANOX_CORE_ENGINE_H

#include <stdint.h>

#include "nci.h"

#define ENG_SLOTS 32u
#define ENG_RESULT_MAX 3072u

enum eng_state {
    ACT_NONE = 0,
    ACT_CREATED,
    ACT_OBSERVING,
    ACT_PLANNED,
    ACT_RUNNING,
    ACT_VERIFYING,
    ACT_SUCCEEDED,
    ACT_FAILED,
    ACT_OUTCOME_UNKNOWN,
    ACT_CANCELLED,
};

struct eng_action {
    char id[NCI_ID_MAX + 1];
    char op[NCI_OP_MAX + 1];
    uint64_t fp;  /* nci_fingerprint of the request */
    uint32_t seq; /* order of creation, for eviction */
    int state;
    uint32_t result_len; /* stored response (complete lines), valid when final */
    char result[ENG_RESULT_MAX];
};

struct engine {
    struct eng_action a[ENG_SLOTS];
    uint32_t next_seq;
    int nodedup; /* negative control: every request is new */
    uint64_t started, replayed, evicted;
};

enum eng_begin_result {
    ENG_NEW = 0,     /* *out is a fresh record in state CREATED */
    ENG_REPLAY,      /* *out holds the final result of an earlier request */
    ENG_IN_PROGRESS, /* *out is still running (not reachable single-threaded) */
    ENG_ID_REUSED,   /* the id was used for a different request */
    ENG_FULL,        /* every record is in progress */
};

void eng_init(struct engine *e, int nodedup);
int eng_begin(struct engine *e, const char *id, const char *op, uint64_t fp,
              struct eng_action **out);
/* Record of `id`, or NULL (never seen or evicted). */
struct eng_action *eng_find(struct engine *e, const char *id);
/* 0 if the transition is allowed by the lifecycle (and made), -1 if not. */
int eng_advance(struct eng_action *a, int to);
int eng_is_final(int state);
/* Stores the response text; 0 ok, -1 too long (nothing stored). */
int eng_store(struct eng_action *a, const char *text, uint32_t len);
const char *eng_state_name(int state);

#endif
