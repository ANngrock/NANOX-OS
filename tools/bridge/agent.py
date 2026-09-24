"""Agent loop of the host bridge (M3): user request -> model decision ->
NCI call -> guest result and verification -> next decision.

The host keeps its own lifecycle of the user's request (CREATED ->
RUNNING -> SUCCEEDED / FAILED / OUTCOME_UNKNOWN) next to the guest's
per-action lifecycle.  A model decision is only a proposal: an action counts
as done when the guest answered SUCCEEDED and, for actions with effects,
its executor reported verify=ok (ARCHITECTURE.md §8.1, §17).
"""

import time

import adapters
import nci
import toolspec as tools

MAX_STEPS = 12
MODEL_ATTEMPTS = 3
MODEL_BACKOFF_S = (0.2, 0.5)


def observation(tool, resp):
    return {"tool": tool, "state": resp.state, "fields": resp.fields, "items": resp.items,
            "lines": resp.lines}


def guest_verdict(tool_name, resp):
    """(guest verdict, host verdict) for one action result."""
    tool = tools.BY_NAME[tool_name]
    guest = resp.get("verify", "none")
    if not resp.ok:
        return guest, "failed: guest state %s code %s" % (resp.state, resp.get("code"))
    if tool["mutating"] and guest != "ok":
        return guest, "failed: action with effects not verified by the executor"
    return guest, "ok"


def call_reliably(client, trace, op, request_id, args, lose_first=False):
    """Sends a request; if the response is lost, establishes the outcome
    through action.status and repeats the same request id only as a replay
    (never a second execution).  Returns (response, recovery info)."""
    try:
        return client.call(op, request_id=request_id, drop_response=lose_first, **args), None
    except nci.ResponseLost as e:
        dropped = e.dropped.lines
    info = {"request": request_id, "host_state": "OUTCOME_UNKNOWN", "dropped_response": dropped}
    status = client.call("action.status", request_id=request_id + "-st", request=request_id)
    info["status"] = status.fields
    if status.get("known") == "yes" and status.get("state") in nci.FINAL_STATES:
        # Executed (or rejected) already: ask again with the same id, which
        # the guest answers from its record.
        resp = client.call(op, request_id=request_id, **args)
        info["resolution"] = "replayed" if resp.replayed else "re-executed"
    else:
        resp = client.call(op, request_id=request_id, **args)
        info["resolution"] = "sent again (never executed)"
    info["host_state"] = resp.state
    trace.write("recovery", **info)
    return resp, info


class Agent:
    def __init__(self, client, adapter, trace, task_id="t1"):
        self.client = client
        self.adapter = adapter
        self.trace = trace
        self.task_id = task_id
        self.actions = []

    def _decide(self, obs):
        last = None
        for attempt in range(MODEL_ATTEMPTS):
            try:
                return self.adapter.next(obs)
            except adapters.ModelUnavailable as e:
                last = e
                self.trace.write("model", task=self.task_id, adapter=self.adapter.name,
                                 attempt=attempt + 1, error=str(e))
                if attempt < len(MODEL_BACKOFF_S):
                    time.sleep(MODEL_BACKOFF_S[attempt])
        raise last

    def _end(self, state, **fields):
        self.trace.write("task", task=self.task_id, state=state, **fields)
        return dict(state=state, actions=self.actions, **fields)

    def run(self, request):
        self.trace.write("task", task=self.task_id, state="CREATED", request=request,
                         adapter=self.adapter.name)
        self.adapter.start(request)
        obs = None
        for step in range(1, MAX_STEPS + 1):
            try:
                decision = self._decide(obs)
            except adapters.ModelUnavailable as e:
                return self._end("FAILED", reason="model_unavailable", detail=str(e))
            if decision.is_final:
                bad = [a for a in self.actions if a["verification"]["host"] != "ok"]
                return self._end("FAILED" if bad else "SUCCEEDED", final=decision.final,
                                 reason="action_failed" if bad else None)
            rid = "%s-%d" % (self.task_id, step)
            try:
                op, args = tools.to_nci(decision.tool, decision.args)
            except tools.ToolError as e:
                # Not sent to the guest; the model is told and may correct itself.
                self.trace.write("rejected_tool", id=rid, task=self.task_id, request=request,
                                 decision=decision.to_json(), error=str(e))
                obs = {"tool": decision.tool, "state": "REJECTED", "fields": {"code": "BAD_TOOL"},
                       "items": [], "lines": ["host rejected the tool call: %s" % e]}
                continue
            line = nci.format_request(rid, op, args)
            resp = self.client.call(op, request_id=rid, **args)
            guest, host = guest_verdict(decision.tool, resp)
            rec = self.trace.write(
                "action", id=rid, task=self.task_id, step=step, request=request,
                decision=decision.to_json(),
                call={"id": rid, "op": op, "tool": decision.tool, "args": args, "line": line},
                result=resp.to_json(),
                verification={"guest": guest, "guest_checks": resp.get("checks"), "host": host})
            self.actions.append(rec)
            obs = observation(decision.tool, resp)
        return self._end("FAILED", reason="too_many_steps")
