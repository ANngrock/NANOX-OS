"""Host-side scenario scripts of the M3 bench (docs/m3-core.md §8).

Each script drives one bridge session and returns a list of problems found
by host-side checks (empty: all checks passed).  A problem is a string
"<code>: <explanation>"; the code of the first problem is reported to the
guest in session.close (reason=...).

  agent        criteria 2, 3, 4, 6: the model adapter handles the request
               "list the tasks, start the test workload, measure it, stop it"
  nci          criterion 1: every operation of the first NCI set, the
               kernel's state events and the executor's task engine
  faults       criterion 5: stale references, repeated requests, a lost
               response and a lost request
  model-down   criterion 5: the model is unavailable
"""

import agent as agentmod
import adapters
import m4scripts
import nci
import bridgetrace as tracemod

AGENT_REQUEST = ("Перечисли задачи системы, запусти тестовую нагрузку, измерь, сколько CPU "
                 "она получает, и останови её.")


class Checks:
    def __init__(self):
        self.problems = []

    def expect(self, cond, code, msg):
        if not cond:
            self.problems.append("%s: %s" % (code, msg))
        return cond


def alive_tasks(client, name):
    resp = client.call("task.list")
    return [i for i in resp.items if i.get("name") == name and
            i.get("state") in ("ready", "running", "blocked")]


def cleanup(client, name="load"):
    """Stops whatever is left of the workload (after a failed check)."""
    for i in alive_tasks(client, name):
        client.call("task.terminate", target=i["ref"])


def run_agent(ctx):
    c, client = Checks(), ctx["client"]
    ag = agentmod.Agent(client, adapters.make(ctx.get("adapter", "mock")), ctx["trace"])
    result = ag.run(ctx.get("request") or AGENT_REQUEST)
    ctx["agent_result"] = {k: v for k, v in result.items() if k != "actions"}
    tools_used = [a["decision"]["tool"] for a in result["actions"]]
    c.expect(result["state"] == "SUCCEEDED", "agent_task_failed",
             "host task ended %s (%s)" % (result["state"], result.get("reason")))
    for need in ("list_tasks", "spawn_task", "measure_task", "terminate_task"):
        c.expect(need in tools_used, "agent_step_missing", "the model never called %s" % need)
    by_tool = {}
    for a in result["actions"]:
        by_tool.setdefault(a["decision"]["tool"], []).append(a)
    for a in result["actions"]:
        c.expect(a["verification"]["host"] == "ok", "action_not_verified",
                 "%s %s: %s" % (a["id"], a["decision"]["tool"], a["verification"]["host"]))
    spawn = by_tool.get("spawn_task", [None])[0]
    if spawn and spawn["result"]["state"] == "SUCCEEDED":
        ref = spawn["result"]["fields"]["ref"]
        m = by_tool.get("measure_task", [None])[0]
        if m and m["result"]["state"] == "SUCCEEDED":
            f = m["result"]["fields"]
            c.expect(int(f["cpu_ticks"]) > 0, "measure_zero",
                     "the running workload got no CPU time in the window")
            c.expect(int(f["window_ticks"]) >= 50, "measure_window",
                     "window of %s ticks, 50 requested" % f["window_ticks"])
        # Independent host check of the end state, outside the agent loop.
        ins = client.call("task.inspect", target=ref)
        c.expect(ins.state == "FAILED" and ins.get("code") == "GONE", "workload_not_gone",
                 "after the agent finished, inspect of %s gave %s %s" % (
                     ref, ins.state, ins.get("code")))
        c.expect(not alive_tasks(client, "load"), "workload_alive",
                 "a load task is still alive after the agent finished")
    c.problems += tracemod.check_trace(ctx["trace"].records)
    if c.problems:
        cleanup(client)
    return c.problems


def run_nci(ctx):
    c, client, hello = Checks(), ctx["client"], ctx["hello"]
    boot = hello["boot"]
    d = client.call("system.describe")
    c.expect(d.ok and d.get("boot") == boot and d.get("core") == hello["core"], "describe",
             "system.describe: %s boot=%s core=%s" % (d.state, d.get("boot"), d.get("core")))
    ops = (d.get("ops") or "").split(",")
    for op in ("system.describe", "task.list", "task.inspect", "task.spawn", "task.terminate",
               "memory.stats", "event.subscribe"):
        c.expect(op in ops, "describe_ops", "operation %s not offered" % op)
    m0 = client.call("memory.stats")
    sub = client.call("event.subscribe")
    c.expect(sub.ok, "subscribe", "event.subscribe: %s" % sub.state)
    lst = client.call("task.list")
    names = {(i["name"], i["kind"]) for i in lst.items}
    for want in (("kmain", "kernel"), ("idle", "kernel"), ("reaper", "kernel"), ("core", "user")):
        c.expect(want in names, "list", "task.list lacks %s (%s)" % want)
    c.expect(any(i["ref"] == hello["core"] and i["state"] == "running" for i in lst.items),
             "list_self", "the executor is not listed as running")
    sp = client.call("task.spawn", program="load")
    if not c.expect(sp.ok and sp.get("verify") == "ok", "spawn",
                    "task.spawn: %s %s %s" % (sp.state, sp.get("code"), sp.get("detail"))):
        cleanup(client)
        return c.problems
    ref = sp.get("ref")
    ins = client.call("task.inspect", target=ref)
    c.expect(ins.ok and ins.get("state") in ("ready", "running") and ins.get("kind") == "user"
             and ins.get("rev") == "2", "inspect",
             "task.inspect: %s state=%s rev=%s" % (ins.state, ins.get("state"), ins.get("rev")))
    term = client.call("task.terminate", request_id="nci-term", target=ref,
                       expect_rev=ins.get("rev") or "0")
    c.expect(term.ok and term.get("verify") == "ok" and term.get("state") == "reaped",
             "terminate", "task.terminate: %s %s %s" % (term.state, term.get("code"),
                                                          term.get("detail")))
    poll = client.call("event.poll", sub=sub.get("sub") or "1")
    mine = [i for i in poll.items if i["task"] == ref]
    types = [i["type"] for i in mine]
    c.expect(types == ["created", "started", "killed", "reaped"], "events",
             "events of %s: %s" % (ref, types))
    seqs = [int(i["seq"]) for i in poll.items]
    c.expect(seqs == list(range(int(sub.get("next", "0")), int(sub.get("next", "0")) + len(seqs))),
             "event_seq", "event sequence numbers %s do not continue the subscription" % seqs)
    killed = [i for i in mine if i["type"] == "killed"]
    core_id = hello["core"].rsplit("/", 1)[1]
    c.expect(bool(killed) and killed[0]["arg"] == core_id, "event_killer",
             "killed event does not name the executor #%s" % core_id)
    poll2 = client.call("event.poll", sub=sub.get("sub") or "1")
    c.expect(poll2.ok and poll2.get("count") == "0", "event_cursor",
             "second poll returned %s events" % poll2.get("count"))
    m1 = client.call("memory.stats")
    c.expect(m0.get("free_pages") == m1.get("free_pages") and
             m0.get("page_tables") == m1.get("page_tables"), "memory_returned",
             "free_pages %s -> %s, page_tables %s -> %s" % (
                 m0.get("free_pages"), m1.get("free_pages"), m0.get("page_tables"),
                 m1.get("page_tables")))
    gone = client.call("task.inspect", target=ref)
    c.expect(gone.state == "FAILED" and gone.get("code") == "GONE", "inspect_gone",
             "inspect after terminate: %s %s" % (gone.state, gone.get("code")))
    st = client.call("action.status", request="nci-term")
    c.expect(st.get("known") == "yes" and st.get("state") == "SUCCEEDED" and
             st.get("op") == "task.terminate", "action_status",
             "action.status: known=%s state=%s" % (st.get("known"), st.get("state")))
    unk = client.call("frobnicate.now")
    c.expect(unk.state == "REJECTED" and unk.get("code") == "UNKNOWN_OP", "unknown_op",
             "unknown operation: %s %s" % (unk.state, unk.get("code")))
    client.send_line("REQ bad-1 task.spawn program=load program=load")
    dup = client.read_response("bad-1")
    c.expect(dup.state == "REJECTED" and dup.get("detail") == "duplicate_argument", "bad_request",
             "duplicate argument: %s %s" % (dup.state, dup.get("detail")))
    miss = client.call("task.terminate")
    c.expect(miss.state == "FAILED" and miss.get("code") == "BAD_REQUEST" and
             miss.get("effects") == "none", "missing_arg",
             "terminate without target: %s %s" % (miss.state, miss.get("code")))
    if c.problems:
        cleanup(client)
    return c.problems


def run_faults(ctx):
    c, client, hello = Checks(), ctx["client"], ctx["hello"]
    trace = ctx["trace"]
    boot = hello["boot"]
    a = client.call("task.spawn", request_id="f-spawn-a", program="load")
    if not c.expect(a.ok, "spawn", "task.spawn: %s %s" % (a.state, a.get("code"))):
        return c.problems
    ref_a = a.get("ref")
    task_a = ref_a.rsplit("/", 1)[1]

    # Stale and unknown references, failed precondition: no effect.
    other_boot = "%016x" % (int(boot, 16) ^ 1)
    r = client.call("task.inspect", target="task/%s/%s" % (other_boot, task_a))
    c.expect(r.state == "FAILED" and r.get("code") == "STALE_REF", "stale_boot",
             "reference from another boot: %s %s" % (r.state, r.get("code")))
    r = client.call("task.terminate", target="task/%s/%s" % (other_boot, task_a))
    c.expect(r.state == "FAILED" and r.get("code") == "STALE_REF" and r.get("effects") == "none",
             "stale_boot_terminate", "terminate through a reference from another boot: %s %s"
             % (r.state, r.get("code")))
    r = client.call("task.inspect", target="task/%s/999" % boot)
    c.expect(r.state == "FAILED" and r.get("code") == "NOT_FOUND", "not_found",
             "never issued id: %s %s" % (r.state, r.get("code")))
    r = client.call("task.terminate", target=ref_a, expect_rev="999")
    c.expect(r.state == "FAILED" and r.get("code") == "CONFLICT" and r.get("effects") == "none",
             "conflict", "terminate with a stale revision: %s %s" % (r.state, r.get("code")))
    r = client.call("task.inspect", target=ref_a)
    c.expect(r.ok and r.get("state") in ("ready", "running"), "no_effect",
             "after the refused requests %s is %s" % (ref_a, r.get("state")))

    # A repeated request is answered from the record, not executed again.
    b1 = client.call("task.spawn", request_id="f-spawn-b", program="load")
    b2 = client.call("task.spawn", request_id="f-spawn-b", program="load")
    c.expect(b1.ok and b2.ok and b2.replayed and b2.get("ref") == b1.get("ref"),
             "retry_not_replayed", "repeat of f-spawn-b: replayed=%s refs %s / %s" % (
                 b2.replayed, b1.get("ref"), b2.get("ref")))
    loads = alive_tasks(client, "load")
    c.expect(len(loads) == 2, "retry_created_second_task",
             "after spawning A and B (B requested twice) %d load tasks are alive" % len(loads))
    r = client.call("task.spawn", request_id="f-spawn-b", program="nosuch")
    c.expect(r.state == "REJECTED" and r.get("code") == "ID_REUSED", "id_reused",
             "same id, different request: %s %s" % (r.state, r.get("code")))

    # Lost response: the outcome is read back, the action is not repeated.
    resp, info = agentmod.call_reliably(client, trace, "task.terminate", "f-kill-a",
                                        {"target": ref_a}, lose_first=True)
    c.expect(info["status"].get("known") == "yes" and
             info["status"].get("state") == "SUCCEEDED", "loss_status",
             "action.status after the lost response: %s" % info["status"])
    c.expect(resp.ok and resp.replayed and info["resolution"] == "replayed", "loss_replay",
             "after the lost response: %s replayed=%s (%s)" % (resp.state, resp.replayed,
                                                              info["resolution"]))
    r = client.call("task.inspect", target=ref_a)
    c.expect(r.state == "FAILED" and r.get("code") == "GONE", "loss_effect",
             "A after terminate with lost response: %s %s" % (r.state, r.get("code")))
    ref_b = b1.get("ref")
    r = client.call("task.inspect", target=ref_b)
    c.expect(r.ok and r.get("state") in ("ready", "running"), "loss_side_effect",
             "B after terminating A: %s %s" % (r.state, r.get("state")))

    # Lost request (simulated by not sending it): the status is unknown, so
    # sending it now is safe and executes it once.
    st = client.call("action.status", request="f-kill-b")
    c.expect(st.get("known") == "no", "lost_request_status",
             "status of a request that never arrived: known=%s" % st.get("known"))
    resp = client.call("task.terminate", request_id="f-kill-b", target=ref_b)
    c.expect(resp.ok and resp.get("verify") == "ok", "terminate_b",
             "terminate B: %s %s" % (resp.state, resp.get("code")))

    # Old reference after the task ended, with a new task running.
    d = client.call("task.spawn", request_id="f-spawn-d", program="load")
    r = client.call("task.terminate", target=ref_a)
    c.expect(r.state == "FAILED" and r.get("code") == "GONE" and r.get("effects") == "none",
             "old_ref", "terminate through the old reference of A: %s %s" % (r.state,
                                                                           r.get("code")))
    if d.ok:
        r = client.call("task.inspect", target=d.get("ref"))
        c.expect(r.ok and r.get("state") in ("ready", "running"), "old_ref_side_effect",
                 "new task D after the old-reference request: %s %s" % (r.state, r.get("state")))
        client.call("task.terminate", target=d.get("ref"))
    c.expect(not alive_tasks(client, "load"), "leftover", "load tasks left alive")
    cleanup(client)
    return c.problems


def run_model_down(ctx):
    c, client = Checks(), ctx["client"]
    ag = agentmod.Agent(client, adapters.make(ctx.get("adapter", "unavailable")), ctx["trace"])
    result = ag.run(ctx.get("request") or AGENT_REQUEST)
    ctx["agent_result"] = {k: v for k, v in result.items() if k != "actions"}
    c.expect(result["state"] == "FAILED" and result.get("reason") == "model_unavailable",
             "model_down_state", "host task ended %s (%s)" % (result["state"],
                                                              result.get("reason")))
    c.expect(not result["actions"], "model_down_actions",
             "%d actions were sent without a model decision" % len(result["actions"]))
    attempts = [r for r in ctx["trace"].records if r["kind"] == "model"]
    c.expect(len(attempts) == agentmod.MODEL_ATTEMPTS, "model_down_attempts",
             "%d attempts recorded, expected %d" % (len(attempts), agentmod.MODEL_ATTEMPTS))
    # The system itself stays usable without the model (direct control path).
    d = client.call("system.describe")
    c.expect(d.ok and d.get("actions") == "1", "model_down_guest",
             "guest after the model failure: %s actions=%s" % (d.state, d.get("actions")))
    lst = client.call("task.list")
    c.expect(lst.ok and any(i["name"] == "core" for i in lst.items), "model_down_list",
             "task.list without the model: %s" % lst.state)
    c.problems += tracemod.check_trace(ctx["trace"].records)
    return c.problems


SCRIPTS = {"agent": run_agent, "nci": run_nci, "faults": run_faults,
           "model-down": run_model_down}
SCRIPTS.update(m4scripts.SCRIPTS)  # M4 (docs/m4-store.md §8)
