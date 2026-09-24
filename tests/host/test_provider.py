"""Tests of the M5 test provider (tools/bench/provider.py) and of the
agent-script helpers: the scripted policy, the event stream it produces,
and the server spoken to by Python's own TLS/HTTP client (an independent
client, so that the provider is not only checked by the guest)."""

import http.client
import json
import ssl
import sys
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "tools" / "bench"))
sys.path.insert(0, str(REPO / "tools" / "bridge"))
sys.path.insert(0, str(REPO / "tools" / "net"))
import m5host  # noqa: E402
import pki  # noqa: E402
import provider  # noqa: E402
import m5scripts  # noqa: E402


def res(_state, **fields):
    return "RES x %s %s\nEND x\n" % (_state, " ".join("%s=%s" % kv for kv in fields.items()))


def conversation(question, results):
    """messages of a conversation whose tool calls got `results`."""
    msgs = [{"role": "user", "content": question}]
    for i, (name, text, err) in enumerate(results):
        msgs.append({"role": "assistant", "content": [
            {"type": "tool_use", "id": "t%d" % i, "name": name, "input": {}}]})
        msgs.append({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t%d" % i, "content": text,
             "is_error": err}]})
    return msgs


def parse_sse(data):
    events = []
    for block in data.decode().split("\n\n"):
        if not block.strip():
            continue
        lines = dict(line.split(": ", 1) for line in block.split("\n"))
        events.append((lines["event"], json.loads(lines["data"])))
    return events


class PolicyTest(unittest.TestCase):
    def test_workload_sequence(self):
        ref = "task/0123456789abcdef/5"
        steps = []
        results = []
        fake = {"list_tasks": res("SUCCEEDED", count=4),
                "spawn_task": res("SUCCEEDED", ref=ref, rev=1),
                "measure_task": res("SUCCEEDED", rev=2, window_ticks=50, cpu_ticks=49,
                                    share_pct=98),
                "terminate_task": res("SUCCEEDED", state="reaped")}
        while True:
            d = provider.Policy(conversation("Запусти нагрузку load", results)).decide()
            if d[0] == "text":
                break
            steps.append((d[1], d[2]))
            results.append((d[1], fake[d[1]], False))
        self.assertEqual([s[0] for s in steps], ["list_tasks", "spawn_task", "measure_task",
                                                 "terminate_task", "list_tasks"])
        self.assertEqual(steps[3][1], {"target": ref, "expect_rev": "2"})
        self.assertIn("(98%)", d[1])

    def test_failure_stops(self):
        msgs = conversation("spawn nosuch", [("spawn_task", "RES x FAILED code=NOT_FOUND\n", True)])
        d = provider.Policy(msgs).decide()
        self.assertEqual(d[0], "text")
        self.assertIn("NOT_FOUND", d[1])

    def test_other_requests(self):
        self.assertEqual(provider.Policy(conversation("call a bogus tool", [])).decide()[1],
                         "format_disk")
        self.assertEqual(provider.Policy(conversation("describe the system", [])).decide()[1],
                         "describe_system")

    def test_stream_shape(self):
        for decision in (("text", "Готово: всё хорошо."),
                         ("tool", "measure_task", {"target": "task/x/5", "window_ms": "500"})):
            ev = parse_sse(b"".join(provider.stream_events(decision, 3, 10)))
            for name, data in ev:
                self.assertEqual(name, data["type"])
            names = [e[0] for e in ev]
            self.assertEqual(names[0], "message_start")
            self.assertEqual(names[-2:], ["message_delta", "message_stop"])
            if decision[0] == "tool":
                raw = "".join(d["delta"]["partial_json"] for n, d in ev
                              if n == "content_block_delta" and d["index"] == 1)
                self.assertEqual(json.loads(raw), decision[2])
                self.assertEqual(ev[-2][1]["delta"]["stop_reason"], "tool_use")
            else:
                text = "".join(d["delta"]["text"] for n, d in ev if n == "content_block_delta")
                self.assertEqual(text, decision[1])


class ServerTest(unittest.TestCase):
    """The provider with Python's http.client over TLS verified against the
    test anchor (with the provider's host name)."""

    @classmethod
    def setUpClass(cls):
        cls.srv = provider.ProviderServer("sk-test-key")
        anchor = (m5host.pki_dir("ec-leaf") / "anchor.der").read_bytes()
        cls.ctx = ssl.create_default_context(cadata=pki.pem("CERTIFICATE", anchor))

    @classmethod
    def tearDownClass(cls):
        cls.srv.close()

    def post(self, body, key="sk-test-key"):
        c = http.client.HTTPSConnection("127.0.0.1", self.srv.port, context=self.ctx, timeout=10)
        c.sock = self.ctx.wrap_socket(
            __import__("socket").create_connection(("127.0.0.1", self.srv.port), timeout=10),
            server_hostname="provider.nanox.test")
        c.putrequest("POST", "/v1/messages", skip_host=True, skip_accept_encoding=True)
        data = json.dumps(body).encode()
        for k, v in (("host", "provider.nanox.test:%d" % self.srv.port),
                     ("content-type", "application/json"), ("accept", "text/event-stream"),
                     ("anthropic-version", "2023-06-01"), ("x-api-key", key),
                     ("content-length", str(len(data)))):
            c.putheader(k, v)
        c.endheaders(data)
        r = c.getresponse()
        out = (r.status, dict(r.getheaders()), r.read())
        c.close()
        return out

    def body(self, question="describe the system"):
        return {"model": provider.MODEL, "max_tokens": 100, "stream": True,
                "tools": [{"name": "list_tasks", "input_schema": {"type": "object"}}],
                "messages": [{"role": "user", "content": question}]}

    def test_success_and_faults(self):
        status, headers, data = self.post(self.body())
        self.assertEqual(status, 200)
        ev = parse_sse(data)
        tool = [d for n, d in ev if n == "content_block_start" and d["index"] == 1]
        self.assertEqual(tool[0]["content_block"]["name"], "describe_system")
        status, _, data = self.post(self.body(), key="wrong")
        self.assertEqual(status, 401)
        self.assertEqual(json.loads(data)["error"]["type"], "authentication_error")
        self.srv.push("http:429:retry=2")
        status, headers, data = self.post(self.body())
        self.assertEqual((status, headers.get("retry-after")), (429, "2"))
        self.srv.push("sse_error:overloaded_error")
        status, _, data = self.post(self.body())
        self.assertEqual(parse_sse(data)[-1][1]["error"]["type"], "overloaded_error")
        bad = self.body()
        bad["model"] = "something-else"
        status, _, _ = self.post(bad)
        self.assertEqual(status, 400)
        self.assertTrue(any("model" in p for p in self.srv.problems))


class AskHelpersTest(unittest.TestCase):
    def test_question_encoding(self):
        q = m5scripts.WORKLOAD_Q
        args = m5scripts.ask_args(q)
        self.assertLessEqual(len(args), 3)
        joined = "".join(args[k] for k in sorted(args))
        import base64
        self.assertEqual(base64.urlsafe_b64decode(joined + "=" * (-len(joined) % 4)).decode(), q)
        with self.assertRaises(ValueError):
            m5scripts.ask_args("x" * 200)

    def test_answer_decoding(self):
        class R:
            items = [{"seq": "1", "text": "tdGC"}, {"seq": "0", "text": "0L_RgNC40LLQ"}]
        self.assertEqual(m5scripts.answer_of(R()), "привет")


if __name__ == "__main__":
    unittest.main()
