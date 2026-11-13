"""The leak scan of plan step 33 (D242, D259): whether a credential's value, or a run of more of
its bytes than a provider itself shows of one, is anywhere in a run's saved memory, its disk or
its members' environments; and every credential-shaped string there that no value it is told
of explains.

    scan.py --secrets FILE [--secrets FILE]... [--public FILE]... PATH...

--secrets FILE  a JSON document whose every string held under a name with token, key, secret or
                account_id in it is a secret (harness-bringup-v2/credentials.py's): a person's
                sign-in (Codex's auth.json, Claude Code's .credentials.json), or what a run's
                lessor leased, written to a file of the caller's alone.
--public FILE   the same, of values a run holds by design: its placeholders. A run of a secret's
                bytes that is a public value's too is no leak, and a credential-shaped string
                that one explains is no finding.
PATH            files and directories, every regular file in them read whole, no link
                followed: a save's directory (ImageGuest::save: its memory, state, swap, layer,
                ...), a disk image, the members' environments as given.

Each finding is a line of its kind, path, offset and length, and the name its value is held
under: never a value. Exit 0: nothing found; 3: a secret's bytes found, a leak; 4: no secret's
bytes, but credential-shaped strings no value given explains, each by where, for a person to look
at; 2: what it was asked to read could not be.
"""

import base64
import importlib.util
import json
import mmap
import os
import re
import sys

# The secrets' names and the leak's run are the bring-up's, where the method lives (R12):
# harness-bringup-v2/credentials.py, its review's F1.
_HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location(
    "credentials", os.path.join(_HERE, os.pardir, "harness-bringup-v2", "credentials.py")
)
credentials = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(credentials)

RUN = credentials.RUN

# A token carries at least 128 bits: RFC 6749 §10.10 holds the odds of guessing one an
# authorization server makes to 2^-128 or less. A base64url character carries 6 (RFC 4648 §5's
# 64 symbols), so a credential's random part is at least ceil(128 / 6) = 22 of them; a shorter
# match is a literal of a program's (a prefix it checks for), not a credential.
ENTROPY_BITS = 128
BITS_PER_CHARACTER = 6
LEAST = -(-ENTROPY_BITS // BITS_PER_CHARACTER)

# The shapes the bring-up's records were scanned for (harness-bringup-v2/README.md, Limits):
# key prefixes, JWTs, bearer credentials; and token field names, counted alone, as they name
# where a value goes and every harness's code holds them.
SHAPES = [
    # Anthropic's keys and tokens (sk-ant-...) and OpenAI's keys (sk-, sk-proj-, ...).
    ("an sk- key", re.compile(rb"sk-[A-Za-z0-9_-]{%d,}" % LEAST), 0),
    # A JWS in compact form (RFC 7515 §7.1): base64url of a JSON object, '{"' being "eyJ",
    # then its payload, then a signature of 128 bits or more.
    ("a JWT", re.compile(rb"eyJ[A-Za-z0-9_-]*\.[A-Za-z0-9_-]*\.[A-Za-z0-9_-]{%d,}" % LEAST), 0),
    # RFC 6750 §2.1: "Bearer" 1*SP b64token, the credential the second group.
    ("a bearer credential", re.compile(rb"Bearer +([A-Za-z0-9._~+/-]{%d,}=*)" % LEAST), 1),
]
FIELDS = re.compile(rb"access_token|refresh_token|id_token")


def documents(paths):
    """Each (label, value) of the JSON documents `paths`, its label the file's name and the
    name the value is held under."""
    found = []
    for path in paths:
        with open(path, "rb") as f:
            document = json.loads(f.read())
        for name, value in credentials.named_secrets(document):
            found.append(("%s:%s" % (os.path.basename(path), name), value))
    return found


def forms(value):
    """`value` as a memory holds it: its UTF-8; and base64 of it, in either alphabet (RFC 4648
    §4, §5), from each of the three places its first byte can take in a group of three, of its
    whole groups alone, as in whatever larger encoding it lies. Each with its name."""
    raw = value.encode()
    yield "", raw
    for skip in range(3):
        whole = raw[skip : skip + (len(raw) - skip) // 3 * 3]
        if whole:
            yield " (base64)", base64.b64encode(whole)
            yield " (base64url)", base64.urlsafe_b64encode(whole)


def windows_of(data, width):
    return [data[i : i + width] for i in range(len(data) - width + 1)]


class Index:
    """What is looked for: every RUN-byte window of each secret in each of its forms, by the
    label it came from, and every RUN-character one of its UTF-16LE, as JavaScriptCore's and
    Windows' wide strings hold text; less those a public value holds in a form of its own;
    each secret shorter than RUN, whole; and the bytes any of them holds, which a run of a
    secret's is made of alone."""

    def __init__(self, secrets, public):
        self.public = [form for _, value in public for _, form in forms(value)]
        held = set()
        held16 = set()
        for _, value in public:
            for _, form in forms(value):
                held.update(windows_of(form, RUN))
            held16.update(windows_of(value.encode("utf-16-le"), 2 * RUN))
        self.windows = {}
        self.windows16 = {}
        self.short = []
        for label, value in secrets:
            if len(value.encode()) < RUN:
                if not any(value.encode() in p for p in self.public):
                    self.short.append((label, value.encode()))
                continue
            for name, form in forms(value):
                for window in windows_of(form, RUN):
                    if window not in held:
                        self.windows.setdefault(window, label + name)
            for window in windows_of(value.encode("utf-16-le"), 2 * RUN):
                if window not in held16:
                    self.windows16.setdefault(window, label + " (UTF-16LE)")
        self.runs = _runs_of([set().union(*self.windows)] if self.windows else [])
        units = [set(), set()]
        for window in self.windows16:
            units[0].update(window[0::2])
            units[1].update(window[1::2])
        self.runs16 = _runs_of(units if self.windows16 else [])

    def holds_secret(self, data):
        return any(w in self.windows for w in windows_of(data, RUN)) or any(
            value in data for _, value in self.short
        )

    def explained(self, token):
        return any(token in p or p in token for p in self.public)


def _runs_of(units):
    """A pattern of RUN or more characters, each its bytes in turn of the sets `units` (one, a
    byte; two, a UTF-16LE code unit's low and high byte): none for no sets."""
    if not units:
        return None
    unit = b"".join(b"[" + b"".join(re.escape(bytes([c])) for c in sorted(u)) + b"]" for u in units)
    return re.compile(b"(?:" + unit + b"){%d,}" % RUN)


def _runs(buf, pattern, windows, width, found):
    """Each run of `windows`' in `buf` within a match of `pattern`, those that touch merged,
    added to `found` as (offset, length, label): every offset of a match looked up."""
    if pattern is None:
        return
    for candidate in pattern.finditer(buf):
        start, end = candidate.span()
        first = last = label = None
        for i in range(start, end - width + 1):
            hit = windows.get(buf[i : i + width])
            if hit is None:
                continue
            if last is not None and i <= last + width:
                last = i
                continue
            if last is not None:
                found.append((first, last + width - first, label))
            first = last = i
            label = hit
        if last is not None:
            found.append((first, last + width - first, label))


def leaks(buf, index):
    """Each (offset, length, label) where `buf` holds a run of RUN or more of a secret's bytes
    in one of its forms, runs that touch merged, or a short secret whole. A run lies within a
    run of its form's own bytes, so only those are looked up, at every offset."""
    found = []
    _runs(buf, index.runs, index.windows, RUN, found)
    _runs(buf, index.runs16, index.windows16, 2 * RUN, found)
    for label, value in index.short:
        at = buf.find(value)
        while at != -1:
            found.append((at, len(value), label))
            at = buf.find(value, at + 1)
    return found


# A credential begins where a token does: its shape matched inside a longer word ("disk-" holds
# "sk-") is none.
TOKEN_CHARACTERS = frozenset(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-")


def shapes(buf, index):
    """Each (shape, offset, length, class) of a credential-shaped string in `buf`: a secret's
    (its bytes a leak's, counted there), a public value's, or unexplained."""
    found = []
    for name, pattern, group in SHAPES:
        for match in pattern.finditer(buf):
            if match.start() > 0 and buf[match.start() - 1] in TOKEN_CHARACTERS:
                continue
            token = match.group(group)
            if index.holds_secret(token):
                kind = "secret"
            elif index.explained(token):
                kind = "public"
            else:
                kind = "unexplained"
            found.append((name, match.start(group), len(token), kind))
    return found


def files(paths):
    for root in paths:
        if os.path.isfile(root) and not os.path.islink(root):
            yield root
            continue
        for directory, dirs, names in os.walk(root):
            dirs[:] = [d for d in dirs if not os.path.islink(os.path.join(directory, d))]
            for leaf in sorted(names):
                path = os.path.join(directory, leaf)
                if os.path.isfile(path) and not os.path.islink(path):
                    yield path


def scan(paths, index, say):
    """Scans every file of `paths`, saying each finding; the counts."""
    counts = {"files": 0, "bytes": 0, "leaks": 0, "unexplained": 0, "fields": 0}
    shaped = {}
    for path in files(paths):
        counts["files"] += 1
        with open(path, "rb") as f:
            size = os.fstat(f.fileno()).st_size
            if size == 0:
                continue
            with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as buf:
                counts["bytes"] += size
                for offset, length, label in leaks(buf, index):
                    counts["leaks"] += 1
                    say("FOUND %s offset %d length %d: a run of %s" % (path, offset, length, label))
                for name, offset, length, kind in shapes(buf, index):
                    shaped[(name, kind)] = shaped.get((name, kind), 0) + 1
                    if kind == "unexplained":
                        counts["unexplained"] += 1
                        say("SHAPE %s offset %d length %d: %s none given explains" % (path, offset, length, name))
                counts["fields"] += sum(1 for _ in FIELDS.finditer(buf))
    for (name, kind), n in sorted(shaped.items()):
        say("shapes: %d of %s, %s" % (n, name, kind))
    return counts


def main(argv):
    secrets, public, paths = [], [], []
    args = iter(argv)
    for arg in args:
        if arg in ("--secrets", "--public"):
            value = next(args, None)
            if value is None:
                sys.exit(__doc__)
            (secrets if arg == "--secrets" else public).append(value)
        else:
            paths.append(arg)
    if not secrets or not paths:
        sys.exit(__doc__)
    try:
        index = Index(documents(secrets), documents(public))
        counts = scan(paths, index, print)
    except (OSError, ValueError) as e:
        # Its type and the file it names: never a message that could quote what it read.
        print("leak scan: could not read %s: %s" % (getattr(e, "filename", None) or "a document", type(e).__name__))
        return 2
    print(
        "leak scan: %d secret windows of %d bytes, %d short secrets, %d public values; "
        "%d files (%d bytes) scanned: %d secret runs found, %d unexplained credential shapes, "
        "%d token field names"
        % (
            len(index.windows),
            RUN,
            len(index.short),
            len(index.public),
            counts["files"],
            counts["bytes"],
            counts["leaks"],
            counts["unexplained"],
            counts["fields"],
        )
    )
    if counts["leaks"]:
        return 3
    return 4 if counts["unexplained"] else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
