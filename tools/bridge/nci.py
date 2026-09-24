"""NCI v1 client of the host bridge (M3).

Wire format (docs/m3-core.md §4): the host writes one request line
``REQ <id> <op> [key=value ...]``; the guest executor answers
``RES <id> <STATE> [key=value ...]``, zero or more ``ITEM <id> ...`` lines and
``END <id>``.  Before the first request the guest sends
``HELLO nci=1 core=<ref> boot=<hex> dedup=on|off``.

This module is transport only: it knows nothing about models or scenarios.
It can simulate the loss of a response (the bytes are read from the link and
thrown away, as if the link had dropped them), which the fault scenarios use.
"""

import re
import socket
import time

ID_RE = re.compile(r"^[A-Za-z0-9._-]{1,32}$")
OP_RE = re.compile(r"^[a-z][a-z.]{0,30}[a-z]$")
KEY_RE = re.compile(r"^[a-z_]{1,16}$")
VAL_RE = re.compile(r"^[A-Za-z0-9._/:,-]{1,64}$")
FINAL_STATES = ("SUCCEEDED", "FAILED", "OUTCOME_UNKNOWN", "CANCELLED", "REJECTED")


class BridgeError(Exception):
    """The link failed (closed, timed out, garbage)."""


class ResponseLost(Exception):
    """A response was dropped on purpose (fault simulation)."""

    def __init__(self, request_id, dropped):
        super().__init__("response to %s lost" % request_id)
        self.request_id = request_id
        self.dropped = dropped


def parse_fields(tokens):
    fields = {}
    for tok in tokens:
        if "=" not in tok:
            raise BridgeError("malformed field %r" % tok)
        k, v = tok.split("=", 1)
        fields[k] = v
    return fields


def format_request(request_id, op, args):
    if not ID_RE.match(request_id):
        raise ValueError("bad request id %r" % request_id)
    if not OP_RE.match(op):
        raise ValueError("bad operation %r" % op)
    parts = ["REQ", request_id, op]
    for k, v in args.items():
        v = str(v)
        if not KEY_RE.match(k) or not VAL_RE.match(v):
            raise ValueError("bad argument %s=%r" % (k, v))
        parts.append("%s=%s" % (k, v))
    return " ".join(parts)


class Response:
    def __init__(self, request_id, state, fields, items, lines):
        self.id = request_id
        self.state = state
        self.fields = fields
        self.items = items
        self.lines = lines
        self.request_line = None

    @property
    def ok(self):
        return self.state == "SUCCEEDED"

    @property
    def replayed(self):
        return self.fields.get("replayed") == "1"

    def get(self, key, default=None):
        return self.fields.get(key, default)

    def to_json(self):
        return {"id": self.id, "state": self.state, "fields": self.fields,
                "items": self.items, "lines": self.lines, "replayed": self.replayed}


def parse_response(lines):
    """Parses the lines RES ... / ITEM ... / END of one response."""
    first = lines[0].split(" ")
    if len(first) < 3 or first[0] != "RES":
        raise BridgeError("expected RES, got %r" % lines[0])
    rid, state = first[1], first[2]
    fields = parse_fields(first[3:])
    items = []
    for line in lines[1:-1]:
        parts = line.split(" ")
        if parts[0] != "ITEM" or parts[1] != rid:
            raise BridgeError("unexpected line %r in response %s" % (line, rid))
        items.append(parse_fields(parts[2:]))
    if lines[-1] != "END " + rid:
        raise BridgeError("response %s not terminated: %r" % (rid, lines[-1]))
    return Response(rid, state, fields, items, lines)


class NciClient:
    """Line protocol over a connected stream socket."""

    def __init__(self, sock, log=None, timeout_s=30.0):
        self.sock = sock
        self.buf = b""
        self.timeout_s = timeout_s
        self.log = log  # callable(direction, line) or None
        self.hello = None
        self.counter = 0

    def next_id(self, prefix):
        self.counter += 1
        return "%s-%d" % (prefix, self.counter)

    def _readline(self, deadline):
        while b"\n" not in self.buf:
            left = deadline - time.monotonic()
            if left <= 0:
                raise BridgeError("timeout waiting for the guest")
            self.sock.settimeout(left)
            try:
                chunk = self.sock.recv(4096)
            except socket.timeout:
                raise BridgeError("timeout waiting for the guest")
            except OSError as e:
                raise BridgeError("link error: %s" % e)
            if not chunk:
                raise BridgeError("link closed by the guest side")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        text = line.decode("ascii", "replace").rstrip("\r")
        if self.log:
            self.log("guest", text)
        return text

    def wait_hello(self, timeout_s=90.0):
        deadline = time.monotonic() + timeout_s
        while True:
            line = self._readline(deadline)
            if line.startswith("HELLO "):
                self.hello = parse_fields(line.split(" ")[1:])
                return self.hello

    def send_line(self, line):
        if self.log:
            self.log("host", line)
        try:
            self.sock.sendall(line.encode("ascii") + b"\n")
        except OSError as e:
            raise BridgeError("link error: %s" % e)

    def read_response(self, request_id, timeout_s=None):
        deadline = time.monotonic() + (timeout_s or self.timeout_s)
        while True:
            line = self._readline(deadline)
            if line.startswith("RES " + request_id + " "):
                break
            if line.startswith("RES "):
                raise BridgeError("response to another request: %r" % line)
        lines = [line]
        while not lines[-1].startswith("END "):
            lines.append(self._readline(deadline))
        return parse_response(lines)

    def call(self, op, request_id=None, drop_response=False, timeout_s=None, **args):
        """Sends a request and returns its Response.  With drop_response the
        response is read and discarded, and ResponseLost is raised."""
        request_id = request_id or self.next_id("r")
        line = format_request(request_id, op, args)
        self.send_line(line)
        resp = self.read_response(request_id, timeout_s)
        resp.request_line = line
        if drop_response:
            raise ResponseLost(request_id, resp)
        return resp
