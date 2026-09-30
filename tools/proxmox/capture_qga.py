#!/usr/bin/env python3
"""Captures request/response pairs from the reference `qemu-ga` (the one in
the pinned Nix QEMU 9.2.4 package) into crates/qga/tests/fixtures, so the
guest-agent implementation in crates/qga is compared with the real thing.

    python3 tools/proxmox/capture_qga.py

Safety: qemu-ga runs as the current user on a unix socket with every command
that could act on the machine blocked (shutdown, fsfreeze, suspend, exec,
file access, user/password, ssh keys, memory/cpu control). Nothing is sent
that would need them; the blocked ones are only asked to capture the
"disabled" answer. Privacy: values of commands that return host identifiers
(host name, OS details, interface names/MACs/addresses) are not stored,
only their structure ("shape").

Records: request bytes (hex), raw response bytes (hex), and either the exact
response text (`exact`) or its `shape` (keys and value types).
"""

import glob
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "crates/qga/tests/fixtures/qemu-ga-9.2.4.json"

BLOCKED = [
    "guest-shutdown", "guest-suspend-disk", "guest-suspend-ram",
    "guest-suspend-hybrid", "guest-fsfreeze-freeze", "guest-fsfreeze-freeze-list",
    "guest-fsfreeze-thaw", "guest-exec", "guest-exec-status", "guest-file-open",
    "guest-file-close", "guest-file-read", "guest-file-write", "guest-file-seek",
    "guest-file-flush", "guest-set-user-password", "guest-ssh-add-authorized-keys",
    "guest-ssh-remove-authorized-keys", "guest-set-vcpus", "guest-set-memory-blocks",
    "guest-set-time", "guest-fstrim",
]

# name, raw request, mode: exact | shape
REQUESTS = [
    # The agent, not the client, puts the 0xFF sentinel in front of this reply.
    ("sync-delimited", b'{"execute":"guest-sync-delimited","arguments":{"id":314159265}}\n', "exact"),
    ("sync", b'{"execute":"guest-sync","arguments":{"id":123456789}}\n', "exact"),
    ("sync-negative-id", b'{"execute":"guest-sync","arguments":{"id":-5}}\n', "exact"),
    ("ping", b'{"execute":"guest-ping"}\n', "exact"),
    ("ping-with-id", b'{"execute":"guest-ping","id":"req-7"}\n', "exact"),
    ("info", b'{"execute":"guest-info"}\n', "shape"),
    ("get-time", b'{"execute":"guest-get-time"}\n', "shape"),
    ("get-host-name", b'{"execute":"guest-get-host-name"}\n', "shape"),
    ("get-osinfo", b'{"execute":"guest-get-osinfo"}\n', "shape"),
    ("network-get-interfaces", b'{"execute":"guest-network-get-interfaces"}\n', "shape"),
    ("fsfreeze-status", b'{"execute":"guest-fsfreeze-status"}\n', "exact"),
    ("unknown-command", b'{"execute":"guest-no-such-command"}\n', "exact"),
    ("missing-execute", b'{"foo":1}\n', "exact"),
    ("sync-missing-arguments", b'{"execute":"guest-sync"}\n', "exact"),
    ("sync-id-string", b'{"execute":"guest-sync","arguments":{"id":"x"}}\n', "exact"),
    ("ping-unknown-argument", b'{"execute":"guest-ping","arguments":{"x":1}}\n', "exact"),
    ("bad-json", b'{"execute" "guest-ping"}\n', "exact"),
    ("empty-object", b'{}\n', "exact"),
    ("arguments-only", b'{"arguments":{}}\n', "exact"),
    ("arguments-not-object", b'{"execute":"guest-ping","arguments":5}\n', "exact"),
    ("sync-id-float", b'{"execute":"guest-sync","arguments":{"id":1.5}}\n', "exact"),
    ("sync-id-max", b'{"execute":"guest-sync","arguments":{"id":9223372036854775807}}\n', "exact"),
    ("sync-id-huge", b'{"execute":"guest-sync","arguments":{"id":9223372036854775808}}\n', "exact"),
    ("sync-id-null", b'{"execute":"guest-sync","arguments":{"id":null}}\n', "exact"),
    ("array-top-level", b'[1,2]\n', "exact"),
    ("missing-value", b'{"execute":}\n', "exact"),
    ("trailing-comma", b'{"execute":"guest-ping",}\n', "exact"),
    ("missing-comma", b'{"execute":"guest-ping" "id":1}\n', "exact"),
    ("id-object", b'{"execute":"guest-ping","id":{"a":[1,2]}}\n', "exact"),
    ("escaped-execute", b'{"execute":"guest-\\u0070ing"}\n', "exact"),
    ("duplicate-execute", b'{"execute":"guest-ping","execute":"guest-sync"}\n', "exact"),
    ("unexpected-member-with-id", b'{"id":9,"foo":1}\n', "exact"),
    ("unexpected-member-id-last", b'{"foo":1,"id":9}\n', "exact"),
    ("unknown-command-with-id", b'{"execute":"guest-nope","id":"a"}\n', "exact"),
    ("sync-missing-with-id", b'{"execute":"guest-sync","arguments":{},"id":3}\n', "exact"),
    ("sync-unexpected-extra", b'{"execute":"guest-sync","arguments":{"id":1,"x":2}}\n', "exact"),
    ("sync-only-unexpected", b'{"execute":"guest-sync","arguments":{"x":2}}\n', "exact"),
    ("disabled-with-id", b'{"execute":"guest-shutdown","id":4}\n', "exact"),
    ("arguments-before-execute", b'{"arguments":{},"execute":"guest-ping"}\n', "exact"),
    ("string-escapes-in-id", b'{"execute":"guest-ping","id":"a\\"b\\\\c\\n\\u00e9"}\n', "exact"),
    ("execute-not-string", b'{"execute":5}\n', "exact"),
    ("shutdown-disabled", b'{"execute":"guest-shutdown","arguments":{"mode":"powerdown"}}\n', "exact"),
    ("fsfreeze-freeze-disabled", b'{"execute":"guest-fsfreeze-freeze"}\n', "exact"),
    ("exec-disabled", b'{"execute":"guest-exec","arguments":{"path":"/bin/true"}}\n', "exact"),
]


def shape(v):
    if isinstance(v, dict):
        return {k: shape(x) for k, x in v.items()}
    if isinstance(v, list):
        return [shape(v[0])] if v else []
    if isinstance(v, bool):
        return "bool"
    if isinstance(v, int):
        return "int"
    if isinstance(v, float):
        return "float"
    if v is None:
        return "null"
    return "string"


def find_qemu_ga():
    hits = sorted(glob.glob("/nix/store/*-qemu-9.2.4-ga/bin/qemu-ga"))
    if not hits:
        sys.exit("qemu-ga 9.2.4 not found in /nix/store")
    return hits[0]


def drain(s):
    """Discards anything the agent sent that nobody asked for."""
    s.settimeout(0.3)
    try:
        while s.recv(65536):
            pass
    except (socket.timeout, BlockingIOError):
        pass


def read_response(s, delimited=False):
    """One response: an optional 0xFF, then a JSON object ending in newline."""
    s.settimeout(5)
    data = b""
    while not data.endswith(b"\n"):
        chunk = s.recv(65536)
        if not chunk:
            break
        data += chunk
    return data


# Anything that looks like a MAC, IPv4 or IPv6 address must never be stored
# verbatim; only structure of such replies is kept.
IDENT = re.compile(
    rb"([0-9a-f]{2}:){5}[0-9a-f]{2}|\b\d{1,3}(\.\d{1,3}){3}\b|([0-9a-f]{1,4}:){3,}[0-9a-f]{0,4}",
    re.I,
)


def main():
    ga = find_qemu_ga()
    tmp = tempfile.mkdtemp(prefix="qga-capture-")
    sock_path = os.path.join(tmp, "qga.sock")
    proc = subprocess.Popen(
        [ga, "-m", "unix-listen", "-p", sock_path, "-t", tmp,
         "-l", os.path.join(tmp, "qga.log"), "-b", ",".join(BLOCKED)],
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
    )
    try:
        for _ in range(50):
            if os.path.exists(sock_path):
                break
            time.sleep(0.1)
        else:
            sys.exit("qemu-ga did not create its socket: " + proc.stderr.read().decode())
        def connect():
            c = socket.socket(socket.AF_UNIX)
            c.connect(sock_path)
            return c

        s = connect()
        version = subprocess.run([ga, "--version"], capture_output=True, text=True).stdout.strip()
        records = []
        for name, req, mode in REQUESTS:
            drain(s)
            s.sendall(req)
            try:
                resp = read_response(s)
            except socket.timeout:
                # An incomplete message: the agent waits for more. Record it
                # and start a fresh connection (a fresh parser).
                records.append({"name": name, "request_hex": req.hex(), "mode": "silent"})
                print(f"{name:26} no reply")
                s.close()
                time.sleep(0.3)
                s = connect()
                continue
            body = resp.lstrip(b"\xff").decode("utf-8", "replace").strip()
            rec = {"name": name, "request_hex": req.hex(), "mode": mode}
            try:
                parsed = json.loads(body)
            except json.JSONDecodeError:
                parsed = None
            if mode == "exact":
                if IDENT.search(resp):
                    sys.exit(f"{name}: exact reply looks like it holds an address; refusing to store it")
                rec["response_hex"] = resp.hex()
                rec["exact"] = body
            else:
                rec["shape"] = shape(parsed)
                rec["response_len"] = len(resp)
            records.append(rec)
            print(f"{name:26} {len(resp):5} bytes")
        # A second request after garbage: does the agent recover?
        drain(s)
        s.sendall(b'{"execute":"guest-ping"}\n')
        recover = read_response(s, False)
        OUT.parent.mkdir(parents=True, exist_ok=True)
        OUT.write_text(json.dumps(
            {"agent": version, "blocked": BLOCKED, "records": records,
             "recovers_after_bad_json": recover.decode().strip()}, indent=1) + "\n")
        print("wrote", OUT)
    finally:
        proc.terminate()
        proc.wait(timeout=5)
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
