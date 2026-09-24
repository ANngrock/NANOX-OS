"""Test provider of the M5 scenarios (docs/m5-net.md §10): a local HTTPS
server that speaks the provider protocol the guest uses (POST /v1/messages,
streamed as server-sent events, Messages API shapes) with a scripted
policy in place of a language model.  Only the Python standard library;
TLS by OpenSSL (Python's ssl module) with a certificate of the test PKI
(tools/net/pki.py), which the guest verifies like any other.

The policy is NOT a language model.  It recognises a few requests by
keywords and chooses each step from the tool results it got back, like
the M3 mock adapter (tools/bridge/adapters.py):

  workload (default)  list_tasks, spawn_task(load), measure_task,
                      terminate_task, list_tasks, final answer
  "describe"          describe_system, final answer
  "nosuch"            spawn_task(nosuch): the executor fails the action
  "bogus tool"        calls a tool that does not exist
  any tool result with is_error: stop and answer with the failure

Faults are queued by the scenario script and applied one per request
(push()):

  http:<status>[:retry=<s>]   error response with an error body
  malformed_http              not an HTTP response
  malformed_json              event stream whose first event is not JSON
  sse_error:<type>            the stream reports an error event
  cut                         part of the stream, then TCP FIN without
                              close_notify
  early_close                 part of the stream, then close_notify
  delay:<s>                   read the request, answer nothing for <s> s
  reset                       read the request, reset the connection (RST)
  idle_close                  answer normally, then close the connection

Everything the server saw is recorded (requests(), problems) so that the
scripts can compare the guest's telemetry with the provider's own view.
"""

import json
import socket
import ssl
import struct
import threading
import time

import m5host

API_VERSION = "2023-06-01"
MODEL = "nanox-mock-model"  # a test identifier, not a real model

ERROR_TYPES = {400: "invalid_request_error", 401: "authentication_error",
               403: "permission_error", 404: "not_found_error", 429: "rate_limit_error",
               500: "api_error", 529: "overloaded_error"}
REASONS = {200: "OK", 400: "Bad Request", 401: "Unauthorized", 403: "Forbidden",
           404: "Not Found", 429: "Too Many Requests", 500: "Internal Server Error",
           529: "Overloaded"}


def nci_fields(text):
    """Fields of the first line of an NCI response ("RES id STATE k=v ...")."""
    first = text.split("\n", 1)[0].split(" ")
    fields = {"_state": first[2] if len(first) > 2 else "?"}
    for tok in first[3:]:
        if "=" in tok:
            k, v = tok.split("=", 1)
            fields[k] = v
    return fields


def nci_items(text):
    out = []
    for line in text.split("\n")[1:]:
        if line.startswith("ITEM "):
            out.append(dict(t.split("=", 1) for t in line.split(" ")[2:] if "=" in t))
    return out


class Policy:
    """Scripted decisions from the conversation (stateless)."""

    def __init__(self, messages):
        self.question = ""
        first = messages[0]["content"] if messages else ""
        if isinstance(first, str):
            self.question = first
        self.steps = []  # (tool name, input, result text, is_error)
        pending = {}
        for m in messages:
            if m["role"] == "assistant" and isinstance(m["content"], list):
                for b in m["content"]:
                    if b.get("type") == "tool_use":
                        pending[b["id"]] = (b["name"], b.get("input", {}))
            elif m["role"] == "user" and isinstance(m["content"], list):
                for b in m["content"]:
                    if b.get("type") == "tool_result":
                        name, inp = pending.pop(b["tool_use_id"], ("?", {}))
                        content = b.get("content", "")
                        if isinstance(content, list):
                            content = "".join(x.get("text", "") for x in content)
                        self.steps.append((name, inp, content, bool(b.get("is_error"))))

    def done(self, tool):
        return [s for s in self.steps if s[0] == tool]

    def decide(self):
        """("tool", name, input) or ("text", answer)."""
        q = self.question.lower()
        for name, inp, text, err in self.steps:
            if err:
                f = nci_fields(text)
                return ("text", "Действие %s не выполнено: %s %s." % (
                    name, f.get("_state", "?"), f.get("code", text.strip()[:60])))
        if "bogus tool" in q:
            return ("tool", "format_disk", {"device": "all"})
        if "nosuch" in q:
            return ("tool", "spawn_task", {"program": "nosuch"})
        if "describe" in q:
            if not self.done("describe_system"):
                return ("tool", "describe_system", {})
            f = nci_fields(self.done("describe_system")[0][2])
            return ("text", "Система работает: задач %s, частота %s Гц." % (
                f.get("tasks", "?"), f.get("hz", "?")))
        # the M3 workload scenario
        if not self.done("list_tasks"):
            return ("tool", "list_tasks", {})
        spawn = self.done("spawn_task")
        if not spawn:
            return ("tool", "spawn_task", {"program": "load"})
        sf = nci_fields(spawn[0][2])
        ref = sf.get("ref", "?")
        if not self.done("measure_task"):
            return ("tool", "measure_task", {"target": ref, "window_ms": "500"})
        mf = nci_fields(self.done("measure_task")[0][2])
        if not self.done("terminate_task"):
            return ("tool", "terminate_task", {"target": ref,
                                               "expect_rev": mf.get("rev") or sf.get("rev", "1")})
        if len(self.done("list_tasks")) < 2:
            return ("tool", "list_tasks", {})
        before = nci_items(self.done("list_tasks")[0][2])
        tf = nci_fields(self.done("terminate_task")[0][2])
        return ("text", "Задач до запуска: %d. Запущена нагрузка %s; за окно %s тиков она "
                "получила %s тиков CPU (%s%%). Нагрузка остановлена (%s), ресурсы освобождены." % (
                    len(before), ref, mf.get("window_ticks", "?"), mf.get("cpu_ticks", "?"),
                    mf.get("share_pct", "?"), tf.get("state", "?")))


def sse(event, data):
    return ("event: %s\ndata: %s\n\n" % (event, json.dumps(data, ensure_ascii=False))).encode()


def stream_events(decision, msg_no, in_tokens):
    """The event stream of one decision, as a list of byte strings."""
    ev = [sse("message_start", {"type": "message_start", "message": {
        "id": "msg_test_%d" % msg_no, "type": "message", "role": "assistant", "model": MODEL,
        "content": [], "stop_reason": None, "stop_sequence": None,
        "usage": {"input_tokens": in_tokens, "output_tokens": 1}}}),
        sse("ping", {"type": "ping"})]
    if decision[0] == "text":
        text = decision[1]
        cut = len(text) // 2
        ev.append(sse("content_block_start", {"type": "content_block_start", "index": 0,
                                              "content_block": {"type": "text", "text": ""}}))
        for part in (text[:cut], text[cut:]):
            ev.append(sse("content_block_delta", {"type": "content_block_delta", "index": 0,
                                                  "delta": {"type": "text_delta", "text": part}}))
        ev.append(sse("content_block_stop", {"type": "content_block_stop", "index": 0}))
        stop, out_tokens = "end_turn", max(1, len(text) // 4)
    else:
        _, name, inp = decision
        ev.append(sse("content_block_start", {"type": "content_block_start", "index": 0,
                                              "content_block": {"type": "text", "text": ""}}))
        ev.append(sse("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {
            "type": "text_delta", "text": "Вызываю %s." % name}}))
        ev.append(sse("content_block_stop", {"type": "content_block_stop", "index": 0}))
        ev.append(sse("content_block_start", {"type": "content_block_start", "index": 1,
                                              "content_block": {"type": "tool_use",
                                                                "id": "toolu_%d" % msg_no,
                                                                "name": name, "input": {}}}))
        raw = json.dumps(inp)
        for i in range(0, len(raw), 7):
            ev.append(sse("content_block_delta", {"type": "content_block_delta", "index": 1,
                                                  "delta": {"type": "input_json_delta",
                                                            "partial_json": raw[i:i + 7]}}))
        ev.append(sse("content_block_stop", {"type": "content_block_stop", "index": 1}))
        stop, out_tokens = "tool_use", 20
    ev.append(sse("message_delta", {"type": "message_delta", "delta": {
        "stop_reason": stop, "stop_sequence": None}, "usage": {"output_tokens": out_tokens}}))
    ev.append(sse("message_stop", {"type": "message_stop"}))
    return ev


def chunk(data):
    return b"%x\r\n%s\r\n" % (len(data), data)


class ProviderServer:
    def __init__(self, key, profile="ec-leaf"):
        self.key = key
        self.profile = profile
        self.ctx = m5host.server_context(profile)
        self.lock = threading.Lock()
        self.faults = []
        self.log = []       # one record per request
        self.problems = []  # protocol violations of the client
        self.conns = 0
        self.msg_no = 0
        self.stop = False
        self.sock = None
        self.port = None
        self._listen()
        threading.Thread(target=self._run, daemon=True).start()

    # ---- control from the scripts ----

    def push(self, *faults):
        with self.lock:
            self.faults.extend(faults)

    def pending_faults(self):
        with self.lock:
            return list(self.faults)

    def requests(self):
        with self.lock:
            return list(self.log)

    def pause(self):
        """Stops listening: new connections are refused."""
        with self.lock:
            s, self.sock = self.sock, None
        if s:
            s.close()

    def resume(self):
        self._listen(self.port)

    def _listen(self, port=0):
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        s.bind(("127.0.0.1", port))
        s.listen(8)
        s.settimeout(0.2)
        self.port = s.getsockname()[1]
        with self.lock:
            self.sock = s

    # ---- serving ----

    def _run(self):
        while not self.stop:
            with self.lock:
                s = self.sock
            if s is None:
                time.sleep(0.05)
                continue
            try:
                raw, _ = s.accept()
            except socket.timeout:
                continue
            except OSError:
                time.sleep(0.05)
                continue
            with self.lock:
                self.conns += 1
                cid = self.conns
            threading.Thread(target=self._serve, args=(raw, cid), daemon=True).start()

    def _read_request(self, s, buf):
        while b"\r\n\r\n" not in buf:
            chunk_ = s.recv(65536)
            if not chunk_:
                return None, buf
            buf += chunk_
        head, rest = buf.split(b"\r\n\r\n", 1)
        lines = head.decode("latin-1").split("\r\n")
        method, path, _ = (lines[0].split(" ") + ["", "", ""])[:3]
        headers = {}
        for line in lines[1:]:
            k, _, v = line.partition(":")
            headers[k.strip().lower()] = v.strip()
        n = int(headers.get("content-length", "0"))
        while len(rest) < n:
            chunk_ = s.recv(65536)
            if not chunk_:
                return None, b""
            rest += chunk_
        return (method, path, headers, rest[:n]), rest[n:]

    def _check(self, method, path, headers, body):
        """Problems of one request; the parsed body (or None)."""
        p = []
        if method != "POST" or path != "/v1/messages":
            p.append("request line %s %s" % (method, path))
        for k, want in (("anthropic-version", API_VERSION), ("content-type", "application/json"),
                        ("accept", "text/event-stream")):
            if headers.get(k) != want:
                p.append("header %s=%r" % (k, headers.get(k)))
        if not headers.get("host", "").startswith("provider.nanox.test"):
            p.append("host header %r" % headers.get("host"))
        try:
            j = json.loads(body.decode("utf-8"))
        except (UnicodeDecodeError, ValueError) as e:
            p.append("body is not JSON: %s" % e)
            return p, None
        if j.get("model") != MODEL:
            p.append("model %r" % j.get("model"))
        if j.get("stream") is not True:
            p.append("stream is not true")
        if not isinstance(j.get("max_tokens"), int):
            p.append("max_tokens")
        tools = j.get("tools", [])
        names = [t.get("name") for t in tools]
        if "list_tasks" not in names or any("input_schema" not in t for t in tools):
            p.append("tools %s" % names)
        msgs = j.get("messages", [])
        roles = [m.get("role") for m in msgs]
        if not msgs or roles[0] != "user" or any(a == b for a, b in zip(roles, roles[1:])):
            p.append("message roles %s" % roles)
        # every tool_use must be answered by a tool_result in the next message
        for i, m in enumerate(msgs):
            if m.get("role") == "assistant" and isinstance(m.get("content"), list):
                ids = {b["id"] for b in m["content"] if b.get("type") == "tool_use"}
                if ids and i + 1 < len(msgs):
                    nxt = msgs[i + 1].get("content")
                    got = {b.get("tool_use_id") for b in nxt if isinstance(b, dict)} \
                        if isinstance(nxt, list) else set()
                    if ids != got:
                        p.append("tool_use %s answered by %s" % (sorted(ids), sorted(got)))
        return p, j

    def _serve(self, raw, cid):
        raw.settimeout(60)
        try:
            s = self.ctx.wrap_socket(raw, server_side=True)
        except (ssl.SSLError, OSError) as e:
            with self.lock:
                self.log.append({"conn": cid, "handshake_error": str(e)})
            raw.close()
            return
        buf = b""
        try:
            while not self.stop:
                req, buf = self._read_request(s, buf)
                if req is None:
                    break
                if not self._answer(s, cid, *req):
                    break
        except (ssl.SSLError, OSError):
            pass
        finally:
            try:
                s.close()
            except OSError:
                pass

    def _answer(self, s, cid, method, path, headers, body):
        """Answers one request; False when the connection is to be dropped."""
        problems, j = self._check(method, path, headers, body)
        auth = headers.get("x-api-key") == self.key
        with self.lock:
            fault = self.faults.pop(0) if self.faults else None
            self.msg_no += 1
            no = self.msg_no
            rec = {"conn": cid, "n": no, "fault": fault, "auth": auth, "problems": problems,
                   "bytes": len(body), "messages": len(j.get("messages", [])) if j else 0}
            self.log.append(rec)
            self.problems += problems
        if not auth:
            rec["status"] = 401
            return self._error(s, 401)
        if problems:
            rec["status"] = 400
            return self._error(s, 400)
        decision = Policy(j["messages"]).decide()
        rec["decision"] = decision[1] if decision[0] == "tool" else "text"
        if fault and fault.startswith("http:"):
            parts = fault.split(":")
            status = int(parts[1])
            retry = parts[2].split("=")[1] if len(parts) > 2 else None
            rec["status"] = status
            return self._error(s, status, retry)
        if fault == "malformed_http":
            rec["status"] = "malformed"
            s.sendall(b"HTTX/1.1 200 OK\r\n\r\n")
            self._close_notify(s)
            return False
        if fault and fault.startswith("delay:"):
            rec["status"] = "delayed"
            time.sleep(float(fault.split(":")[1]))
            return False
        if fault == "reset":
            rec["status"] = "reset"
            raw = s  # SSLSocket: set linger on the socket and close: RST
            raw.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
            return False
        events = stream_events(decision, no, len(body) // 4)
        if fault == "malformed_json":
            events[0] = b"event: message_start\ndata: {not json\n\n"
        elif fault and fault.startswith("sse_error:"):
            events = events[:3] + [sse("error", {"type": "error", "error": {
                "type": fault.split(":", 1)[1], "message": "test fault"}})]
        head = (b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream; charset=utf-8\r\n"
                b"cache-control: no-cache\r\ntransfer-encoding: chunked\r\n"
                b"request-id: req_test_%d\r\n\r\n" % no)
        s.sendall(head)
        # the record is complete before the last byte goes out: the scripts
        # read it as soon as the guest has answered
        if fault in ("cut", "early_close"):
            rec["status"] = fault
            for e in events[:len(events) // 2]:
                s.sendall(chunk(e))
            if fault == "early_close":
                self._close_notify(s)
            return False  # "cut": the socket is closed without close_notify
        rec["status"] = 200
        for e in events:
            s.sendall(chunk(e))
        s.sendall(b"0\r\n\r\n")
        if fault == "idle_close":
            time.sleep(0.2)
            self._close_notify(s)
            return False
        return True

    def _error(self, s, status, retry=None):
        body = json.dumps({"type": "error", "error": {
            "type": ERROR_TYPES.get(status, "api_error"), "message": "test fault %d" % status},
            "request_id": "req_test"}).encode()
        head = "HTTP/1.1 %d %s\r\ncontent-type: application/json\r\ncontent-length: %d\r\n" % (
            status, REASONS.get(status, "Error"), len(body))
        if retry:
            head += "retry-after: %s\r\n" % retry
        s.sendall(head.encode() + b"\r\n" + body)
        return True

    @staticmethod
    def _close_notify(s):
        try:
            s.unwrap()
        except (ssl.SSLError, OSError, ValueError):
            pass

    def record(self):
        return {"port": self.port, "requests": self.requests(), "problems": list(self.problems),
                "pending_faults": self.pending_faults()}

    def close(self):
        self.stop = True
        self.pause()
