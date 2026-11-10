"""Replay the unchanged raw-Child/read_line fixture against controlled Linux children.

The generated Rust probe embeds the actual Client/ready/finished source, not a
second implementation. Only common::command's executable factory is substituted.
Caller patience is an observation/rescue bound; the old ready function has none.
"""
import argparse
import hashlib
import json
import os
import pathlib
import selectors
import signal
import subprocess
import sys
import time

parser = argparse.ArgumentParser()
parser.add_argument("--patience-ms", type=int, required=True)
parser.add_argument("--evidence", type=pathlib.Path, required=True)
args = parser.parse_args()
if sys.platform != "linux" or args.patience_ms <= 0:
    raise SystemExit("requires Linux pidfds and positive caller patience")
args.evidence.mkdir(parents=True, exist_ok=False)
root = pathlib.Path(__file__).resolve().parents[4]
host = root / "crates/shards/tests/managed_cleanup.rs"
text = host.read_text()
actual = text[text.index("struct Client {"):text.index("struct Daemon<'a>")]
(args.evidence / "actual-client.rs").write_text(actual)
prefix = r'''
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdout};
fn kernel() -> &'static Path { Path::new("/dev/null") }
fn guest_init() -> &'static Path { Path::new("/dev/null") }
mod common {
    use std::process::Command;
    pub fn command() -> Command {
        let mut command = Command::new("python3");
        command.arg(std::env::var_os("CLEANUP_CHILD_HELPER").unwrap());
        command
    }
    pub fn interruptible(command: &mut Command) -> &mut Command { command }
}
use std::process::Stdio;
'''
suffix = r'''
unsafe extern "C" { fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32; }
fn main() {
    let mode = std::env::args().nth(1).unwrap();
    match std::panic::catch_unwind(|| ready(Path::new("."), &[&mode])) {
        Ok(mut client) => {
            println!("CLIENT_READY");
            std::io::stdout().flush().unwrap();
            let mut control = String::new();
            std::io::stdin().read_line(&mut control).unwrap();
            println!("CLIENT_FINISHED {:?}", finished(&mut client));
        }
        Err(_) => {
            println!("READINESS_PANIC");
            std::io::stdout().flush().unwrap();
            let mut control = String::new();
            std::io::stdin().read_line(&mut control).unwrap();
            let pid: i32 = control.trim().parse().unwrap();
            let mut status = 0;
            // SAFETY: the controller supplies only the controlled direct child's PID;
            // the unmodified ready path dropped its handle without waiting for it.
            let waited = unsafe { waitpid(pid, &mut status, 0) };
            assert_eq!(waited, pid);
            println!("RESCUED_OWNED_CHILD {waited} status={status}");
        }
    }
}
'''
source = args.evidence / "probe.rs"
source.write_text(prefix + actual + suffix)
binary = args.evidence / "probe"
build = subprocess.run(["rustc", "--edition=2024", str(source), "-o", str(binary)], capture_output=True)
(args.evidence / "build.log").write_bytes(build.stdout + build.stderr)
(args.evidence / "build.exit").write_text(str(build.returncode) + "\n")
if build.returncode:
    raise SystemExit(build.returncode)
env = dict(os.environ, CLEANUP_CHILD_HELPER=str(pathlib.Path(__file__).with_name("readiness-child.py").resolve()))
patience = args.patience_ms / 1000
samples = []
for mode in ["silent", "early", "partial", "normal"]:
    probe = subprocess.Popen([str(binary), mode], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    selector = selectors.DefaultSelector()
    selector.register(probe.stdout, selectors.EVENT_READ, "stdout")
    selector.register(probe.stderr, selectors.EVENT_READ, "stderr")
    probe_fd = os.pidfd_open(probe.pid)
    selector.register(probe_fd, selectors.EVENT_READ, "exit")
    buffers = {"stdout": b"", "stderr": b""}
    lines = []
    helper_fd = None
    helper_pid = None
    deadline = time.monotonic() + patience
    timed_out = False
    rescued = False
    try:
        while probe.returncode is None:
            events = selector.select(max(0, deadline - time.monotonic()))
            if not events:
                if timed_out or helper_fd is None:
                    raise TimeoutError(f"{mode}: controlled rescue did not complete")
                timed_out = True
                signal.pidfd_send_signal(helper_fd, signal.SIGKILL)
                deadline = time.monotonic() + patience
                continue
            for key, _ in events:
                if key.data == "exit":
                    probe.wait()
                    continue
                chunk = os.read(key.fileobj.fileno(), 4096)
                if not chunk:
                    selector.unregister(key.fileobj)
                    continue
                buffers[key.data] += chunk
                while b"\n" in buffers[key.data]:
                    line, buffers[key.data] = buffers[key.data].split(b"\n", 1)
                    line = line.decode(errors="replace")
                    lines.append([key.data, line])
                    if line.startswith("CLEANUP_CHILD_PID "):
                        helper_pid = int(line.split()[1])
                        helper_fd = os.pidfd_open(helper_pid)
                    elif line == "READINESS_PANIC":
                        state = pathlib.Path(f"/proc/{helper_pid}/status").read_text().split("State:", 1)[1].splitlines()[0].strip()
                        lines.append(["observer", "owned_child_state=" + state])
                        probe.stdin.write(f"{helper_pid}\n".encode())
                        probe.stdin.flush()
                        rescued = True
                    elif line == "CLIENT_READY":
                        signal.pidfd_send_signal(helper_fd, signal.SIGUSR1)
                        probe.stdin.write(b"finish\n")
                        probe.stdin.flush()
        sample = {"mode": mode, "caller_patience_ms": args.patience_ms, "deadline_hit": timed_out,
                  "unjoined_child_rescued": rescued, "probe_exit": probe.returncode, "lines": lines}
        samples.append(sample)
        (args.evidence / (mode + ".json")).write_text(json.dumps(sample, indent=2) + "\n")
        print(json.dumps(sample), flush=True)
    finally:
        if probe.returncode is None:
            if helper_fd is not None:
                try:
                    signal.pidfd_send_signal(helper_fd, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            probe.kill()
            probe.wait()
        if helper_fd is not None:
            os.close(helper_fd)
        os.close(probe_fd)
        selector.close()
        for stream in [probe.stdin, probe.stdout, probe.stderr]:
            stream.close()
summary = {"host_source_sha256": hashlib.sha256(host.read_bytes()).hexdigest(),
           "actual_client_sha256": hashlib.sha256(actual.encode()).hexdigest(), "samples": samples}
(args.evidence / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
raise SystemExit(1 if any(s["deadline_hit"] or s["unjoined_child_rescued"] for s in samples) else 0)
