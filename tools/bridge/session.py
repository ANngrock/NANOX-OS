"""One bridge session: wait for the guest's HELLO, run a scenario script,
close the session reporting the host-side checks (docs/m3-core.md §7).

Also usable from the command line against a running guest:
  session.py --connect PATH | --listen PATH  --script agent [--adapter mock]
"""

import argparse
import json
import os
import re
import socket
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import nci  # noqa: E402
import scripts  # noqa: E402
import bridgetrace as tracemod  # noqa: E402


def reason_code(problem):
    code = problem.split(":", 1)[0]
    code = re.sub(r"[^A-Za-z0-9._-]", "_", code)[:60]
    return code or "unspecified"


def run_session(sock, script, adapter=None, request=None, trace_path=None, wire_path=None,
                hello_timeout_s=120.0):
    """Returns a dict: ok, problems, hello, close, agent, trace_path."""
    wire = open(wire_path, "w") if wire_path else None
    t0 = time.monotonic()

    def log(direction, line):
        if wire:
            wire.write("%8.3f %s %s\n" % (time.monotonic() - t0,
                                          "->" if direction == "host" else "<-", line))
            wire.flush()

    trace = tracemod.Trace(trace_path)
    client = nci.NciClient(sock, log=log)
    out = {"script": script, "adapter": adapter, "ok": False, "problems": [], "hello": None,
           "close": None, "agent": None, "trace": trace_path}
    try:
        out["hello"] = client.wait_hello(hello_timeout_s)
        ctx = {"client": client, "trace": trace, "hello": out["hello"], "request": request}
        if adapter:
            ctx["adapter"] = adapter
        problems = scripts.SCRIPTS[script](ctx)
        out["agent"] = ctx.get("agent_result")
        args = {"host_checks": "fail" if problems else "ok"}
        if problems:
            args["reason"] = reason_code(problems[0])
        close = client.call("session.close", request_id="close", **args)
        out["close"] = close.fields
        out["problems"] = problems
        out["ok"] = not problems
    except (nci.BridgeError, KeyError, ValueError) as e:
        out["problems"].append("bridge_error: %s: %s" % (type(e).__name__, e))
    finally:
        if wire:
            wire.close()
    out["actions"] = sum(1 for r in trace.records if r["kind"] == "action")
    out["duration_s"] = round(time.monotonic() - t0, 3)
    return out


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--connect", help="unix socket the guest channel listens on")
    g.add_argument("--listen", help="unix socket to listen on for the guest channel")
    ap.add_argument("--script", choices=sorted(scripts.SCRIPTS), default="agent")
    ap.add_argument("--adapter", default=None)
    ap.add_argument("--request", default=None)
    ap.add_argument("--trace", default="bridge-trace.jsonl")
    args = ap.parse_args(argv)
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    if args.connect:
        s.connect(args.connect)
    else:
        s.bind(args.listen)
        s.listen(1)
        s, _ = s.accept()
    res = run_session(s, args.script, args.adapter, args.request, args.trace)
    print(json.dumps(res, indent=2, ensure_ascii=False))
    return 0 if res["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
