#!/usr/bin/env python3
"""Focused local guard removals; run only in this lane's private checkout/target."""
import difflib
import hashlib
import json
from pathlib import Path
import subprocess
import sys

out = Path(sys.argv[1]).resolve()
out.mkdir(parents=True, exist_ok=True)
files = {name: Path(f"crates/api/src/{name}.rs") for name in ("borrowed", "channel", "taken", "stream")}
original = {name: path.read_text() for name, path in files.items()}
head = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
(out / "source.json").write_text(json.dumps({"head": head, "sha256": {n: hashlib.sha256(s.encode()).hexdigest() for n, s in original.items()}}, indent=2))


def once(source, old, new):
    if source.count(old) != 1:
        raise RuntimeError(f"expected one exact source guard: {old!r}")
    return source.replace(old, new, 1)


def execute(name, test, ignored, expected, marker=None):
    # Source sync can restore old mtimes. Force our crate to rebuild and retain the
    # actual compilation/output for both mutation and restoration; never touch peers.
    Path("crates/api/src/lib.rs").touch()
    args = ["cargo", "test", "--locked", "-j4", "--profile", "test-release", "-p", "shards-api", "--lib"]
    if test:
        args.append(test)
    args.append("--")
    if test:
        args.append("--exact")
    args.extend(["--test-threads=1", "--nocapture"])
    if ignored:
        args.append("--ignored")
    with (out / f"{name}.log").open("w") as log:
        run = subprocess.run(args, stdout=log, stderr=subprocess.STDOUT, timeout=30)
    content = (out / f"{name}.log").read_text()
    (out / f"{name}.json").write_text(json.dumps({"args": args, "exit": run.returncode, "expected": expected}, indent=2))
    if run.returncode != expected or (marker and marker not in content):
        raise RuntimeError(f"{name}: unexpected result; inspect {out / (name + '.log')}")
    print(f"{name}: exit={run.returncode}, expected={expected}", flush=True)


borrowed_old_budget = original["borrowed"]
start = borrowed_old_budget.index("    if patience.is_zero() {")
end = borrowed_old_budget.index("    #[cfg(unix)]", start)
borrowed_old_budget = borrowed_old_budget[:start] + "    let deadline = began.checked_add(patience).filter(|_| !patience.is_zero());\n" + borrowed_old_budget[end:]
borrowed_old_budget = once(borrowed_old_budget, "Some(deadline), &mut cost", "deadline, &mut cost")
borrowed_old_budget = borrowed_old_budget.replace("if Instant::now() >= deadline {", "if deadline.is_some_and(|at| Instant::now() >= at) {")

post = original["borrowed"]
start = post.index("    held.check().map_err")
end = post.index("    if Instant::now() >= deadline", start)
post = post[:start] + "    // Diagnostic: post-handshake process validation removed.\n" + post[end:]

deadline = original["channel"]
start = deadline.index("fn remaining(")
end = deadline.index("pub(crate) fn handshake", start)
deadline = deadline[:start] + "fn remaining(_deadline: Option<Instant>, _phase: HandshakePhase) -> Result<Option<Duration>, HandshakeError> { Ok(None) }\n\n" + deadline[end:]

cases = [
    ("zero", "borrowed", borrowed_old_budget, "borrowed::tests::zero_with_withheld_muxer_never_dials", False, "actual_CONNECT=true"),
    ("overflow", "borrowed", borrowed_old_budget, "borrowed::tests::overflow_with_withheld_muxer_never_dials", False, "actual_CONNECT=true"),
    ("identity", "taken", once(original["taken"], "if found != expected {", "if false {"), "borrowed::tests::changed_identity_is_rejected_before_dial", False, "stale_identity_actual_CONNECT=true"),
    ("post-exit", "borrowed", post, "borrowed::tests::process_exit_before_or_during_handshake_is_rejected", False, "actual_process_reaped_before_OK=true returned_stream=true"),
    ("total-deadline", "channel", deadline, "client::borrowed_repro::a_current_handshake_dribble_cannot_renew_the_total_budget", True, "accepted_late_reply=true"),
    ("allocation", "stream", once(original["stream"], "triomphe::Arc::try_new(Shared::default()).map_err(|_| io::ErrorKind::OutOfMemory.into())", "Ok(triomphe::Arc::new(Shared::default()))"), "borrowed::tests::shared_state_and_stream_box_oom_return_without_allocating_errors", False, "memory allocation of"),
]

try:
    for name, module, changed, test, ignored, marker in cases:
        path = files[module]
        (out / f"{name}.patch").write_text("".join(difflib.unified_diff(original[module].splitlines(True), changed.splitlines(True), fromfile=str(path), tofile=str(path))))
        path.write_text(changed)
        try:
            execute(name, test, ignored, 101, marker)
        finally:
            path.write_text(original[module])
        execute(name + "-restored", test, ignored, 0)
finally:
    for name, path in files.items():
        path.write_text(original[name])
    Path("crates/api/src/lib.rs").touch()
    (out / "restored.sha256").write_text("\n".join(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path}" for path in files.values()) + "\n")

execute("restored-corpus", "", False, 0)
