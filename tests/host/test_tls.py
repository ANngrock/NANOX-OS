"""Interoperability of the M5 TLS 1.3 client (lib/tls, lib/crypto, run as
out/host/tlstool) with an independent implementation: OpenSSL behind
Python's ssl module, using the test PKI of tools/net/pki.py.  Also checks
the test PKI itself with OpenSSL as the verifier, so that a certificate
the guest accepts or refuses is judged the same way by OpenSSL."""

import os
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "tools" / "net"))
import pki  # noqa: E402

TLSTOOL = REPO / "out" / "host" / "tlstool"
NOW = 1767225600  # 2026-01-01T00:00:00Z, the bench's RTC
PKI_DIR = REPO / "out" / "m5-pki"


def profile_dir(profile):
    return pki.write(profile, PKI_DIR / profile)


class Server:
    """One TLS 1.3 connection served by OpenSSL: reads a message, answers
    "echo:<message>", then closes with or without close_notify."""

    def __init__(self, profile, chain=None, close_notify=True):
        d = profile_dir(profile)
        self.ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        self.ctx.minimum_version = ssl.TLSVersion.TLSv1_3
        self.tmp = None
        chain_file = d / "chain.pem"
        if chain is not None:  # a different certificate list than the profile's
            self.tmp = tempfile.NamedTemporaryFile("w", suffix=".pem", delete=False)
            self.tmp.write(chain)
            self.tmp.close()
            chain_file = self.tmp.name
        self.ctx.load_cert_chain(str(chain_file), str(d / "key.pem"))
        self.sock = socket.socket()
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(1)
        self.port = self.sock.getsockname()[1]
        self.result = {}
        self.close_notify = close_notify
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    def _run(self):
        c, _ = self.sock.accept()
        c.settimeout(20)
        try:
            s = self.ctx.wrap_socket(c, server_side=True)
            self.result["cipher"] = s.cipher()[0]
            m = s.recv(200)
            s.sendall(b"echo:" + m)
            if self.close_notify:
                s = s.unwrap()  # sends close_notify, returns the plain socket
            s.close()
        except (ssl.SSLError, OSError) as e:
            self.result["error"] = str(e)
        finally:
            c.close()

    def finish(self):
        self.thread.join(20)
        self.sock.close()
        if self.tmp:
            os.unlink(self.tmp.name)
        return self.result


def run_tool(port, profile, name="provider.nanox.test", suites=3, now=NOW, anchor=None):
    anchor = anchor or str(profile_dir(profile) / "anchor.der")
    out = subprocess.run([str(TLSTOOL), "127.0.0.1", str(port), name, anchor, str(now),
                          str(suites), "hello"], stdout=subprocess.PIPE, timeout=60)
    line = out.stdout.decode().strip()
    fields = dict(tok.split("=", 1) for tok in line.split()[2:] if "=" in tok)
    return line, out.returncode, fields


@unittest.skipUnless(TLSTOOL.exists(), "out/host/tlstool not built")
class TlsInteropTest(unittest.TestCase):
    def handshake(self, profile, **kw):
        srv = Server(profile, chain=kw.pop("chain", None), close_notify=kw.pop("close_notify", True))
        line, code, fields = run_tool(srv.port, profile, **kw)
        return line, code, fields, srv.finish()

    def test_ec_leaf_aes(self):
        line, code, f, srv = self.handshake("ec-leaf", suites=1)
        self.assertEqual(code, 0, line)
        self.assertEqual(f["suite"], "TLS_AES_128_GCM_SHA256")
        self.assertEqual(f["sig"], "ecdsa_secp256r1_sha256")
        self.assertEqual(f["echo"], "echo:hello")
        self.assertEqual(srv["cipher"], "TLS_AES_128_GCM_SHA256")

    def test_ec_leaf_chacha(self):
        line, code, f, srv = self.handshake("ec-leaf", suites=2)
        self.assertEqual(code, 0, line)
        self.assertEqual(f["suite"], "TLS_CHACHA20_POLY1305_SHA256")
        self.assertEqual(f["echo"], "echo:hello")

    def test_rsa_chain_with_intermediate(self):
        line, code, f, srv = self.handshake("rsa-chain", suites=1)
        self.assertEqual(code, 0, line)
        self.assertEqual(f["sig"], "rsa_pss_rsae_sha256")
        self.assertEqual((f["chain"], f["depth"]), ("2", "3"))

    def test_wildcard_name(self):
        line, code, f, srv = self.handshake("rsa-chain", name="api.nanox.test")
        self.assertEqual(code, 0, line)
        line, code, f, srv = self.handshake("rsa-chain", name="a.b.nanox.test")
        self.assertIn("tls error cert_name", line)

    def test_missing_intermediate(self):
        leaf_only = (profile_dir("rsa-chain") / "chain.pem").read_text().split("-----END")[0]
        leaf_only += "-----END CERTIFICATE-----\n"
        line, code, f, srv = self.handshake("rsa-chain", chain=leaf_only)
        self.assertIn("tls error cert_untrusted class=tls", line)
        self.assertIn("unknown ca", srv.get("error", "").lower())

    def test_expired(self):
        line, code, f, srv = self.handshake("expired")
        self.assertIn("tls error cert_expired class=tls", line)
        self.assertIn("detail=leaf_validity", line)
        self.assertIn("certificate expired", srv.get("error", "").lower())

    def test_not_yet_valid(self):
        line, code, f, srv = self.handshake("ec-leaf", now=1700000000)  # 2023: before notBefore
        self.assertIn("tls error cert_expired", line)

    def test_wrong_name(self):
        line, code, f, srv = self.handshake("wrong-name")
        self.assertIn("tls error cert_name class=tls", line)

    def test_untrusted(self):
        line, code, f, srv = self.handshake("untrusted")
        self.assertIn("tls error cert_untrusted class=tls", line)

    def test_other_anchor(self):
        other = str(profile_dir("rsa-chain") / "anchor.der")
        line, code, f, srv = self.handshake("ec-leaf", anchor=other)
        self.assertIn("tls error cert_untrusted", line)

    def test_truncation_detected(self):
        # the server closes TCP without close_notify: not a clean end
        line, code, f, srv = self.handshake("ec-leaf", close_notify=False)
        self.assertIn("tls error peer_closed class=net during=data", line)


class TestPkiWithOpenSSL(unittest.TestCase):
    """OpenSSL (as a client) judges the test PKI as the guest must."""

    def connect(self, profile, name="provider.nanox.test"):
        d = profile_dir(profile)
        srv = Server(profile)
        ctx = ssl.create_default_context(
            cadata=pki.pem("CERTIFICATE", (d / "anchor.der").read_bytes()))
        try:
            with ctx.wrap_socket(socket.create_connection(("127.0.0.1", srv.port), timeout=20),
                                 server_hostname=name) as s:
                s.sendall(b"x")
                s.recv(10)
            return "ok"
        except ssl.SSLCertVerificationError as e:
            return e.verify_message
        finally:
            srv.finish()

    def test_profiles(self):
        # OpenSSL checks validity against the real clock, the guest against
        # the bench's fixed RTC; both are inside 2025..2030 for the good ones
        self.assertEqual(self.connect("ec-leaf"), "ok")
        self.assertEqual(self.connect("rsa-chain"), "ok")
        self.assertIn("expired", self.connect("expired"))
        self.assertIn("mismatch", self.connect("wrong-name").lower())
        self.assertIn("local issuer", self.connect("untrusted"))


class PkiTest(unittest.TestCase):
    def test_der_and_keys(self):
        self.assertEqual(pki.integer(0), b"\x02\x01\x00")
        self.assertEqual(pki.integer(128), b"\x02\x02\x00\x80")
        self.assertEqual(pki.oid("1.2.840.113549.1.1.11"),
                         bytes.fromhex("06092a864886f70d01010b"))
        self.assertEqual(pki.tag(0x04, b"x" * 200)[:3], b"\x04\x81\xc8")
        k = pki.EcKey("t")
        c = k.c
        x, y = k.Q
        self.assertEqual((y * y - (x ** 3 - 3 * x + c["b"])) % c["p"], 0)
        self.assertEqual(pki.EcKey("t").d, k.d)  # deterministic

    def test_cache(self):
        with tempfile.TemporaryDirectory() as d:
            pki.write("ec-leaf", d)
            first = (Path(d) / "chain.pem").read_bytes()
            pki.write("ec-leaf", d)
            self.assertEqual((Path(d) / "chain.pem").read_bytes(), first)
            self.assertEqual(os.stat(Path(d) / "key.pem").st_mode & 0o777, 0o600)


if __name__ == "__main__":
    unittest.main()
