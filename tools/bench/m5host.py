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

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parents[0] / "store"))
import nxstore  # noqa: E402

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
    """

    def __init__(self, spec, run_dir):
        self.spec = spec or {}
        self.run_dir = Path(run_dir)
        records = self.spec.get("dns", {"provider.nanox.test": GUEST_HOST_ALIAS})
        self.dns = DnsServer(records)
        self.echo = EchoServer() if self.spec.get("echo", True) else None
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
                "qmp": self.qmp.log if self.qmp else []}

    def close(self):
        self.dns.close()
        if self.echo:
            self.echo.close()
        if self.qmp:
            self.qmp.close()
        if self.provider:
            self.provider.close()
