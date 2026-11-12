"""A signed-in harness's credential, handled without ever printing it (harness bring-up v2).

    credentials.py validity SRC                  how long SRC's access token stays valid
    credentials.py copy SRC PRIVATE NAME FILE   SRC's private copy, PRIVATE/NAME/FILE
    credentials.py leakcheck PRIVATE PATH...     whether any secret of the originals or
                                                 copies is in the files under PATH
    credentials.py settle PRIVATE                each copy against its original: unchanged,
                                                 or written back where the harness
                                                 refreshed it and the original did not change
    credentials.py scrub PRIVATE                 overwrite and delete PRIVATE

PRIVATE is a fresh 0700 directory the caller made outside any synced repository (a tmpfs
where the machine has one); PRIVATE/NAME is what the harness is given, PRIVATE/state.json,
which the harness never sees, what was copied: each original's path, its SHA-256 then, its
mode. Every output line is a status, a count or a path: no value, no hash.
"""

import base64
import calendar
import hashlib
import json
import os
import re
import stat
import sys
import tempfile
import time

# A secret is a string a credential file holds under one of these names (Codex's
# auth.json: OPENAI_API_KEY, tokens.{access,id,refresh}_token, tokens.account_id; Claude
# Code's .credentials.json: claudeAiOauth.{access,refresh}Token).
SECRET_NAME = re.compile(r"token|key|secret|account_id", re.IGNORECASE)
# A file is a leak where it holds a whole secret, or any run of WINDOW + STRIDE - 1 = 31 or
# more consecutive bytes of one (a log can cut a token): every WINDOW-byte window of each
# secret is kept, and the file's windows are looked up at every STRIDE-th offset, of which
# one falls in such a run's first STRIDE bytes. JWTs of one issuer share their header, so a
# leak is reported by path for a person to look at, never by value.
WINDOW = 24
STRIDE = 8


def state_path(private):
    return os.path.join(private, "state.json")


def load_state(private):
    try:
        with open(state_path(private)) as f:
            return json.load(f)
    except FileNotFoundError:
        return {}


def save_state(private, state):
    fd = os.open(state_path(private) + ".new", os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump(state, f)
    os.replace(state_path(private) + ".new", state_path(private))


def secrets(value, name=""):
    if isinstance(value, dict):
        for key, item in value.items():
            yield from secrets(item, key)
    elif isinstance(value, list):
        for item in value:
            yield from secrets(item, name)
    elif isinstance(value, str) and SECRET_NAME.search(name) and len(value) >= 16:
        yield value


def validity(document):
    """How long the document's access token stays valid, and for Codex how long ago its
    tokens were last refreshed, in whole minutes and days: times, never a value. A harness
    that finds its token about to expire refreshes it, and a refresh can retire the
    refresh token every other holder of the original still has. "unknown" where it cannot
    tell (never an exception, whose message could quote a token)."""
    try:
        return _validity(document)
    except Exception:
        return "its access token's expiry: unknown"


def _validity(document):
    now = time.time()
    expires = None
    oauth = document.get("claudeAiOauth")
    if isinstance(oauth, dict) and isinstance(oauth.get("expiresAt"), (int, float)):
        expires = oauth["expiresAt"] / 1000
    tokens = document.get("tokens")
    access = tokens.get("access_token") if isinstance(tokens, dict) else None
    if expires is None and isinstance(access, str) and access.count(".") == 2:
        payload = access.split(".")[1]
        claims = json.loads(base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4)))
        if isinstance(claims.get("exp"), (int, float)):
            expires = claims["exp"]
    if expires is None:
        said = "its access token's expiry: unknown"
    elif expires >= now:
        said = f"its access token is valid for {int((expires - now) // 60)} more minutes"
    else:
        said = f"its access token expired {int((now - expires) // 60)} minutes ago"
    refreshed = document.get("last_refresh")
    if isinstance(refreshed, str):
        # RFC 3339 in UTC, as Codex writes it; the seconds are enough.
        at = calendar.timegm(time.strptime(refreshed[:19], "%Y-%m-%dT%H:%M:%S"))
        said += f"; its tokens were last refreshed {int((now - at) // 86400)} days ago"
    return said


def copy(src, private, name, file):
    if stat.S_IMODE(os.stat(private).st_mode) != 0o700:
        sys.exit(f"{private} is not 0700")
    shared = os.path.join(private, name)
    os.mkdir(shared, 0o700)
    target = os.path.join(shared, file)
    with open(src, "rb") as f:
        data = f.read()
    document = json.loads(data)
    fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as f:
        f.write(data)
    state = load_state(private)
    state[name] = {
        "src": os.path.abspath(src),
        "copy": target,
        "sha256": hashlib.sha256(data).hexdigest(),
        "mode": stat.S_IMODE(os.stat(src).st_mode),
    }
    save_state(private, state)
    print(f"{name}: copied to a 0600 file in a 0700 directory; {validity(document)}")


def all_secrets(state):
    found = set()
    for entry in state.values():
        for path in (entry["src"], entry["copy"]):
            try:
                with open(path, "rb") as f:
                    found.update(secrets(json.loads(f.read())))
            except (FileNotFoundError, ValueError):
                pass
    windows, whole = set(), set()
    for value in found:
        data = value.encode()
        whole.add(data)
        windows.update(data[i : i + WINDOW] for i in range(len(data) - WINDOW + 1))
    return found, windows, whole


def leaked(data, windows, whole):
    if any(value in data for value in whole):
        return True
    return any(data[i : i + WINDOW] in windows for i in range(0, len(data) - WINDOW + 1, STRIDE))


def leakcheck(private, paths):
    state = load_state(private)
    found, windows, whole = all_secrets(state)
    files = scanned = 0
    leaks = []
    for root in paths:
        for directory, dirs, names in os.walk(root):
            dirs[:] = [d for d in dirs if os.path.realpath(os.path.join(directory, d)) != os.path.realpath(private)]
            for leaf in names:
                path = os.path.join(directory, leaf)
                if not os.path.isfile(path) or os.path.islink(path):
                    continue
                with open(path, "rb") as f:
                    data = f.read()
                files += 1
                scanned += len(data)
                if leaked(data, windows, whole):
                    leaks.append(path)
    print(f"leak check: {len(found)} secret values of {len(state)} credentials, "
          f"{files} files ({scanned} bytes) scanned: " + ("FOUND in " + ", ".join(leaks) if leaks else "not found"))
    return 3 if leaks else 0


def tokens_of(document):
    return sorted(secrets(document))


def write_back(src, data, mode):
    if os.stat(src).st_uid != os.getuid():
        raise PermissionError(f"{src} is not this user's")
    directory = os.path.dirname(src)
    fd, tmp = tempfile.mkstemp(dir=directory, prefix=".shards-writeback-")
    try:
        os.fchmod(fd, mode)
        with os.fdopen(fd, "wb") as f:
            f.write(data)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, src)
    except BaseException:
        if os.path.exists(tmp):
            os.unlink(tmp)
        raise
    dfd = os.open(directory, os.O_DIRECTORY)
    try:
        os.fsync(dfd)
    finally:
        os.close(dfd)


def settle(private):
    state = load_state(private)
    for name, entry in sorted(state.items()):
        with open(entry["copy"], "rb") as f:
            data = f.read()
        if hashlib.sha256(data).hexdigest() == entry["sha256"]:
            print(f"{name}: the copy is unchanged: no refresh, nothing to write back")
            continue
        try:
            changed = json.loads(data)
        except ValueError:
            print(f"{name}: the copy changed but is not JSON: not written back")
            continue
        with open(entry["src"], "rb") as f:
            original = f.read()
        refreshed = tokens_of(changed) != tokens_of(json.loads(original))
        if hashlib.sha256(original).hexdigest() != entry["sha256"]:
            print(f"{name}: CONFLICT: the original changed on the host during the run, and the "
                  f"copy too (its tokens {'changed' if refreshed else 'did not change'}): not written back")
            continue
        write_back(entry["src"], data, entry["mode"])
        print(f"{name}: the copy changed in the microVM (its tokens {'changed: a refresh' if refreshed else 'did not change'}): "
              "written back over the original atomically, its mode kept")


def scrub(private):
    # What the harness made in its share may have no write or search bit for its owner.
    for directory, dirs, names in os.walk(private):
        os.chmod(directory, 0o700)
        for leaf in dirs:
            os.chmod(os.path.join(directory, leaf), 0o700)
        for leaf in names:
            path = os.path.join(directory, leaf)
            if os.path.isfile(path) and not os.path.islink(path):
                os.chmod(path, 0o600)
    count = 0
    for directory, dirs, names in os.walk(private, topdown=False):
        for leaf in names:
            path = os.path.join(directory, leaf)
            if os.path.isfile(path) and not os.path.islink(path):
                size = os.path.getsize(path)
                with open(path, "r+b") as f:
                    f.write(b"\0" * size)
                    f.flush()
                    os.fsync(f.fileno())
            os.unlink(path)
            count += 1
        for leaf in dirs:
            os.rmdir(os.path.join(directory, leaf))
    os.rmdir(private)
    print(f"scrubbed: {count} files overwritten and removed, {private} removed")


def main(argv):
    if len(argv) == 2 and argv[0] == "validity":
        with open(argv[1], "rb") as f:
            print(f"{argv[1]}: {validity(json.loads(f.read()))}")
        return 0
    if len(argv) == 5 and argv[0] == "copy":
        copy(*argv[1:5])
        return 0
    if len(argv) >= 3 and argv[0] == "leakcheck":
        return leakcheck(argv[1], argv[2:])
    if len(argv) == 2 and argv[0] == "settle":
        settle(argv[1])
        return 0
    if len(argv) == 2 and argv[0] == "scrub":
        scrub(argv[1])
        return 0
    sys.exit(__doc__)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
