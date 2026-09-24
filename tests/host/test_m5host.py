"""Tests of the M5 host-side pieces: the bench DNS server, the provisioning
of the data disk (checked with both implementations of the store format),
the QEMU network options and the diagnostic scripts' helpers."""

import os
import socket
import struct
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "tools" / "bench"))
sys.path.insert(0, str(REPO / "tools" / "store"))
sys.path.insert(0, str(REPO / "tools" / "bridge"))
import m5host  # noqa: E402
import nxstore  # noqa: E402
import qemu  # noqa: E402

STORETOOL = REPO / "out" / "host" / "storetool"


def query(name, qid=0x4242):
    q = struct.pack(">HHHHHH", qid, 0x0100, 1, 0, 0, 0)
    for label in name.split("."):
        q += bytes([len(label)]) + label.encode()
    return q + b"\x00" + struct.pack(">HH", 1, 1)


class DnsServerTest(unittest.TestCase):
    def setUp(self):
        self.dns = m5host.DnsServer({"provider.nanox.test": "10.0.2.2"})

    def tearDown(self):
        self.dns.close()

    def test_answer_known(self):
        r = self.dns.answer(query("provider.nanox.test"))
        self.assertEqual(r[:2], b"\x42\x42")
        self.assertEqual(r[2:4], b"\x81\x80")
        self.assertEqual(struct.unpack(">H", r[6:8])[0], 1)
        self.assertEqual(r[-4:], socket.inet_aton("10.0.2.2"))
        self.assertEqual(self.dns.queries, ["provider.nanox.test"])

    def test_nxdomain_and_silent(self):
        r = self.dns.answer(query("other.nanox.test"))
        self.assertEqual(r[3] & 0x0F, 3)
        self.dns.silent = True
        self.assertIsNone(self.dns.answer(query("provider.nanox.test")))

    def test_over_udp(self):
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.settimeout(5)
        s.sendto(query("provider.nanox.test", 7), ("127.0.0.1", self.dns.port))
        r, _ = s.recvfrom(512)
        s.close()
        self.assertEqual(struct.unpack(">H", r[:2])[0], 7)
        self.assertEqual(r[-4:], socket.inet_aton("10.0.2.2"))


class ProvisionTest(unittest.TestCase):
    def test_provisioned_image_reads_back(self):
        objs = [("cfg/net.dns", nxstore.KIND_CONFIG, b"10.0.2.2"),
                ("secret/provider", nxstore.KIND_SECRET, b"k" * 5000),
                ("tls/anchor0", nxstore.KIND_ANCHOR, bytes(range(256)) * 3)]
        data = nxstore.provisioned_image(objs)
        img = nxstore.Image(data)
        rep, problems = nxstore.check(img)
        self.assertEqual(problems, [])
        self.assertEqual(rep["gen"], 1)
        root = nxstore.mount(img)["root"]
        got = {o["name"]: nxstore.object_data(img, o)[0] for o in root["objects"]}
        self.assertEqual(got, {n: d for n, _, d in objs})
        self.assertEqual(nxstore.state_of(img, root)["config"], {"net.dns": "10.0.2.2"})

    @unittest.skipUnless(STORETOOL.exists(), "out/host/storetool not built")
    def test_lib_store_mounts_it(self):
        data = nxstore.provisioned_image([("cfg/a", 1, b"x"), ("cfg/b", 1, b"y" * 4097)])
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "p.img")
            Path(p).write_bytes(data)
            out = subprocess.run([str(STORETOOL), "mount", p], stdout=subprocess.PIPE,
                                 check=True).stdout.decode()
        self.assertIn("mount ok gen=1", out)
        self.assertIn("objects=2 check=ok", out)

    def test_rejects_bad_objects(self):
        with self.assertRaises(ValueError):
            nxstore.provisioned_image([("bad name", 1, b"x")])
        with self.assertRaises(ValueError):
            nxstore.provisioned_image([("cfg/a", 1, b"x"), ("cfg/a", 1, b"y")])
        with self.assertRaises(ValueError):
            nxstore.provisioned_image([("big", 1, bytes(nxstore.OBJ_MAX_BYTES + 1))])

    def test_empty_format_unchanged(self):
        # the build's data disk must not change with the provisioning code
        self.assertEqual(nxstore.format_image()[:4096], nxstore.format_image()[:4096])
        rep, problems = nxstore.check(nxstore.Image(nxstore.format_image()))
        self.assertEqual((rep["gen"], problems), (1, []))


class ServicesTest(unittest.TestCase):
    def test_config_and_objects(self):
        with tempfile.TemporaryDirectory() as d:
            sv = m5host.Services({"config": {"provider.host": "provider.nanox.test"}}, d)
            try:
                cfg = sv.config()
                self.assertEqual(cfg["net.dns"], "10.0.2.2")
                self.assertEqual(cfg["net.dns_port"], str(sv.dns.port))
                self.assertEqual(cfg["echo.port"], str(sv.echo.port))
                self.assertEqual(cfg["provider.host"], "provider.nanox.test")
                data = sv.provision(os.path.join(d, "data.img"))
                st = nxstore.state_of(nxstore.Image(data),
                                      nxstore.mount(nxstore.Image(data))["root"])
                self.assertEqual(st["config"]["net.dns_port"], str(sv.dns.port))
            finally:
                sv.close()

    def test_echo_server(self):
        e = m5host.EchoServer()
        try:
            s = socket.create_connection(("127.0.0.1", e.port), timeout=5)
            s.sendall(b"abc\n")
            self.assertEqual(s.recv(16), b"abc\n")
            s.close()
            self.assertEqual(e.lines, ["abc"])
        finally:
            e.close()


class QemuNetTest(unittest.TestCase):
    def test_net_argv(self):
        argv = qemu.base_argv("img", "vars", "stdio", net_qmp="/tmp/q.sock")
        self.assertIn("user,id=nxnet,ipv6=off", argv)
        self.assertIn("virtio-rng-pci,rng=nxrng", argv)
        self.assertIn("unix:/tmp/q.sock,server=on,wait=off", argv)
        plain = qemu.base_argv("img", "vars", "stdio")
        self.assertNotIn("-netdev", plain)


if __name__ == "__main__":
    unittest.main()
