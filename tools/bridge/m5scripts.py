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
"""

import socket

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
        c.expect(any(openssl in e.lower() for e in tls[prof].errors), "tls_%s_alert" % prof,
                 "OpenSSL did not see the alert %r: %s" % (openssl, tls[prof].errors))
        c.expect(not tls[prof].lines, "tls_%s_no_data" % prof,
                 "application data went over a refused connection")
    return c.problems


SCRIPTS = {"m5-net": run_net, "m5-net-loss": run_net_lossy, "m5-tls": run_tls}
