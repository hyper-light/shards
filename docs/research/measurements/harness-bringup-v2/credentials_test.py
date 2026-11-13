"""Self-tests of credentials.py on fake sign-ins: no credential of anyone is read or made.

    python3 -I credentials_test.py

Each fake secret is a digest of a label, so no provider issued it and every run sees the same
bytes. The leak check is also run over the committed records of harness bring-up v1 and v2,
real text that cannot hold these fakes.
"""

import base64
import contextlib
import hashlib
import importlib.util
import io
import json
import os
import re
import stat
import tempfile
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location("credentials", os.path.join(HERE, "credentials.py"))
credentials = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(credentials)


def fake(label, length):
    """A secret no provider issued: `length` base64url characters of a digest of `label`."""
    return base64.urlsafe_b64encode(hashlib.sha512(label.encode()).digest()).decode()[:length]


ACCESS = "sk-ant-oat01-" + fake("access", 80)
REFRESH = "sk-ant-ort01-" + fake("refresh", 80)
# Shorter than the 16 characters below which the check once skipped a value.
SHORT = fake("short", 12)


def shown_by_provider():
    """How many of a key's first and last characters OpenAI's refusal shows, the rest masked one
    for one, as harness bring-up v1 recorded it."""
    record = os.path.join(HERE, "..", "harness-bringup-v1", "records", "codex-login-volume", "stdout.bin")
    with open(record, "rb") as f:
        masked = re.search(r"Incorrect API key provided: (\S+?)\.", f.read().decode()).group(1)
    return masked.index("*"), len(masked) - masked.rindex("*") - 1


FIRST, LAST = shown_by_provider()


def quietly(call, *args):
    """`call`'s return and what it printed."""
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        result = call(*args)
    return result, out.getvalue()


class Private:
    """A login of `document` beside a fresh 0700 PRIVATE, both in a directory of the test's."""

    def __init__(self, document, mode=0o600):
        self.root = tempfile.TemporaryDirectory()
        os.chmod(self.root.name, 0o700)
        self.login = os.path.join(self.root.name, "login.json")
        with open(os.open(self.login, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as f:
            json.dump(document, f)
        os.chmod(self.login, mode)
        self.private = os.path.join(self.root.name, "private")
        os.mkdir(self.private, 0o700)
        os.chmod(self.private, 0o700)
        self.work = os.path.join(self.root.name, "work")
        os.mkdir(self.work)

    def copy(self):
        quietly(credentials.copy, self.login, self.private, "home", "login.json")
        return os.path.join(self.private, "home", "login.json")

    def file(self, name, data):
        with open(os.path.join(self.work, name), "wb") as f:
            f.write(data)

    def leakcheck(self, *paths):
        return quietly(credentials.leakcheck, self.private, list(paths) or [self.work])

    def close(self):
        self.root.cleanup()


def claude(access=ACCESS, refresh=REFRESH, **more):
    return {"claudeAiOauth": {"accessToken": access, "refreshToken": refresh,
                              "expiresAt": int((time.time() + 3600) * 1000), "scopes": ["user:inference"],
                              **more}}


class LeakCheck(unittest.TestCase):
    def setUp(self):
        self.p = Private(claude(apiKey=SHORT))
        self.p.copy()
        self.addCleanup(self.p.close)

    def test_a_secret_however_short_is_looked_for_whole(self):
        self.p.file("log.txt", b"the harness said " + SHORT.encode() + b" and went on\n")
        status, said = self.p.leakcheck()
        self.assertEqual(status, 3, said)
        self.assertIn("FOUND in", said)
        self.assertNotIn(SHORT, said)

    def test_a_run_one_longer_than_a_provider_shows_is_found_at_every_offset(self):
        run = ACCESS[21:21 + FIRST + 1].encode()
        _, runs, whole = credentials.all_secrets(credentials.load_state(self.p.private))
        for offset in range(len(run)):
            data = b"x" * offset + run + b"\n"
            self.assertTrue(credentials.leaked(data, runs, whole), f"a run at offset {offset}")

    def test_what_a_provider_shows_of_a_key_is_no_leak(self):
        shown = ACCESS[:FIRST] + "*" * (len(ACCESS) - FIRST - LAST) + ACCESS[-LAST:]
        self.p.file("refusal.json", json.dumps({"message": "Incorrect API key provided: " + shown}).encode())
        status, said = self.p.leakcheck()
        self.assertEqual(status, 0, said)

    def test_an_empty_value_is_no_secret(self):
        other = Private({"OPENAI_API_KEY": "", "tokens": {"access_token": fake("codex", 900)}})
        self.addCleanup(other.close)
        other.copy()
        other.file("log.txt", b"nothing of either login\n")
        status, said = other.leakcheck()
        self.assertEqual(status, 0, said)

    def test_the_committed_records_hold_none_of_these_fakes(self):
        records = [os.path.join(HERE, "records"), os.path.join(HERE, "..", "harness-bringup-v1", "records")]
        status, said = self.p.leakcheck(*records)
        self.assertEqual(status, 0, said)
        self.assertIn("not found", said)


class Settle(unittest.TestCase):
    def setUp(self):
        self.p = Private(claude())
        self.copied = self.p.copy()
        self.addCleanup(self.p.close)

    def rewrite(self, path, document):
        with open(path, "w") as f:
            json.dump(document, f)

    def original(self):
        with open(self.p.login) as f:
            return json.load(f)

    def test_the_copy_is_a_0600_file_in_a_0700_directory(self):
        self.assertEqual(stat.S_IMODE(os.stat(self.copied).st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(os.stat(os.path.dirname(self.copied)).st_mode), 0o700)

    def test_a_private_directory_others_may_enter_is_refused(self):
        os.chmod(self.p.private, 0o755)
        with self.assertRaises(SystemExit):
            quietly(credentials.copy, self.p.login, self.p.private, "again", "login.json")

    def test_an_unchanged_copy_is_left(self):
        before = self.original()
        _, said = quietly(credentials.settle, self.p.private)
        self.assertIn("unchanged", said)
        self.assertEqual(self.original(), before)

    def test_a_change_of_format_alone_is_not_written_back(self):
        before = self.original()
        self.rewrite(self.copied, claude(subscriptionType="newer"))
        _, said = quietly(credentials.settle, self.p.private)
        self.assertIn("its tokens did not", said)
        self.assertEqual(self.original(), before)

    def test_a_refresh_is_written_back_its_mode_kept(self):
        # A mode of the person's own, not the 0600 a temporary file is made with.
        p = Private(claude(), mode=0o640)
        self.addCleanup(p.close)
        copied = p.copy()
        refreshed = claude(access="sk-ant-oat01-" + fake("refreshed", 80))
        self.rewrite(copied, refreshed)
        _, said = quietly(credentials.settle, p.private)
        self.assertIn("written back", said)
        with open(p.login) as f:
            self.assertEqual(json.load(f), refreshed)
        self.assertEqual(stat.S_IMODE(os.stat(p.login).st_mode), 0o640)
        self.assertEqual([n for n in os.listdir(p.root.name) if n.startswith(".shards-writeback-")], [])

    def test_a_refresh_meets_a_changed_original_as_a_conflict(self):
        self.rewrite(self.copied, claude(access="sk-ant-oat01-" + fake("in the microVM", 80)))
        on_host = claude(access="sk-ant-oat01-" + fake("on the host", 80))
        self.rewrite(self.p.login, on_host)
        _, said = quietly(credentials.settle, self.p.private)
        self.assertIn("CONFLICT", said)
        self.assertEqual(self.original(), on_host)

    def test_a_change_to_the_original_as_it_is_written_back_is_kept(self):
        refreshed = claude(access="sk-ant-oat01-" + fake("refreshed", 80))
        self.rewrite(self.copied, refreshed)
        on_host = claude(access="sk-ant-oat01-" + fake("refreshed on the host", 80))
        real = credentials.write_back

        def racing(*args):
            # The host's own refresh lands after settle last read the original, before it writes.
            self.rewrite(self.p.login, on_host)
            return real(*args)

        credentials.write_back = racing
        self.addCleanup(setattr, credentials, "write_back", real)
        _, said = quietly(credentials.settle, self.p.private)
        self.assertEqual(self.original(), on_host, said)
        self.assertIn("CONFLICT", said)
        self.assertEqual([n for n in os.listdir(self.p.root.name) if n.startswith(".shards-writeback-")], [])

    def test_scrub_removes_everything_it_holds(self):
        made = os.path.join(self.p.private, "home", "state")
        os.mkdir(made)
        with open(os.path.join(made, "f"), "w") as f:
            f.write("harness state")
        os.chmod(os.path.join(made, "f"), 0o400)
        os.chmod(made, 0o500)
        quietly(credentials.scrub, self.p.private)
        self.assertFalse(os.path.exists(self.p.private))


class Validity(unittest.TestCase):
    def test_minutes_left_expired_and_unknown(self):
        self.assertIn("more minutes", credentials.validity(claude()))
        expired = claude()
        expired["claudeAiOauth"]["expiresAt"] = int((time.time() - 600) * 1000)
        self.assertIn("expired", credentials.validity(expired))
        self.assertIn("unknown", credentials.validity({"tokens": {"access_token": "not.a jwt.at all"}}))

    def test_a_codex_token_says_when_it_was_last_refreshed(self):
        claims = base64.urlsafe_b64encode(json.dumps({"exp": time.time() + 7200}).encode()).decode().rstrip("=")
        document = {"tokens": {"access_token": "e30." + claims + ".c2ln"}, "last_refresh": "2026-01-01T00:00:00.000Z"}
        said = credentials.validity(document)
        self.assertIn("more minutes", said)
        self.assertIn("days ago", said)


if __name__ == "__main__":
    unittest.main(verbosity=2)
