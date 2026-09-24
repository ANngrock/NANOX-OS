#!/usr/bin/env python3
"""Test PKI of the M5 bench: deterministic keys and X.509 certificates
written from scratch with the Python standard library only (big-integer
RSA and elliptic-curve arithmetic, DER encoding).  Independent of the
guest's C parser and verifier (lib/tls/x509.c), and checked by OpenSSL
through Python's ssl module in tests/host/test_tls.py.

The keys are TEST KEYS derived from fixed seeds: they are generated on
the fly, never committed, and protect nothing.

Profiles (docs/m5-net.md §7):
  ec-leaf      RSA-2048 root CA -> P-256 leaf "provider.nanox.test"
  rsa-chain    P-384 root CA -> RSA-2048 intermediate -> RSA-2048 leaf
  expired      as ec-leaf, the leaf expired on 2025-06-01
  wrong-name   as ec-leaf, the leaf names "other.nanox.test"
  untrusted    the leaf is issued by a CA that is not provisioned

  pki.py make PROFILE OUTDIR    writes chain.pem, key.pem, anchor.der
"""

import base64
import hashlib
import hmac
import os
import sys
from pathlib import Path

# ---------------------------------------------------------------------------
# deterministic randomness

class Stream:
    """HMAC-SHA256 counter stream from a seed (test keys only)."""

    def __init__(self, seed):
        self.key = hashlib.sha256(seed.encode()).digest()
        self.n = 0

    def bytes(self, k):
        out = b""
        while len(out) < k:
            self.n += 1
            out += hmac.new(self.key, self.n.to_bytes(8, "big"), hashlib.sha256).digest()
        return out[:k]

    def int(self, bits):
        return int.from_bytes(self.bytes((bits + 7) // 8), "big") >> ((8 - bits % 8) % 8)


SMALL_PRIMES = [p for p in range(3, 2000) if all(p % q for q in range(2, int(p ** 0.5) + 1))]


def is_probable_prime(n, rnd, rounds=40):
    if n < 2:
        return False
    for p in SMALL_PRIMES:
        if n % p == 0:
            return n == p
    d, s = n - 1, 0
    while d % 2 == 0:
        d //= 2
        s += 1
    for _ in range(rounds):
        a = 2 + rnd.int(n.bit_length() - 1) % (n - 3)
        x = pow(a, d, n)
        if x in (1, n - 1):
            continue
        for _ in range(s - 1):
            x = pow(x, 2, n)
            if x == n - 1:
                break
        else:
            return False
    return True


def gen_prime(bits, rnd, e=65537):
    while True:
        c = rnd.int(bits) | (3 << (bits - 2)) | 1
        if (c - 1) % e and is_probable_prime(c, rnd):
            return c


class RsaKey:
    def __init__(self, seed, bits=2048):
        rnd = Stream("rsa:" + seed)
        self.e = 65537
        while True:
            p, q = gen_prime(bits // 2, rnd), gen_prime(bits // 2, rnd)
            n = p * q
            if p != q and n.bit_length() == bits:
                break
        self.p, self.q, self.n = max(p, q), min(p, q), n
        self.d = pow(self.e, -1, (self.p - 1) * (self.q - 1))
        self.bits = bits

    def spki(self):
        key = seq(integer(self.n), integer(self.e))
        return seq(seq(oid("1.2.840.113549.1.1.1"), null()), bitstring(key))

    def sign(self, data):
        """sha256WithRSAEncryption (PKCS #1 v1.5)."""
        k = self.bits // 8
        di = bytes.fromhex("3031300d060960864801650304020105000420") + hashlib.sha256(data).digest()
        em = b"\x00\x01" + b"\xff" * (k - len(di) - 3) + b"\x00" + di
        return pow(int.from_bytes(em, "big"), self.d, self.n).to_bytes(k, "big")

    sig_alg = property(lambda self: seq(oid("1.2.840.113549.1.1.11"), null()))

    def pem(self):
        dp, dq, qi = self.d % (self.p - 1), self.d % (self.q - 1), pow(self.q, -1, self.p)
        der = seq(integer(0), integer(self.n), integer(self.e), integer(self.d), integer(self.p),
                  integer(self.q), integer(dp), integer(dq), integer(qi))
        return pem("RSA PRIVATE KEY", der)


# ---------------------------------------------------------------------------
# elliptic curves (short Weierstrass, a = -3)

CURVES = {
    "P-256": dict(
        p=0xffffffff00000001000000000000000000000000ffffffffffffffffffffffff,
        n=0xffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551,
        b=0x5ac635d8aa3a93e7b3ebbd55769886bc651d06b0cc53b0f63bce3c3e27d2604b,
        gx=0x6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296,
        gy=0x4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5,
        oid="1.2.840.10045.3.1.7", size=32, hash=hashlib.sha256,
        sig_oid="1.2.840.10045.4.3.2"),
    "P-384": dict(
        p=int("fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe"
              "ffffffff0000000000000000ffffffff", 16),
        n=int("ffffffffffffffffffffffffffffffffffffffffffffffffc7634d81f4372ddf"
              "581a0db248b0a77aecec196accc52973", 16),
        b=int("b3312fa7e23ee7e4988e056be3f82d19181d9c6efe8141120314088f5013875a"
              "c656398d8a2ed19d2a85c8edd3ec2aef", 16),
        gx=int("aa87ca22be8b05378eb1c71ef320ad746e1d3b628ba79b9859f741e082542a38"
               "5502f25dbf55296c3a545e3872760ab7", 16),
        gy=int("3617de4a96262c6f5d9e98bf9292dc29f8f41dbd289a147ce9da3113b5f0b8c0"
               "0a60b1ce1d7e819d7a431d7c90ea0e5f", 16),
        oid="1.3.132.0.34", size=48, hash=hashlib.sha384, sig_oid="1.2.840.10045.4.3.3"),
}


def ec_add(c, P, Q):
    if P is None:
        return Q
    if Q is None:
        return P
    p = c["p"]
    if P[0] == Q[0]:
        if (P[1] + Q[1]) % p == 0:
            return None
        lam = (3 * P[0] * P[0] - 3) * pow(2 * P[1], -1, p) % p
    else:
        lam = (Q[1] - P[1]) * pow(Q[0] - P[0], -1, p) % p
    x = (lam * lam - P[0] - Q[0]) % p
    return (x, (lam * (P[0] - x) - P[1]) % p)


def ec_mul(c, k, P):
    R = None
    while k:
        if k & 1:
            R = ec_add(c, R, P)
        P = ec_add(c, P, P)
        k >>= 1
    return R


class EcKey:
    def __init__(self, seed, curve="P-256"):
        self.c = CURVES[curve]
        self.curve = curve
        rnd = Stream("ec:" + seed)
        self.d = 1 + rnd.int(self.c["size"] * 8) % (self.c["n"] - 1)
        self.Q = ec_mul(self.c, self.d, (self.c["gx"], self.c["gy"]))

    def point(self):
        s = self.c["size"]
        return b"\x04" + self.Q[0].to_bytes(s, "big") + self.Q[1].to_bytes(s, "big")

    def spki(self):
        return seq(seq(oid("1.2.840.10045.2.1"), oid(self.c["oid"])), bitstring(self.point()))

    def sign(self, data):
        """ECDSA with the curve's hash; the nonce is derived from the key
        and the message (deterministic, like RFC 6979 in spirit)."""
        c, n = self.c, self.c["n"]
        h = c["hash"](data).digest()
        e = int.from_bytes(h, "big") >> max(0, len(h) * 8 - n.bit_length())
        k = int.from_bytes(hmac.new(self.d.to_bytes(c["size"], "big"), h, hashlib.sha512).digest(),
                           "big") % (n - 1) + 1
        r = ec_mul(c, k, (c["gx"], c["gy"]))[0] % n
        s = pow(k, -1, n) * (e + r * self.d) % n
        return seq(integer(r), integer(s))

    sig_alg = property(lambda self: seq(oid(self.c["sig_oid"])))

    def pem(self):
        der = seq(integer(1), octet(self.d.to_bytes(self.c["size"], "big")),
                  tag(0xA0, oid(self.c["oid"])), tag(0xA1, bitstring(self.point())))
        return pem("EC PRIVATE KEY", der)


# ---------------------------------------------------------------------------
# DER

def tag(t, content):
    n = len(content)
    if n < 0x80:
        ln = bytes([n])
    else:
        b = n.to_bytes((n.bit_length() + 7) // 8, "big")
        ln = bytes([0x80 | len(b)]) + b
    return bytes([t]) + ln + content


def seq(*items):
    return tag(0x30, b"".join(items))


def set_(*items):
    return tag(0x31, b"".join(items))


def integer(v):
    b = v.to_bytes(max(1, (v.bit_length() + 8) // 8), "big")
    return tag(0x02, b)


def null():
    return b"\x05\x00"


def octet(b):
    return tag(0x04, b)


def bitstring(b):
    return tag(0x03, b"\x00" + b)


def boolean(v):
    return tag(0x01, b"\xff" if v else b"\x00")


def oid(dotted):
    parts = [int(x) for x in dotted.split(".")]
    out = bytes([40 * parts[0] + parts[1]])
    for p in parts[2:]:
        enc = [p & 0x7F]
        p >>= 7
        while p:
            enc.append(0x80 | (p & 0x7F))
            p >>= 7
        out += bytes(reversed(enc))
    return tag(0x06, out)


def name(cn, org="NANOX test PKI"):
    return seq(set_(seq(oid("2.5.4.10"), tag(0x0C, org.encode()))),
               set_(seq(oid("2.5.4.3"), tag(0x0C, cn.encode()))))


def time_(y, mo, d):
    if 1950 <= y < 2050:
        return tag(0x17, ("%02d%02d%02d000000Z" % (y % 100, mo, d)).encode())
    return tag(0x18, ("%04d%02d%02d000000Z" % (y, mo, d)).encode())


def extension(oid_s, value, critical=False):
    return seq(oid(oid_s), boolean(True) if critical else b"", octet(value))


def pem(label, der):
    b64 = base64.b64encode(der).decode()
    lines = [b64[i:i + 64] for i in range(0, len(b64), 64)]
    return "-----BEGIN %s-----\n%s\n-----END %s-----\n" % (label, "\n".join(lines), label)


def certificate(serial, subject_cn, subject_key, issuer_cn, issuer_key, not_before, not_after,
                ca=False, path_len=None, dns=(), eku_server=False):
    exts = []
    if ca:
        bc = boolean(True) + (integer(path_len) if path_len is not None else b"")
        exts.append(extension("2.5.29.19", seq(bc), critical=True))
        exts.append(extension("2.5.29.15", tag(0x03, b"\x01\x06"), critical=True))  # certSign, crlSign
    else:
        exts.append(extension("2.5.29.19", seq(), critical=True))
        exts.append(extension("2.5.29.15", tag(0x03, b"\x07\x80"), critical=True))  # digitalSignature
    if dns:
        exts.append(extension("2.5.29.17", seq(*[tag(0x82, d.encode()) for d in dns])))
    if eku_server:
        exts.append(extension("2.5.29.37", seq(oid("1.3.6.1.5.5.7.3.1"))))
    exts.append(extension("2.5.29.14", octet(hashlib.sha1(subject_key.spki()).digest())))
    tbs = seq(tag(0xA0, integer(2)), integer(serial), issuer_key.sig_alg, name(issuer_cn),
              seq(time_(*not_before), time_(*not_after)), name(subject_cn), subject_key.spki(),
              tag(0xA3, seq(*exts)))
    return seq(tbs, issuer_key.sig_alg, bitstring(issuer_key.sign(tbs)))


HOST = "provider.nanox.test"
VALID = ((2025, 1, 1), (2030, 1, 1))
CA_VALID = ((2024, 1, 1), (2040, 1, 1))


def make(profile):
    """Returns dict(chain=[DER...], key_pem=str, anchor=DER, leaf_key=...)."""
    if profile in ("ec-leaf", "expired", "wrong-name", "untrusted"):
        ca = RsaKey("root-rsa")
        root = certificate(1, "NANOX Test Root R1", ca, "NANOX Test Root R1", ca, *CA_VALID,
                           ca=True)
        signer, signer_cn = ca, "NANOX Test Root R1"
        if profile == "untrusted":
            other = RsaKey("root-rsa-other")
            signer, signer_cn = other, "NANOX Unknown Root"
        leaf_key = EcKey("leaf-ec")
        validity = ((2024, 1, 1), (2025, 6, 1)) if profile == "expired" else VALID
        dns = ["other.nanox.test"] if profile == "wrong-name" else [HOST]
        leaf = certificate(10, HOST, leaf_key, signer_cn, signer, *validity, dns=dns,
                           eku_server=True)
        return {"chain": [leaf], "key_pem": leaf_key.pem(), "anchor": root}
    if profile == "rsa-chain":
        root_key = EcKey("root-p384", "P-384")
        root = certificate(2, "NANOX Test Root E1", root_key, "NANOX Test Root E1", root_key,
                           *CA_VALID, ca=True)
        inter_key = RsaKey("inter-rsa")
        inter = certificate(3, "NANOX Test Intermediate R2", inter_key, "NANOX Test Root E1",
                            root_key, *CA_VALID, ca=True, path_len=0)
        leaf_key = RsaKey("leaf-rsa")
        leaf = certificate(11, HOST, leaf_key, "NANOX Test Intermediate R2", inter_key, *VALID,
                           dns=[HOST, "*.nanox.test"], eku_server=True)
        return {"chain": [leaf, inter], "key_pem": leaf_key.pem(), "anchor": root}
    raise ValueError("unknown profile %r" % profile)


PROFILES = ("ec-leaf", "rsa-chain", "expired", "wrong-name", "untrusted")


def write(profile, outdir):
    """Writes chain.pem, key.pem (mode 0600), anchor.der into outdir (cached:
    an existing complete set made by the same generator is reused)."""
    outdir = Path(outdir)
    stamp = hashlib.sha256(Path(__file__).read_bytes() + profile.encode()).hexdigest()[:16]
    tag_file = outdir / "generator"
    files = [outdir / f for f in ("chain.pem", "key.pem", "anchor.der")]
    if tag_file.exists() and tag_file.read_text() == stamp and all(f.exists() for f in files):
        return outdir
    outdir.mkdir(parents=True, exist_ok=True)
    m = make(profile)
    (outdir / "chain.pem").write_text("".join(pem("CERTIFICATE", c) for c in m["chain"]))
    kp = outdir / "key.pem"
    kp.write_text(m["key_pem"])
    os.chmod(kp, 0o600)
    (outdir / "anchor.der").write_bytes(m["anchor"])
    tag_file.write_text(stamp)
    return outdir


def main(argv):
    if len(argv) != 4 or argv[1] != "make" or argv[2] not in PROFILES:
        sys.stderr.write(__doc__)
        return 2
    print(write(argv[2], argv[3]))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
