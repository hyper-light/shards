"""Runs one of terminal_order_tests.rs's native fixtures as ROOT's managed-terminal-order
caller does, in a fresh private directory: the case's finite deadline, and for `file-limit`
a soft and hard RLIMIT_FSIZE of the given strict prefix with SIGXFSZ ignored, both inherited
across exec by the test binary alone. Prints the metadata file's bytes afterwards.

    python3 -I terminal-fixture.py BINARY TEST DIRECTORY CASE_MS [PREFIX_BYTES]
"""

import os
import resource
import signal
import subprocess
import sys


def main():
    binary, test, directory, case_ms = sys.argv[1:5]
    prefix = int(sys.argv[5]) if len(sys.argv) > 5 else None
    os.mkdir(directory, 0o700)
    env = {k: v for k, v in os.environ.items() if not k.startswith(("DYLD_", "SHARDS_TERMINAL_ORDER_"))}
    env["SHARDS_TERMINAL_ORDER_DIRECTORY"] = directory
    env["SHARDS_TERMINAL_ORDER_CASE_MS"] = case_ms
    limit = None
    if prefix is not None:
        env["SHARDS_TERMINAL_ORDER_PREFIX_BYTES"] = str(prefix)

        def limit():
            signal.signal(signal.SIGXFSZ, signal.SIG_IGN)
            resource.setrlimit(resource.RLIMIT_FSIZE, (prefix, prefix))

    argv = [binary, "--exact", test, "--ignored", "--nocapture", "--test-threads=1"]
    print("argv", argv, "prefix", prefix, flush=True)
    # Its output through a pipe, which no file size limit reaches, as ROOT's caller reads it.
    done = subprocess.run(
        argv,
        env=env,
        preexec_fn=limit,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=int(case_ms) / 1000 * 4,
    )
    sys.stdout.write(done.stdout.decode(errors="replace"))
    print("exit", done.returncode, flush=True)
    path = os.path.join(directory, "metadata")
    with open(path, "rb") as f:
        print("metadata bytes", f.read(), flush=True)
    return done.returncode


if __name__ == "__main__":
    sys.exit(main())
