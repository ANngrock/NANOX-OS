"""Unit tests for the M3 host bridge (tools/bridge/) and the harness's
bridge expectations (tools/bench/harness.py)."""

import socket
import sys
import threading
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "tools" / "bench"))
sys.path.insert(0, str(REPO / "tools" / "bridge"))

import adapters  # noqa: E402
import agent  # noqa: E402
import bridgetrace  # noqa: E402
import harness  # noqa: E402
import nci  # noqa: E402
import session  # noqa: E402
import toolspec  # noqa: E402


class FakeGuest(threading.Thread):
    """Answers NCI requests over a socket like bin/core would, with a record
    of executed requests (dedup) unless dedup=False."""

    def __init__(self, sock, dedup=True, preamble=b""):
        super().__init__(daemon=True)
        self.sock, self.dedup, self.preamble = sock, dedup, preamble
        self.records, self.executed = {}, []

    def reply(self, lines):
        self.sock.sendall(("\n".join(lines) + "\n").encode())

    def run(self):
        self.sock.sendall(self.preamble + b"HELLO nci=1 core=task/00000000000000aa/4 "
                          b"boot=00000000000000aa dedup=on\n")
        buf = b""
        while True:
            chunk = self.sock.recv(4096)
            if not chunk:
                return
            buf += chunk
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                self.handle(line.decode())

    def handle(self, line):
        parts = line.split(" ")
        rid, op = parts[1], parts[2]
        args = dict(p.split("=", 1) for p in parts[3:])
        if op == "action.status":
            rec = self.records.get(args["request"])
            if rec:
                self.reply(["RES %s SUCCEEDED request=%s known=yes state=%s op=x"
                            % (rid, args["request"], rec[0].split(" ")[2]), "END " + rid])
            else:
                self.reply(["RES %s SUCCEEDED request=%s known=no" % (rid, args["request"]),
                            "END " + rid])
            return
        if self.dedup and rid in self.records:
            first, rest = self.records[rid][0], self.records[rid][1:]
            self.reply([first + " replayed=1"] + rest)
            return
        self.executed.append(rid)
        lines = ["RES %s SUCCEEDED verify=ok n=%d" % (rid, len(self.executed)),
                 "ITEM %s k=v" % rid, "END " + rid]
        self.records[rid] = lines
        self.reply(lines)


def pair(tc, **kw):
    a, b = socket.socketpair()
    tc.addCleanup(a.close)
    tc.addCleanup(b.close)
    g = FakeGuest(b, **kw)
    g.start()
    return nci.NciClient(a, timeout_s=5), g


class WireFormatTest(unittest.TestCase):
    def test_format_request(self):
        self.assertEqual(nci.format_request("t1-2", "task.spawn", {"program": "load"}),
                         "REQ t1-2 task.spawn program=load")
        for bad in (("a b", "task.list", {}), ("x", "Task.list", {}),
                    ("x", "task.list", {"k": "a b"}), ("x", "task.list", {"K": "v"}),
                    ("x", "task.list", {"k": ""})):
            with self.assertRaises(ValueError):
                nci.format_request(*bad)

    def test_parse_response(self):
        r = nci.parse_response(["RES a SUCCEEDED x=1 y=task/ab/3", "ITEM a k=v", "END a"])
        self.assertTrue(r.ok)
        self.assertEqual(r.fields, {"x": "1", "y": "task/ab/3"})
        self.assertEqual(r.items, [{"k": "v"}])
        self.assertFalse(r.replayed)
        with self.assertRaises(nci.BridgeError):
            nci.parse_response(["RES a SUCCEEDED", "ITEM b k=v", "END a"])
        with self.assertRaises(nci.BridgeError):
            nci.parse_response(["RES a SUCCEEDED", "END b"])
        with self.assertRaises(nci.BridgeError):
            nci.parse_response(["RES a SUCCEEDED novalue", "END a"])


class ClientTest(unittest.TestCase):
    def test_hello_after_firmware_noise_and_call(self):
        client, g = pair(self, preamble=b"\x1b[2JBdsDxe: loading Boot0001\r\n")
        hello = client.wait_hello(5)
        self.assertEqual(hello["boot"], "00000000000000aa")
        r = client.call("task.list", request_id="q1")
        self.assertEqual((r.id, r.state, r.items), ("q1", "SUCCEEDED", [{"k": "v"}]))
        self.assertEqual(r.request_line, "REQ q1 task.list")

    def test_lost_response_is_replayed_not_reexecuted(self):
        client, g = pair(self)
        client.wait_hello(5)
        tr = bridgetrace.Trace()
        resp, info = agent.call_reliably(client, tr, "task.terminate", "k1",
                                         {"target": "task/00000000000000aa/5"}, lose_first=True)
        self.assertEqual(g.executed, ["k1"])  # executed exactly once
        self.assertTrue(resp.replayed)
        self.assertEqual(info["resolution"], "replayed")
        self.assertEqual(info["status"]["known"], "yes")
        self.assertEqual(info["dropped_response"][0], "RES k1 SUCCEEDED verify=ok n=1")
        self.assertEqual([r["kind"] for r in tr.records], ["recovery"])

    def test_lost_response_without_dedup_reexecutes(self):
        client, g = pair(self, dedup=False)
        client.wait_hello(5)
        resp, info = agent.call_reliably(client, bridgetrace.Trace(), "task.spawn", "s1",
                                         {"program": "load"}, lose_first=True)
        self.assertEqual(g.executed, ["s1", "s1"])
        self.assertEqual(info["resolution"], "re-executed")

    def test_timeout(self):
        a, b = socket.socketpair()
        self.addCleanup(a.close)
        self.addCleanup(b.close)
        client = nci.NciClient(a, timeout_s=0.2)
        with self.assertRaises(nci.BridgeError):
            client.call("task.list", request_id="x")


class ToolsTest(unittest.TestCase):
    def test_to_nci(self):
        self.assertEqual(toolspec.to_nci("spawn_task", {"program": "load"}),
                         ("task.spawn", {"program": "load"}))
        self.assertEqual(toolspec.to_nci("terminate_task", {"target": "t"}),
                         ("task.terminate", {"target": "t"}))  # expect_rev optional
        self.assertEqual(toolspec.to_nci("measure_task", {"target": "t", "window_ms": 5})[1],
                         {"target": "t", "window_ms": "5"})
        for tool, args in (("format_disk", {}), ("spawn_task", {}),
                           ("list_tasks", {"x": "1"})):
            with self.assertRaises(toolspec.ToolError):
                toolspec.to_nci(tool, args)


def obs(tool, state="SUCCEEDED", items=(), **fields):
    return {"tool": tool, "state": state, "fields": fields, "items": list(items), "lines": []}


class MockModelTest(unittest.TestCase):
    def test_plan_follows_observations(self):
        m = adapters.MockModel()
        m.start("Перечисли задачи, запусти нагрузку, измерь и останови её")
        d = m.next()
        self.assertEqual(d.tool, "list_tasks")
        d = m.next(obs("list_tasks", items=[{"name": "core", "ref": "task/a/4",
                                             "state": "running"}]))
        self.assertEqual((d.tool, d.args), ("spawn_task", {"program": "load"}))
        d = m.next(obs("spawn_task", ref="task/a/5", rev="2"))
        self.assertEqual((d.tool, d.args["target"]), ("measure_task", "task/a/5"))
        d = m.next(obs("measure_task", window_ticks="50", cpu_ticks="49", share_pct="98"))
        self.assertEqual((d.tool, d.args), ("terminate_task",
                                            {"target": "task/a/5", "expect_rev": "2"}))
        d = m.next(obs("terminate_task", state="SUCCEEDED", **{"state_": "x"}))
        self.assertEqual(d.tool, "list_tasks")
        d = m.next(obs("list_tasks", items=[]))
        self.assertTrue(d.is_final)
        self.assertIn("49", d.final)

    def test_stops_on_failure(self):
        m = adapters.MockModel()
        m.start("останови нагрузку")
        m.next()
        d = m.next(obs("list_tasks", state="FAILED", code="GONE"))
        self.assertTrue(d.is_final)
        self.assertIn("FAILED", d.final)

    def test_unrecognised_request(self):
        m = adapters.MockModel()
        m.start("напиши стихотворение")
        self.assertTrue(m.next().is_final)

    def test_anthropic_without_key_is_unavailable(self):
        m = adapters.AnthropicModel()
        m.key = None
        m.start("x")
        with self.assertRaises(adapters.ModelUnavailable):
            m.next()


class AgentTest(unittest.TestCase):
    def test_model_unavailable_sends_nothing(self):
        saved = agent.MODEL_BACKOFF_S
        agent.MODEL_BACKOFF_S = (0, 0)
        try:
            tr = bridgetrace.Trace()
            client, g = pair(self)
            client.wait_hello(5)
            res = agent.Agent(client, adapters.UnavailableModel(), tr).run("x")
        finally:
            agent.MODEL_BACKOFF_S = saved
        self.assertEqual((res["state"], res["reason"]), ("FAILED", "model_unavailable"))
        self.assertEqual(g.executed, [])
        self.assertEqual(sum(r["kind"] == "model" for r in tr.records), agent.MODEL_ATTEMPTS)
        self.assertEqual(bridgetrace.check_trace(tr.records), [])

    def test_guest_verdict(self):
        ok = nci.parse_response(["RES a SUCCEEDED verify=ok", "END a"])
        unverified = nci.parse_response(["RES a SUCCEEDED", "END a"])
        failed = nci.parse_response(["RES a FAILED code=GONE", "END a"])
        self.assertEqual(agent.guest_verdict("spawn_task", ok), ("ok", "ok"))
        self.assertTrue(agent.guest_verdict("spawn_task", unverified)[1].startswith("failed"))
        self.assertEqual(agent.guest_verdict("list_tasks", unverified)[1], "ok")
        self.assertTrue(agent.guest_verdict("list_tasks", failed)[1].startswith("failed"))


def action(**over):
    rec = {"schema": bridgetrace.SCHEMA, "kind": "action", "id": "t1-1", "request": "r",
           "decision": {"tool": "list_tasks"},
           "call": {"id": "t1-1", "op": "task.list", "tool": "list_tasks",
                    "line": "REQ t1-1 task.list"},
           "result": {"id": "t1-1", "state": "SUCCEEDED"},
           "verification": {"guest": "ok", "host": "ok"}}
    rec.update(over)
    return rec


def task(state):
    return {"schema": bridgetrace.SCHEMA, "kind": "task", "task": "t1", "state": state}


class TraceTest(unittest.TestCase):
    def test_complete(self):
        self.assertEqual(bridgetrace.check_trace([task("CREATED"), action(),
                                                  task("SUCCEEDED")]), [])

    def test_missing_parts(self):
        for part in bridgetrace.ACTION_PARTS:
            probs = bridgetrace.check_trace([action(**{part: None})])
            self.assertTrue(probs and probs[0].startswith("trace_incomplete"), part)

    def test_inconsistent(self):
        bad_call = action(call={"id": "t1-1", "op": "task.spawn", "tool": "list_tasks",
                                "line": "REQ t1-1 task.list"})
        self.assertTrue(bridgetrace.check_trace([bad_call]))
        self.assertTrue(bridgetrace.check_trace([action(result={"id": "t1-9"})]))
        self.assertTrue(bridgetrace.check_trace([action(decision={"tool": "spawn_task"})]))
        self.assertTrue(bridgetrace.check_trace([action(verification={"guest": "ok"})]))
        self.assertEqual(bridgetrace.check_trace([task("CREATED")]),
                         ["trace_task: task t1 never ended"])


class HarnessBridgeTest(unittest.TestCase):
    def test_check_bridge(self):
        good = {"ok": True, "problems": []}
        bad = {"ok": False, "problems": ["retry_created_second_task: 3 alive"]}
        self.assertEqual(harness.check_bridge({"ok": True}, good), [])
        self.assertTrue(harness.check_bridge({"ok": True}, bad))
        self.assertEqual(harness.check_bridge(
            {"ok": False, "problems": ["^retry_created"]}, bad), [])
        self.assertTrue(harness.check_bridge({"ok": False, "problems": ["^other"]}, bad))
        self.assertTrue(harness.check_bridge({"ok": True}, None))

    def test_reason_code(self):
        self.assertEqual(session.reason_code("retry_not_replayed: x y"), "retry_not_replayed")
        self.assertEqual(session.reason_code("a b: c"), "a_b")


if __name__ == "__main__":
    unittest.main()
