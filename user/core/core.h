/*
 * Pieces of bin/core (core.c) shared with the persistence layer
 * (persist.c, M4).  Not a library interface: both files are one program.
 */
#ifndef NANOX_CORE_CORE_H
#define NANOX_CORE_CORE_H

#include <stdint.h>

#include "engine.h"
#include "nci.h"

extern struct engine eng;
extern uint64_t boot_id;
extern uint64_t verify_failures;

/* Lifecycle step with a serial trace line; an illegal transition counts as
 * a verification failure. */
void step(struct eng_action *a, int to, const char *detail);
/* Ends the action FAILED with `code` and builds the response in b. */
void fail_action(struct eng_action *a, struct nci_buf *b, const char *code, const char *detail,
                 const char *effects);
void res_begin(struct nci_buf *b, const char *id, const char *state);
/* Empty response buffer (the one res_begin uses). */
void res_reset(struct nci_buf *b);
void item_begin(struct nci_buf *b, const char *id);
void res_end(struct nci_buf *b, const char *id);
/* One request line; returns the exit code once the session is closed, else -1. */
int64_t core_handle(const char *line, uint32_t len);

typedef void (*op_fn)(struct eng_action *, const struct nci_req *, struct nci_buf *);

/* ---- persist.c ---- */

enum { PS_ABSENT = 0, PS_MOUNTED, PS_UNMOUNTABLE };
extern int ps_state;

/* Mounts the store on block device handle h (0: no store), restores the
 * task-engine records, marks interrupted ones OUTCOME_UNKNOWN and commits
 * that.  Prints the recovery report. */
void ps_init(uint64_t h);
const char *ps_state_name(void);
uint64_t ps_gen(void);
/* NCI operations of the store; NULL if `op` is not one of them. */
op_fn ps_op(const char *op);
extern const char PS_OPS[];
/* Write-ahead record of an action with effects outside the store (task
 * spawn/terminate): committed in state RUNNING before the kernel call.
 * 0 ok or no store; -1 the commit failed (nothing was done). */
int ps_intent(struct eng_action *a);
/* After such an action ended: commits its final record.  Inserts
 * " saved=yes|no" into the first line of the response in b. */
void ps_final(struct eng_action *a, struct nci_buf *b);
/* eng_action.persist values */
#define PS_RECORD 1 /* the record was committed together with a store change */
#define PS_INTENT 2 /* write-ahead record committed; the final one is due */
/* action.status fields of a record: boot and whether it is in the store. */
void ps_status_fields(struct nci_buf *b, const struct eng_action *a);
/* M5: reads object `name` of the current generation (configuration,
 * secrets, trust anchors).  ST_OK or an ST_E_* code. */
int ps_read_obj(const char *name, void *buf, uint32_t cap, uint32_t *len);
/* Modes without the bridge (abi/nanox/m4.h). */
int64_t ps_run_workload(void);
int64_t ps_run_check(void);

#endif
