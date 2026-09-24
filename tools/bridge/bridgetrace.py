"""Trace of the host bridge (M3 criterion 6).

One JSON object per line (schema nanox.bridge-trace.v1).  Kinds:

  task    lifecycle of a user request on the host: CREATED, then a final
          SUCCEEDED / FAILED (with the model's final answer or the reason)
  action  one step of the agent loop, with all five parts:
            request       the user's request the step serves
            decision      what the model chose (tool, arguments, rationale)
            call          the NCI request line sent to the guest
            result        the guest's response (state, fields, items, lines)
            verification  the guest executor's verdict (verify=, checks=) and
                          the host's own check of the result
  recovery  a lost response and how its outcome was established
  model     a failed attempt to get a decision from the model
"""

import json
import time

SCHEMA = "nanox.bridge-trace.v1"
ACTION_PARTS = ("request", "decision", "call", "result", "verification")


class Trace:
    def __init__(self, path=None):
        self.path = path
        self.records = []
        if path:
            open(path, "w").close()

    def write(self, kind, **fields):
        rec = {"schema": SCHEMA, "kind": kind, "time": round(time.time(), 3)}
        rec.update(fields)
        self.records.append(rec)
        if self.path:
            with open(self.path, "a") as f:
                f.write(json.dumps(rec, ensure_ascii=False) + "\n")
        return rec


def check_trace(records):
    """Problems with the completeness of the trace (empty list: complete).

    Every action record must carry all five parts; the call must be the NCI
    request for the tool the model chose; the result must answer that call;
    and every task that was created must have ended."""
    problems = []
    created, ended = set(), set()
    for i, r in enumerate(records):
        if r.get("schema") != SCHEMA:
            problems.append("trace_schema: record %d has schema %r" % (i, r.get("schema")))
        if r.get("kind") == "task":
            (created if r.get("state") == "CREATED" else ended).add(r.get("task"))
        if r.get("kind") != "action":
            continue
        missing = [p for p in ACTION_PARTS if not r.get(p)]
        if missing:
            problems.append("trace_incomplete: action %s lacks %s" % (r.get("id"), ",".join(missing)))
            continue
        call, result, decision = r["call"], r["result"], r["decision"]
        if not call.get("line", "").startswith("REQ %s %s" % (call.get("id"), call.get("op"))):
            problems.append("trace_call: action %s call line does not match" % r.get("id"))
        if result.get("id") != call.get("id"):
            problems.append("trace_result: action %s result answers %r" % (r.get("id"), result.get("id")))
        if call.get("tool") != decision.get("tool"):
            problems.append("trace_decision: action %s call is not the chosen tool" % r.get("id"))
        if "guest" not in r["verification"] or "host" not in r["verification"]:
            problems.append("trace_verification: action %s lacks guest or host verdict" % r.get("id"))
    for t in sorted(created - ended):
        problems.append("trace_task: task %s never ended" % t)
    return problems
