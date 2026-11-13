"""Self-tests of scan.py on planted memory images and files: no credential of anyone is read or
made, and no VM is booted.

    python3 -I scan_test.py

Each fake secret is a digest of a label, so no provider issued it and every run sees the same
bytes; a memory image is the test's own bytes, zeros with text between, as a saved guest's is.
"""

import base64
import contextlib
import hashlib
import importlib.util
import io
import json
import os
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location("scan", os.path.join(HERE, "scan.py"))
scan = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(scan)

# The bring-up's run, which credentials_test.py holds to harness-bringup-v1's record of what a
# provider shows: the scan's own must be it, not one of its own.
RUN = scan.credentials.RUN


def fake(label, length):
    """A secret no provider issued: `length` base64url characters of a digest of `label`."""
    return base64.urlsafe_b64encode(hashlib.sha512(label.encode()).digest()).decode()[:length]


ACCESS = "sk-ant-oat01-" + fake("access", 80)
REFRESH = "sk-ant-ort01-" + fake("refresh", 80)
# What a run holds by design in the access token's place: its marker kept, its random part not.
PLACEHOLDER = "sk-ant-oat01-" + fake("placeholder", 80)
SHORT = fake("short", RUN - 2)
# A page's worth of a guest's text, none of it a secret's.
TEXT = b"export PATH=/usr/local/bin:/usr/bin HOME=/root " * 85


class Planted:
    """A directory of the test's: a secrets document, a public one, and files to scan."""

    def __init__(self, secrets=None, public=None):
        self.root = tempfile.TemporaryDirectory()
        self.secrets = self.json("secrets.json", secrets or {"claudeAiOauth": {"accessToken": ACCESS, "refreshToken": REFRESH}})
        self.public = self.json("public.json", public) if public else None
        self.saved = os.path.join(self.root.name, "saved")
        os.mkdir(self.saved)

    def json(self, name, document):
        path = os.path.join(self.root.name, name)
        with open(path, "w") as f:
            json.dump(document, f)
        return path

    def image(self, name, planted, at):
        """A memory image `name` in the save's directory: zeros, a page of text, `planted` at
        offset `at`, more zeros."""
        data = bytearray(1 << 20)
        data[1 << 16 : (1 << 16) + len(TEXT)] = TEXT
        data[at : at + len(planted)] = planted
        path = os.path.join(self.saved, name)
        with open(path, "wb") as f:
            f.write(bytes(data))
        return path

    def scan(self, *paths):
        argv = ["--secrets", self.secrets]
        if self.public:
            argv += ["--public", self.public]
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = scan.main(argv + list(paths or [self.saved]))
        said = out.getvalue()
        for value in (ACCESS, REFRESH, SHORT):
            assert value[:RUN] not in said and value[-RUN:] not in said, "a value printed"
        return code, said


# Unaligned, as a value lies in a guest's memory wherever its allocator put it.
AT = (3 << 16) + 5


class LeakTest(unittest.TestCase):
    def test_a_planted_values_run_is_found_where_it_lies_and_none_without_it(self):
        """f18's case: a 31-byte run of a value in a memory image is found at its offset, whole;
        an image holding none of it, or holding what a provider itself shows of a key (RUN - 1
        bytes), is clean."""
        p = Planted()
        p.image("memory", ACCESS[20:51].encode(), AT)
        code, said = p.scan()
        self.assertEqual(code, 3, said)
        self.assertIn("memory offset %d length 31: a run of secrets.json:accessToken" % AT, said)
        os.unlink(os.path.join(p.saved, "memory"))
        p.image("memory", ACCESS[20 : 20 + RUN - 1].encode(), AT)
        code, said = p.scan()
        self.assertEqual(code, 0, said)
        self.assertIn("0 secret runs found", said)
        os.unlink(os.path.join(p.saved, "memory"))
        p.image("memory", b"", AT)
        code, said = p.scan()
        self.assertEqual(code, 0, said)

    def test_any_run_longer_than_a_provider_shows_is_found_at_every_offset(self):
        """A run of RUN bytes, and f18's 30, found at every offset of a group of eight, where
        f18's every-8th-offset lookup of 24-byte windows promised 31 alone."""
        for length in (RUN, 30):
            for shift in range(8):
                p = Planted()
                p.image("memory", REFRESH[40 : 40 + length].encode(), AT + shift)
                code, said = p.scan()
                self.assertEqual(code, 3, (length, shift, said))
                self.assertIn("offset %d length %d" % (AT + shift, length), said)

    def test_a_value_on_a_disk_or_in_an_environment_is_found_by_its_path(self):
        """A save's directory, a disk image beside it and a member's environment as given, each
        holding a value whole, each found by its path; a short secret whole too."""
        p = Planted(secrets={"tokens": {"access_token": ACCESS, "account_id": SHORT}})
        disk = os.path.join(p.root.name, "layer.img")
        with open(disk, "wb") as f:
            f.write(bytes(4096) + b'{"access_token":"' + ACCESS.encode() + b'"}' + bytes(4096))
        env = os.path.join(p.root.name, "env")
        os.mkdir(env)
        with open(os.path.join(env, "harness-codex"), "wb") as f:
            f.write(b"HOME=/root\0ACCOUNT=" + SHORT.encode() + b"\0")
        code, said = p.scan(p.saved, disk, env)
        self.assertEqual(code, 3, said)
        self.assertIn("layer.img offset %d length %d" % (4096 + 17, len(ACCESS)), said)
        self.assertIn("harness-codex offset 19 length %d: a run of secrets.json:account_id" % len(SHORT), said)

    def test_a_value_held_as_utf16_or_base64_is_found(self):
        """A value as a wide string holds it, and base64'd in either alphabet from each place it
        can start in a group of three, each found."""
        held = [ACCESS.encode("utf-16-le")]
        for skip in range(3):
            prefix = b"x" * skip
            held.append(base64.b64encode(prefix + ACCESS.encode() + b"!"))
            held.append(base64.urlsafe_b64encode(prefix + ACCESS.encode() + b"?"))
        for form in held:
            p = Planted()
            p.image("memory", form, AT)
            code, said = p.scan()
            self.assertEqual(code, 3, (form[:8], said))
            self.assertIn("memory offset", said)

    def test_a_placeholders_runs_are_no_leak(self):
        """A run holds its placeholder by design: the marker it shares with the value is none
        of the value's, and its own shape is explained; the value itself is still a leak."""
        p = Planted(public={"claudeAiOauth": {"accessToken": PLACEHOLDER}})
        p.image("memory", b"Bearer " + PLACEHOLDER.encode(), AT)
        code, said = p.scan()
        self.assertEqual(code, 0, said)
        self.assertIn("public", said)
        os.unlink(os.path.join(p.saved, "memory"))
        p.image("memory", b"Bearer " + ACCESS.encode(), AT)
        code, said = p.scan()
        self.assertEqual(code, 3, said)
        # The run found is the value's from where it stops being the placeholder's.
        held = set(scan.windows_of(PLACEHOLDER.encode(), RUN))
        own = next(i for i in range(len(ACCESS)) if ACCESS.encode()[i : i + RUN] not in held)
        self.assertLess(own, len("sk-ant-oat01-"))
        self.assertIn("offset %d length %d" % (AT + 7 + own, len(ACCESS) - own), said)

    def test_an_unexplained_credential_shape_is_listed_by_where(self):
        """A JWT none of the given values explains is listed by where it lies (exit 4); given as
        public, it is explained; a program's literal prefix, and a shape inside a longer word,
        are no credential."""
        jwt = "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ4In0." + fake("signature", 43)
        p = Planted()
        p.image("memory", jwt.encode(), AT)
        code, said = p.scan()
        self.assertEqual(code, 4, said)
        self.assertIn("SHAPE %s offset %d length %d: a JWT" % (os.path.join(p.saved, "memory"), AT, len(jwt)), said)
        self.assertNotIn(jwt, said)
        p = Planted(public={"tokens": {"id_token": jwt}})
        p.image(
            "memory",
            jwt.encode() + b' if (key.startsWith("sk-ant-api03-")) /dev/disk-by-partuuid-' + fake("disk", 32).encode(),
            AT,
        )
        code, said = p.scan()
        self.assertEqual(code, 0, said)

    def test_what_cannot_be_read_is_said_without_a_value(self):
        p = Planted()
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = scan.main(["--secrets", os.path.join(p.root.name, "missing.json"), p.saved])
        self.assertEqual(code, 2, out.getvalue())


if __name__ == "__main__":
    unittest.main()
