"""Host-side scripts of the M5 scenarios (docs/m5-net.md §10).

In M5 the second serial port is the operator console of the guest: the
host types NCI requests there as a human operator would, and never calls
the model itself.  Everything that reaches the network goes through the
guest's own stack (virtio-net -> QEMU user networking -> the host services
of tools/bench/m5host.py); the scripts check the guest's answers against
what those services saw independently.

  m5-net         criterion 1: link, ARP, ICMP, DNS, TCP, diagnostics,
                 link down and back up through QMP
  m5-tls         criterion 2: entropy/CSPRNG, TLS 1.3 with both cipher
                 suites and both key types against OpenSSL, certificate
                 verification failures classified as tls
  m5-agent       criterion 3: the guest's agent loop asks the test provider
                 over its own stack and executes the tool calls
  m5-agent-loss  the same under deterministic frame loss (m5-noretx: its
                 negative control)
  m5-faults      criterion 4: timeouts, stream breaks, HTTP errors, retry
                 and connection recovery (m5-noretry: negative control)
  m5-classes     the telemetry tells network, provider and action failures
                 apart (m5-flattel: negative control)
  m5-console     criterion 5: without the provider the operator console
                 keeps working, and the agent recovers when it is back
"""

import base64
import re
import socket
import time

import nci  # noqa: F401  (BridgeError is raised through the client)
from m4scripts import Caller, Checks, describe

HOST = "10.0.2.2"


def closed_port():
    """A local port nobody listens on (bound, then released)."""
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def run_net(ctx):
    c, call, m5 = Checks(), Caller(ctx["client"], "n"), ctx["carry"]["m5"]
    lossy = ctx["carry"].get("lossy", False)
    r = call("system.describe")
    c.expect(r.ok and "net.status" in r.get("ops", ""), "describe_ops",
             "net.* operations not offered: %s" % describe(r))
    r = call("net.status")
    c.expect(r.ok and r.get("link") == "up" and r.get("ip") == "10.0.2.15" and
             r.get("gw") == HOST and r.get("dns_port") == str(m5.dns.port), "net_status",
             describe(r))
    # ICMP echo to the gateway (answered by QEMU's user networking itself);
    # under deliberate loss one echo may be lost, so a few attempts
    for attempt in range(3 if lossy else 1):
        r = call("net.ping", target=HOST)
        if r.ok:
            break
    c.expect(r.ok and r.get("verify") == "ok", "ping", describe(r))
    r = call("net.resolve", name="provider.nanox.test")
    c.expect(r.ok and r.get("address") == HOST, "resolve", describe(r))
    c.expect("provider.nanox.test" in m5.dns.queries, "resolve_seen",
             "the DNS server saw no query (%s)" % m5.dns.queries)
    r = call("net.resolve", name="missing.nanox.test")
    c.expect(r.state == "FAILED" and r.get("detail") == "dns_notfound" and
             r.get("class") == "net", "resolve_missing", describe(r))
    r = call("net.probe", host=HOST, port=m5.echo.port, text="hello-nanox")
    c.expect(r.ok and r.get("verify") == "ok" and r.get("bytes") == "12", "probe", describe(r))
    c.expect("hello-nanox" in m5.echo.lines, "probe_seen",
             "the echo server did not receive the line (%s)" % m5.echo.lines)
    r = call("net.probe", host=HOST, port=closed_port(), text="nobody")
    c.expect(r.state == "FAILED" and r.get("detail") == "conn_refused" and
             r.get("class") == "net", "probe_refused", describe(r))
    r = call("rng.status")
    c.expect(r.ok and r.get("health") == "ok" and r.get("source") == "virtio-rng",
             "rng", describe(r))
    # link down (QEMU set_link through QMP), then up again
    m5.qmp.set_link(False)
    r = call("net.status")
    c.expect(r.ok and r.get("link") == "down", "link_down_status", describe(r))
    r = call("net.probe", host=HOST, port=m5.echo.port, text="while-down")
    c.expect(r.state == "FAILED" and r.get("detail") == "link_down" and
             r.get("class") == "net", "probe_link_down", describe(r))
    m5.qmp.set_link(True)
    r = call("net.probe", host=HOST, port=m5.echo.port, text="after-link-up")
    c.expect(r.ok and r.get("verify") == "ok", "probe_after_link_up", describe(r))
    r = call("net.status")
    c.expect(r.ok and r.get("link") == "up", "link_up_status", describe(r))
    if lossy:
        c.expect(r.ok and int(r.get("dev_rx_test_drops", "0")) +
                 int(r.get("dev_tx_test_drops", "0")) > 0, "loss_happened", describe(r))
        c.expect(r.ok and int(r.get("tcp_retransmits", "0")) > 0, "retransmitted", describe(r))
    return c.problems


def run_net_lossy(ctx):
    ctx["carry"]["lossy"] = True
    return run_net(ctx)


def run_tls(ctx):
    c, call, m5 = Checks(), Caller(ctx["client"], "t"), ctx["carry"]["m5"]
    tls = m5.tls
    r = call("rng.status")
    c.expect(r.ok and r.get("health") == "ok" and r.get("drbg") == "hmac-sha256", "rng",
             describe(r))
    name = "provider.nanox.test"
    for suites, suite in ((1, "TLS_AES_128_GCM_SHA256"), (2, "TLS_CHACHA20_POLY1305_SHA256")):
        r = call("tls.probe", host=name, port=tls["ec-leaf"].port, suites=suites)
        c.expect(r.ok and r.get("suite") == suite and r.get("sig") == "ecdsa_secp256r1_sha256" and
                 r.get("depth") == "2" and r.get("verify") == "ok", "tls_ec_%d" % suites,
                 describe(r))
    c.expect(tls["ec-leaf"].lines == ["hello", "hello"] and
             tls["ec-leaf"].ciphers == ["TLS_AES_128_GCM_SHA256", "TLS_CHACHA20_POLY1305_SHA256"],
             "tls_ec_server_view", "OpenSSL saw %s %s" % (tls["ec-leaf"].lines,
                                                          tls["ec-leaf"].ciphers))
    r = call("tls.probe", host=name, port=tls["rsa-chain"].port)
    c.expect(r.ok and r.get("sig") == "rsa_pss_rsae_sha256" and r.get("chain") == "2" and
             r.get("depth") == "3", "tls_rsa_chain", describe(r))
    r = call("tls.probe", host="api.nanox.test", port=tls["rsa-chain"].port)
    c.expect(r.state == "FAILED" and r.get("detail") == "dns_notfound", "tls_dns_first",
             describe(r))
    for prof, detail, openssl in (("expired", "cert_expired", "certificate expired"),
                                  ("wrong-name", "cert_name", "bad certificate"),
                                  ("untrusted", "cert_untrusted", "unknown ca")):
        r = call("tls.probe", host=name, port=tls[prof].port)
        c.expect(r.state == "FAILED" and r.get("detail") == detail and r.get("class") == "tls" and
                 r.get("code") == "TLS_ERROR", "tls_" + prof, describe(r))
        # the server thread records the failure after the guest has answered
        deadline = time.monotonic() + 5
        while (not any(openssl in e.lower() for e in tls[prof].errors) and
               time.monotonic() < deadline):
            time.sleep(0.05)
        c.expect(any(openssl in e.lower() for e in tls[prof].errors), "tls_%s_alert" % prof,
                 "OpenSSL did not see the alert %r: %s" % (openssl, tls[prof].errors))
        c.expect(not tls[prof].lines, "tls_%s_no_data" % prof,
                 "application data went over a refused connection")
    return c.problems


# ---- agent (criteria 3-5) ------------------------------------------------------------

WORKLOAD_Q = "Запусти нагрузку load, измерь её и останови."
DESCRIBE_Q = "describe the system"
ASK_TIMEOUT_S = 150


def ask_args(text):
    b = base64.urlsafe_b64encode(text.encode("utf-8")).decode("ascii").rstrip("=")
    parts = [b[i:i + 64] for i in range(0, len(b), 64)]
    if len(parts) > 3:
        raise ValueError("question too long for agent.ask")
    return {"q" + "abc"[i]: p for i, p in enumerate(parts)}


def answer_of(r):
    t = "".join(it.get("text", "") for it in sorted(r.items, key=lambda i: int(i.get("seq", 0))))
    return base64.urlsafe_b64decode(t + "=" * (-len(t) % 4)).decode("utf-8", "replace")


class Agent:
    """agent.ask requests with ids "<prefix><n>" and the guest's telemetry."""

    def __init__(self, ctx, prefix):
        self.ctx, self.prefix, self.n = ctx, prefix, 0
        self.m5 = ctx["carry"]["m5"]
        self.prov = self.m5.provider

    def ask(self, text, *faults):
        if faults:
            self.prov.push(*faults)
        self.n += 1
        rid = "%s%d" % (self.prefix, self.n)
        seen = len(self.prov.requests())
        r = self.ctx["client"].call("agent.ask", request_id=rid, timeout_s=ASK_TIMEOUT_S,
                                    **ask_args(text))
        r.provider_requests = self.prov.requests()[seen:]
        r.tel = self.tel(rid)
        return r

    def tel(self, rid):
        """Telemetry lines of request rid from the serial log: list of dicts."""
        text = open(self.ctx["carry"]["serial_path"], encoding="utf-8", errors="replace").read()
        out = []
        for line in text.splitlines():
            i = line.find(" tel ask=%s " % rid)
            if i >= 0:
                out.append(dict(t.split("=", 1) for t in line[i + 5:].split(" ") if "=" in t))
        return out


def model_tel(r):
    return [t for t in r.tel if t.get("phase") == "model"]


def ask_tel(r):
    t = [t for t in r.tel if t.get("phase") == "ask"]
    return t[-1] if t else {}


def check_key_secret(c, ctx):
    """The key must not appear anywhere in the guest's log."""
    m5 = ctx["carry"]["m5"]
    text = open(ctx["carry"]["serial_path"], encoding="utf-8", errors="replace").read()
    c.expect(m5.key and m5.key not in text and m5.key[14:] not in text, "key_not_logged",
             "the provider key appears in the serial log")


def run_agent(ctx):
    c, call, m5 = Checks(), Caller(ctx["client"], "g"), ctx["carry"]["m5"]
    prov = m5.provider
    lossy = ctx["carry"].get("lossy", False)
    ag = Agent(ctx, "ask")
    r = call("system.describe")
    c.expect(r.ok and "agent.ask" in r.get("ops", ""), "describe_ops", describe(r))
    r = call("provider.status")
    c.expect(r.ok and r.get("config") == "ok" and r.get("key") == "present" and
             r.get("key_len") == str(len(m5.key)) and r.get("host") == "provider.nanox.test" and
             r.get("model") == m5.provider_model, "provider_status", describe(r))
    c.expect(m5.key not in " ".join(r.lines), "status_no_key", "provider.status shows the key")
    before = call("task.list")
    r = ag.ask(WORKLOAD_Q)
    ans = answer_of(r) if r.items else ""
    c.expect(r.ok and r.get("steps") == "6" and r.get("actions") == "5" and
             r.get("actions_failed") == "0" and r.get("effects") == "task.spawn,task.terminate",
             "ask_workload", describe(r))
    c.expect("Нагрузка остановлена (reaped)" in ans, "answer",
             "answer %r" % ans)
    reqs = r.provider_requests
    c.expect(len(reqs) == 6 and all(q.get("auth") and q.get("status") == 200 for q in reqs),
             "provider_view", "provider saw %s" % reqs)
    c.expect([q.get("decision") for q in reqs] ==
             ["list_tasks", "spawn_task", "measure_task", "terminate_task", "list_tasks", "text"],
             "decisions", "policy decisions %s" % [q.get("decision") for q in reqs])
    c.expect(not prov.problems, "protocol", "provider found problems: %s" % prov.problems)
    if not lossy:
        c.expect(len({q["conn"] for q in reqs}) == 1, "keep_alive",
                 "requests over %d connections" % len({q["conn"] for q in reqs}))
    # the actions the model chose are ordinary executor records, checked independently
    for n in range(1, 6):
        st = call("action.status", request="ask1.%d" % n)
        c.expect(st.ok and st.get("known") == "yes" and st.get("state") == "SUCCEEDED",
                 "action_%d" % n, describe(st))
    after = call("task.list")
    names = lambda lst: sorted(i.get("name") for i in lst.items if i.get("state") != "dead")
    c.expect(after.ok and names(after) == names(before), "no_leftover",
             "tasks before %s after %s" % (names(before), names(after)))
    mt = model_tel(r)
    c.expect(len(mt) == 6 and all(t.get("result") == "ok" and t.get("http") == "200" for t in mt),
             "telemetry_model", "model attempts %s" % mt)
    at = [t for t in r.tel if t.get("phase") == "action"]
    c.expect(len(at) == 5 and all(t.get("result") == "SUCCEEDED" and t.get("class") == "ok"
                                  for t in at), "telemetry_actions", "actions %s" % at)
    c.expect(ask_tel(r).get("result") == "ok" and ask_tel(r).get("class") == "ok",
             "telemetry_ask", "ask %s" % ask_tel(r))
    t = call("telemetry.status")
    c.expect(t.ok and t.get("asks_ok") == "1" and t.get("net") == "0" and
             t.get("provider") == "0" and t.get("action") == "0", "telemetry_status", describe(t))
    check_key_secret(c, ctx)
    return c.problems


def run_agent_lossy(ctx):
    ctx["carry"]["lossy"] = True
    problems = run_agent(ctx)
    r = Caller(ctx["client"], "gl")("net.status")
    c = Checks()
    c.expect(r.ok and int(r.get("tcp_retransmits", "0")) > 0, "retransmitted", describe(r))
    return problems + c.problems


def attempts_of(r):
    """(result, class, http) of every model attempt of an ask."""
    return [(t.get("result"), t.get("class"), t.get("http")) for t in model_tel(r)]


def run_faults(ctx):
    """Criterion 4: every fault of the provider or of the connection is
    detected, classified, retried when a retry can help, and the next
    request recovers."""
    c, call = Checks(), Caller(ctx["client"], "f")
    ag = Agent(ctx, "fa")

    def recovered(code, faults, first):
        r = ag.ask(DESCRIBE_Q, *faults)
        at = attempts_of(r)
        c.expect(r.ok and at[:len(first)] == first and at[-1][0] == "ok" and
                 r.get("retries") == str(len(first)), code,
                 "%s; attempts %s, expected first %s" % (describe(r), at, first))
        return r

    # retryable faults: the request is repeated and succeeds
    r = recovered("retry_429", ["http:429:retry=1"], [("rate_limit", "provider", "429")])
    t = model_tel(r)
    c.expect(t and t[0].get("retry_in") == "1000" and t[0].get("error_type") == "rate_limit_error",
             "retry_after", "Retry-After not honoured: %s" % t[:1])
    recovered("retry_5xx", ["http:529", "http:500"],
              [("overloaded", "provider", "529"), ("server_error", "provider", "500")])
    recovered("stream_error", ["sse_error:overloaded_error"], [("overloaded", "provider", "200")])
    recovered("stream_cut", ["cut"], [("peer_closed", "net", "200")])
    recovered("stream_truncated", ["early_close"], [("truncated", "provider", "200")])
    recovered("timeout", ["delay:6"], [("provider_timeout", "provider", "0")])
    # a reset of a kept-alive connection before any answer is taken for a
    # connection the provider dropped while idle: one new connection, not
    # counted as an attempt; a reset on the new connection is a failure
    r = recovered("reset", ["reset", "reset"], [("conn_reset", "net", "0")])
    c.expect(r.get("reconnects") == "1" and model_tel(r)[0].get("reconnect") == "stale",
             "reset_stale_first", "%s; %s" % (describe(r), model_tel(r)[:1]))
    # the provider closes the connection after an answer: the next request
    # goes over a new connection, nothing fails, nothing is retried
    r = ag.ask(DESCRIBE_Q, "idle_close")
    conns = [q["conn"] for q in r.provider_requests]
    c.expect(r.ok and r.get("retries") == "0" and r.get("attempts") == "2" and
             len(set(conns)) == 2, "reconnect", "%s; connections %s" % (describe(r), conns))
    # retries exhausted
    r = ag.ask(DESCRIBE_Q, *["http:429:retry=1"] * 3)
    c.expect(r.state == "FAILED" and r.get("code") == "PROVIDER_ERROR" and
             r.get("detail") == "rate_limit" and r.get("attempts") == "3" and
             r.get("http") == "429", "retries_exhausted", describe(r))
    # not retryable: one attempt
    for code, fault, detail in (("bad_request", "http:400", "bad_request"),
                                ("auth", "http:401", "auth"),
                                ("malformed_json", "malformed_json", "malformed"),
                                ("malformed_http", "malformed_http", "malformed_http")):
        r = ag.ask(DESCRIBE_Q, fault)
        c.expect(r.state == "FAILED" and r.get("code") == "PROVIDER_ERROR" and
                 r.get("detail") == detail and r.get("class") == "provider" and
                 r.get("attempts") == "1", code, describe(r))
    # and after all of it the provider path works as before
    r = ag.ask(WORKLOAD_Q)
    c.expect(r.ok and r.get("actions") == "5" and r.get("attempts") == "6", "recovered_workload",
             describe(r))
    c.expect(not ag.prov.pending_faults(), "faults_consumed",
             "faults left unused: %s" % ag.prov.pending_faults())
    c.expect(not ag.prov.problems, "protocol", "provider found problems: %s" % ag.prov.problems)
    t = call("telemetry.status")
    c.expect(t.ok and int(t.get("retries", "0")) >= 10 and int(t.get("reconnects", "0")) >= 1,
             "telemetry_status", describe(t))
    check_key_secret(c, ctx)
    return c.problems


def run_classes(ctx):
    """The done-definition of M5: telemetry tells a network failure, a
    provider error and a failed action apart, in the response and in the
    serial log."""
    c, call, m5 = Checks(), Caller(ctx["client"], "c"), ctx["carry"]["m5"]
    ag = Agent(ctx, "cl")

    def classified(code, r, want_code, detail, cls):
        at = ask_tel(r)
        ok = (r.state == "FAILED" and r.get("code") == want_code and r.get("detail") == detail and
              r.get("class") == cls and at.get("class") == cls)
        c.expect(ok, code, "%s; telemetry %s" % (describe(r), at))
        return r

    # network: nobody listens (refused), then the link is down
    ag.prov.pause()
    r = classified("net_refused", ag.ask(DESCRIBE_Q), "NET_ERROR", "conn_refused", "net")
    c.expect(not r.provider_requests, "net_refused_unseen", "the provider saw a request")
    ag.prov.resume()
    m5.qmp.set_link(False)
    classified("net_link_down", ag.ask(DESCRIBE_Q), "NET_ERROR", "link_down", "net")
    m5.qmp.set_link(True)
    # provider: an error response
    r = classified("provider_http", ag.ask(DESCRIBE_Q, "http:400"), "PROVIDER_ERROR",
                   "bad_request", "provider")
    c.expect(r.get("http") == "400", "provider_http_status", describe(r))
    # provider: a tool call the executor refuses (nothing is executed)
    r = classified("provider_bad_tool", ag.ask("call a bogus tool"), "PROVIDER_ERROR",
                   "bad_tool", "provider")
    c.expect(r.get("actions_failed") == "0" and r.get("effects") == "none", "bad_tool_no_effect",
             describe(r))
    # action: the model's call is valid, the executor fails it
    r = classified("action_failed", ag.ask("spawn the nosuch program"), "ACTION_ERROR",
                   "NOT_FOUND", "action")
    act = [t for t in r.tel if t.get("phase") == "action"]
    c.expect(len(act) == 1 and act[0].get("result") == "FAILED" and act[0].get("class") == "action",
             "action_telemetry", "action lines %s" % act)
    st = call("action.status", request=r.get("action", "-"))
    c.expect(st.ok and st.get("state") == "FAILED", "action_record", describe(st))
    # success for comparison
    r = ag.ask(DESCRIBE_Q)
    c.expect(r.ok and ask_tel(r).get("class") == "ok", "success", describe(r))
    t = call("telemetry.status")
    c.expect(t.ok and int(t.get("net", "0")) >= 2 and int(t.get("provider", "0")) >= 2 and
             t.get("action") == "1" and t.get("unclassified") == "0", "telemetry_status",
             describe(t))
    check_key_secret(c, ctx)
    return c.problems


def run_console(ctx):
    """Criterion 5: with the provider unreachable the agent fails cleanly
    and the operator console (COM2) keeps working: observation, actions,
    diagnostics; when the provider is back the agent works again."""
    c, call, m5 = Checks(), Caller(ctx["client"], "o"), ctx["carry"]["m5"]
    ag = Agent(ctx, "co")
    ag.prov.pause()
    r = ag.ask(WORKLOAD_Q)
    c.expect(r.state == "FAILED" and r.get("class") == "net" and r.get("actions") == "0",
             "ask_without_provider", describe(r))
    m5.qmp.set_link(False)
    r = ag.ask(WORKLOAD_Q)
    c.expect(r.state == "FAILED" and r.get("detail") == "link_down", "ask_link_down", describe(r))
    # the console, while the model is out of reach
    for op in ("system.describe", "task.list", "memory.stats", "net.status", "provider.status",
               "telemetry.status", "store.status", "rng.status"):
        r = call(op)
        c.expect(r.ok, "console_" + op, describe(r))
    sp = call("task.spawn", program="load")
    c.expect(sp.ok and sp.get("verify") == "ok", "console_spawn", describe(sp))
    tm = call("task.terminate", target=sp.get("ref", "-"))
    c.expect(tm.ok and tm.get("verify") == "ok", "console_terminate", describe(tm))
    ns = call("net.status")
    c.expect(ns.ok and ns.get("link") == "down", "console_sees_link_down", describe(ns))
    # the model comes back
    m5.qmp.set_link(True)
    ag.prov.resume()
    r = ag.ask(WORKLOAD_Q)
    c.expect(r.ok and r.get("actions") == "5", "ask_after_recovery", describe(r))
    check_key_secret(c, ctx)
    return c.problems


def run_nokey(ctx):
    """No key in the store: the agent fails as a local error before any
    connection, the console keeps working."""
    c, call = Checks(), Caller(ctx["client"], "k")
    ag = Agent(ctx, "nk")
    r = call("provider.status")
    c.expect(r.ok and r.get("config") == "no_key" and r.get("key") == "missing", "status",
             describe(r))
    r = ag.ask(DESCRIBE_Q)
    c.expect(r.state == "FAILED" and r.get("code") == "LOCAL_ERROR" and
             r.get("detail") == "no_key" and r.get("class") == "local", "ask", describe(r))
    c.expect(not ag.prov.requests(), "no_connection", "the provider was contacted")
    r = call("task.list")
    c.expect(r.ok, "console", describe(r))
    return c.problems


SCRIPTS = {"m5-net": run_net, "m5-net-loss": run_net_lossy, "m5-tls": run_tls,
           "m5-agent": run_agent, "m5-agent-loss": run_agent_lossy, "m5-faults": run_faults,
           "m5-classes": run_classes, "m5-console": run_console, "m5-nokey": run_nokey}
