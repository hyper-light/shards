#!/usr/bin/env python3
"""Guard-removal mutations of audit-fix-server-v1 (R09/R10): each takes away one fix's guard
from crates/server/src, by exact text replacement that must match as many times as given.

  mutate.py NAME     apply mutation NAME, print the file it changed
  mutate.py --file NAME   print the file NAME changes, change nothing

Run from the repository root. mutations.sh backs the file up, applies one, runs the tests
that must fail for that guard, and restores the file.
"""
import pathlib
import sys

SERVER = "crates/server/src/server.rs"
BROKER = "crates/server/src/broker.rs"
SANDBOX = "crates/server/src/sandbox.rs"

# name: (file, [(old, new, times old must occur)])
MUTATIONS = {
    # 1: a deferred program answer closes its connection at once again (the request's close).
    "M1a": (SERVER, [(
        "                    c.waiting = Some((id, after));\n",
        "                    c.waiting = Some((id, after));\n"
        "                    if close {\n"
        "                        c.closing = true;\n"
        "                        c.tls.send_close_notify();\n"
        "                    }\n",
        1,
    )]),
    # 1: the close a request asked for no longer follows its program's answer.
    "M1b": (SERVER, [(
        "if let Some((_, AfterAnswer::Close)) = c.waiting.take() {",
        "if let Some((_, AfterAnswer::Close)) = c.waiting.take()\n                    && false\n                {",
        1,
    )]),
    # 2: a STOP the full socket refused is dropped, as before.
    "M2": (SERVER, [(
        "                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,\n",
        "",
        1,
    )]),
    # 3: a refusal without storage says the executed answer's "uncertain" again.
    "M3": (BROKER, [(
        "Err(_) => self.refusal_literal(),",
        "Err(_) => self.literal(),",
        2,
    )]),
    # 4: a hand sizes its read to the peer's limit, whatever the file holds.
    "M4": (BROKER, [(
        "let mut end = room(usize::try_from(meta.len()).map_err(|_| failed(Reason::TooLong))?)?;",
        "let mut end = full;",
        1,
    )]),
    # 4: a file longer than its size said is refused, not read on within the limit.
    "M4b": (BROKER, [(
        "                end = full;\n                reserve(packet, end)?;\n",
        "                return Err(failed(Reason::TooLong));\n",
        1,
    )]),
    # 5: what a broker tool throws carries no receipt.
    "M5a": (SANDBOX, [(
        "throw r.broker === undefined ? new Error(r.error) : new Error(r.error, { cause: r.broker });",
        "throw new Error(r.error);",
        1,
    )]),
    # 5: what a broker tool returns carries no receipt.
    "M5b": (SANDBOX, [(
        '      Object.defineProperty(r.value, "broker", { value: r.broker });\n',
        "",
        1,
    )]),
    # 6: one worker waitid gives no status of ends the helper, as before.
    "M6": (SANDBOX, [(
        "                let (code, status) = match w.reap(0) {\n"
        "                    Ok(Some(ended)) => ended,\n"
        "                    Ok(None) => (END_UNKNOWN, 0),\n"
        "                    Err(e) => (END_UNKNOWN, e.raw_os_error().unwrap_or(0)),\n"
        "                };\n",
        "                let (code, status) = w\n"
        "                    .reap(0)\n"
        "                    .map_err(|e| HelperCause::io(\"reaping a worker\", e))?\n"
        "                    .ok_or(HelperCause::Control)?;\n",
        1,
    )]),
}


def main() -> int:
    args = sys.argv[1:]
    if len(args) == 2 and args[0] == "--file" and args[1] in MUTATIONS:
        print(MUTATIONS[args[1]][0])
        return 0
    if len(args) != 1 or args[0] not in MUTATIONS:
        print(f"usage: mutate.py [--file] {{{','.join(MUTATIONS)}}}", file=sys.stderr)
        return 2
    path, edits = MUTATIONS[args[0]]
    p = pathlib.Path(path)
    text = p.read_text()
    for old, new, times in edits:
        found = text.count(old)
        if found != times:
            print(f"{args[0]}: {old!r} found {found} times in {path}, not {times}", file=sys.stderr)
            return 1
        text = text.replace(old, new)
    p.write_text(text)
    print(path)
    return 0


if __name__ == "__main__":
    sys.exit(main())
