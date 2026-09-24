"""Host-side services of the M5 scenarios (docs/m5-net.md §10).

The guest reaches the host through QEMU user networking (slirp): the
host's loopback address is 10.0.2.2 inside the guest, so every service
here listens on 127.0.0.1 with an ephemeral port that the harness writes
into the guest's configuration (the data disk is provisioned before the
boot, tools/store/nxstore.py).  Only the Python standard library.

  DnsServer      UDP; answers A queries from a table, NXDOMAIN otherwise,
                 or nothing at all ("silent")
  EchoServer     TCP; echoes lines (net.probe)
  Qmp            QEMU machine protocol client: set_link (link down/up)
  Services       the set of services of one run and the provisioning of
                 the data disk
"""

import json
import os
import socket
import struct
import sys
import threading
import time
from pathlib import Path

import ssl

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
sys.path.insert(0, str(HERE.parents[0] / "store"))
sys.path.insert(0, str(HERE.parents[0] / "net"))
import nxstore  # noqa: E402
import pki  # noqa: E402

PKI_DIR = REPO / "out" / "m5-pki"


def pki_dir(profile):
    """Certificates and key of a test-PKI profile (tools/net/pki.py), cached."""
    return pki.write(profile, PKI_DIR / profile)


def server_context(profile):
    """OpenSSL (Python ssl) server context: TLS 1.3 only, the profile's
    certificate chain and key."""
    d = pki_dir(profile)
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.minimum_version = ssl.TLSVersion.TLSv1_3
    ctx.load_cert_chain(str(d / "chain.pem"), str(d / "key.pem"))
    return ctx

GUEST_HOST_ALIAS = "10.0.2.2"


class DnsServer:
    def __init__(self, records=None):
        self.records = dict(records or {})
        self.silent = False
        self.queries = []
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", 0))
        self.port = self.sock.getsockname()[1]
        self.stop = False
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    @staticmethod
    def parse_name(msg, off):
        labels = []
        while off < len(msg) and msg[off]:
            n = msg[off]
            labels.append(msg[off + 1:off + 1 + n].decode("ascii", "replace"))
            off += 1 + n
        return ".".join(labels), off + 1

    def answer(self, q):
        if len(q) < 17:
            return None
        qid = q[:2]
        name, end = self.parse_name(q, 12)
        question = q[12:end + 4]
        self.queries.append(name)
        if self.silent:
            return None
        ip = self.records.get(name.lower().rstrip("."))
        flags = b"\x81\x80" if ip else b"\x81\x83"
        head = qid + flags + struct.pack(">HHHH", 1, 1 if ip else 0, 0, 0)
        body = question
        if ip:
            body += b"\xc0\x0c" + struct.pack(">HHIH", 1, 1, 60, 4) + socket.inet_aton(ip)
        return head + body

    def _run(self):
        self.sock.settimeout(0.2)
        while not self.stop:
            try:
                q, addr = self.sock.recvfrom(1500)
            except socket.timeout:
                continue
            except OSError:
                return
            r = self.answer(q)
            if r:
                self.sock.sendto(r, addr)

    def close(self):
        self.stop = True
        self.sock.close()


class EchoServer:
    def __init__(self):
        self.lines = []
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(8)
        self.port = self.sock.getsockname()[1]
        self.stop = False
        threading.Thread(target=self._run, daemon=True).start()

    def _serve(self, conn):
        buf = b""
        conn.settimeout(20)
        try:
            while True:
                chunk = conn.recv(4096)
                if not chunk:
                    return
                buf += chunk
                while b"\n" in buf:
                    line, buf = buf.split(b"\n", 1)
                    self.lines.append(line.decode("ascii", "replace"))
                    conn.sendall(line + b"\n")
        except OSError:
            return
        finally:
            conn.close()

    def _run(self):
        self.sock.settimeout(0.2)
        while not self.stop:
            try:
                conn, _ = self.sock.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            threading.Thread(target=self._serve, args=(conn,), daemon=True).start()

    def close(self):
        self.stop = True
        self.sock.close()


class TlsEchoServer:
    """TLS 1.3 (OpenSSL) with a test-PKI profile; echoes lines, answers
    close_notify.  Handshake failures are recorded (OpenSSL's view)."""

    def __init__(self, profile):
        self.profile = profile
        self.ctx = server_context(profile)
        self.errors, self.lines, self.ciphers = [], [], []
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(8)
        self.port = self.sock.getsockname()[1]
        self.stop = False
        threading.Thread(target=self._run, daemon=True).start()

    def _serve(self, raw):
        raw.settimeout(30)
        try:
            s = self.ctx.wrap_socket(raw, server_side=True)
            self.ciphers.append(s.cipher()[0])
            buf = b""
            while b"\n" not in buf:
                chunk = s.recv(4096)
                if not chunk:
                    break
                buf += chunk
            line = buf.split(b"\n", 1)[0]
            self.lines.append(line.decode("ascii", "replace"))
            s.sendall(line + b"\n")
            try:
                s = s.unwrap()
            except (ssl.SSLError, OSError):
                pass
            s.close()
        except (ssl.SSLError, OSError) as e:
            self.errors.append(str(e))
        finally:
            raw.close()

    def _run(self):
        self.sock.settimeout(0.2)
        while not self.stop:
            try:
                conn, _ = self.sock.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            threading.Thread(target=self._serve, args=(conn,), daemon=True).start()

    def close(self):
        self.stop = True
        self.sock.close()


class Qmp:
    """Minimal QMP client: the monitor socket QEMU listens on (server=on,
    wait=off); the harness connects when a script needs it."""

    def __init__(self, path):
        self.path = path
        self.sock = None
        self.buf = b""
        self.log = []

    def _connect(self):
        if self.sock:
            return
        deadline = time.monotonic() + 10
        while True:
            try:
                s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                s.connect(self.path)
                break
            except OSError:
                s.close()
                if time.monotonic() > deadline:
                    raise
                time.sleep(0.1)
        self.sock = s
        self.sock.settimeout(10)
        self._read()  # greeting
        self.command("qmp_capabilities")

    def _read(self):
        while b"\n" not in self.buf:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise OSError("QMP closed")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return json.loads(line)

    def command(self, cmd, **args):
        self._connect()
        msg = {"execute": cmd}
        if args:
            msg["arguments"] = args
        self.sock.sendall(json.dumps(msg).encode() + b"\n")
        while True:
            r = self._read()
            if "return" in r or "error" in r:
                self.log.append({"cmd": cmd, "args": args, "reply": r})
                if "error" in r:
                    raise OSError("QMP %s: %s" % (cmd, r["error"]))
                return r["return"]

    def set_link(self, up):
        return self.command("set_link", name="nxnet", up=bool(up))

    def close(self):
        if self.sock:
            self.sock.close()
            self.sock = None


class Services:
    """Host services of one M5 run.  spec (scenario "m5" key):

      dns        {"name": "a.b.c.d"} records of the DNS server (default:
                 provider.nanox.test -> 10.0.2.2)
      config     extra cfg/<key> values for the guest
      echo       true: start the TCP echo server
      tls        test-PKI profiles to serve with TLS echo servers
      anchors    profiles whose root certificates are provisioned as
                 trust anchors (tls/anchor0, ...)
    """

    def __init__(self, spec, run_dir):
        self.spec = spec or {}
        self.run_dir = Path(run_dir)
        records = self.spec.get("dns", {"provider.nanox.test": GUEST_HOST_ALIAS})
        self.dns = DnsServer(records)
        self.echo = EchoServer() if self.spec.get("echo", True) else None
        self.tls = {p: TlsEchoServer(p) for p in self.spec.get("tls", [])}
        self.qmp_dir = None
        self.qmp_path = None
        self.qmp = None
        self.provider = None  # set up by the provider module (later steps)

    def qmp_socket(self, tmpdir):
        self.qmp_path = os.path.join(tmpdir, "qmp.sock")
        self.qmp = Qmp(self.qmp_path)
        return self.qmp_path

    def config(self):
        cfg = {"net.dns": GUEST_HOST_ALIAS, "net.dns_port": str(self.dns.port)}
        if self.echo:
            cfg["echo.port"] = str(self.echo.port)
        cfg.update(self.spec.get("config", {}))
        return cfg

    def objects(self):
        objs = [("cfg/" + k, nxstore.KIND_CONFIG, str(v).encode("ascii"))
                for k, v in sorted(self.config().items())]
        for i, prof in enumerate(self.spec.get("anchors", [])):
            objs.append(("tls/anchor%d" % i, nxstore.KIND_ANCHOR,
                         (pki_dir(prof) / "anchor.der").read_bytes()))
        return objs

    def provision(self, path):
        """Writes the provisioned data disk (generation 1 with the objects)."""
        data = nxstore.provisioned_image(self.objects())
        Path(path).write_bytes(data)
        return data

    def record(self):
        return {"dns_port": self.dns.port, "dns_queries": list(self.dns.queries),
                "echo_port": self.echo.port if self.echo else None,
                "echo_lines": list(self.echo.lines) if self.echo else [],
                "config": self.config(),
                "tls": {p: {"port": t.port, "lines": list(t.lines), "ciphers": list(t.ciphers),
                            "errors": list(t.errors)} for p, t in self.tls.items()},
                "qmp": self.qmp.log if self.qmp else []}

    def close(self):
        self.dns.close()
        if self.echo:
            self.echo.close()
        for t in self.tls.values():
            t.close()
        if self.qmp:
            self.qmp.close()
        if self.provider:
            self.provider.close()
