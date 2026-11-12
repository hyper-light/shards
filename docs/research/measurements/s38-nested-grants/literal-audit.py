#!/usr/bin/env python3
"""D234's literal audit (R03): every literal the nesting grant adds has its derivation in
D234's own **Literals.** list, none is left underived, and the identifiers D234 takes are
still free in their tables.

    python3 docs/research/measurements/s38-nested-grants/literal-audit.py

Three checks, each against this checkout:
1. Each planned literal (PLANNED, the names, paths, modes, bounds and identifiers D234's
   **Decided.** and **Wire.** sections give the grant) opens a bullet of **Literals.**, and
   that bullet goes on to say where it comes from (a clause after it).
2. No backticked token of **Wire.**, or of the grant's own bullets, is a literal missing
   from PLANNED: each is planned, or an existing name another D-entry owns (EXISTING).
3. The identifiers are their tables' next free values (marker 7 above shards_abi::marker,
   run kind 49 above shards_abi::run::kind, ipc kind 90 above shards_ipc's kinds), or,
   once their step has landed, held by the grant's own constant and nothing else.

Prints each literal's verdict and the count underived; exits 1 if any check fails.
"""

from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[4]

# (literal, the text that must open its derivation in **Literals.**)
PLANNED = [
    ("init", "The leaf's name, `init`"),
    ("PIPE_BUF", "The notify datagram's bound, PIPE_BUF"),
    ("the deadline", "The deadline: the caller's"),
    ("fs.nr_open", "`fs.nr_open`: the run's own ask"),
    ("the GPUs' identities", "The GPUs' identities: as many as the run names"),
    ("WORKLOAD_READY = 7", "`WORKLOAD_READY` = 7"),
    ("run kind 49", "`shards_abi::run::kind` 49"),
    ("ipc kind 90", "`shards_ipc` kind 90"),
    ("/run/shards/notify", "`/run/shards/notify`"),
    ("/run/shards/gpus", "`/run/shards/gpus`"),
    ("0444", "The identities file's mode, 0444"),
    ("NESTING", "The names: `NESTING`"),
    ("nesting=true", "`nesting=true`"),
    ("nesting", "`nesting` the grant"),
    ("ready-by=", "`ready-by=`"),
    ("gpus=", "`gpus=` the GPUs"),
    ("NOTIFY_SOCKET", "`NOTIFY_SOCKET`"),
    ("READY=1", "`READY=1` are sd_notify's"),
    ("SHARDS_READY_BY", "`SHARDS_READY_BY` takes the place of"),
    ("CAP_SYS_ADMIN (21)", "CAP_SYS_ADMIN (21)"),
    ("CAP_NET_ADMIN (12)", "CAP_NET_ADMIN (12)"),
    ("the controllers", "The controllers the workload may enable"),
    ("notify socket mode 0666", "The notify socket's mode, 0666"),
    ("READY's 9-byte payload", "READY's payload, 9 bytes"),
    ("init's `notify` entry and tag `N`", "Init's own names"),
    ("NR_OPEN_MAX 2147483584", "`NR_OPEN_MAX`, 2147483584"),
    ("ready_by", "`ready_by` is shards-api's field"),
    ("the run's events", "The run's events `ready` and"),
    ("/run/shards 0755", "`/run/shards`'s mode, 0755"),
    ("credentials buffer 32", "The credentials buffer, 32 bytes"),
    ("notify queue", "The notify queue"),
]

# Backticked names in D234 that are no literal of its own: each is another entry's.
EXISTING = {
    "cgroups-rw": "D42's writable-cgroups entry",
    "seccomp=": "D42's filter entry",
    "seccomp-mounts=": "D121's mount filter entry",
    "shards_abi::gpu": "D232's identity grammar",
    "--gpu-device": "D232's flag",
    "gpu_devices": "D232's shards-api field",
    "--gpus": "D144's flag",
    "--device": "D44's flag",
    "--tmpfs /run": "a run's own flag (D115's table)",
    "-v": "a run's own flag (D109)",
    "--network container:": "D119's joiner",
    "--allow": "the build's entitlement flag (AGENTFILE_ARCH.md)",
    "RUN --security=insecure": "BuildKit's step option",
    "HOST_TIME": "the control page's register (crates/abi)",
    "/run/shards/engine.sock": "D181's socket",
    "run::MAX_PAYLOAD": "crates/abi's payload bound",
    "MAX_PHYSICAL_GPUS": "D232's bound",
    "sysctl_nr_open_max": "the kernel's ceiling (fs/file.c)",
    "--ulimit\n    nofile": "a run's own flag",
    "--ulimit nofile": "a run's own flag",
    "run/workload": "init's existing workload cgroup (D148)",
    "run/workload/init": "the leaf, planned as `init`",
    "nesting": "planned",
    "ready-by=<ns>": "planned as `ready-by=`",
    "gpus=<identity>[,<identity>…]": "planned as `gpus=`",
    "run::kind": "planned as run kind 49",
    "NESTING": "planned",
    "nesting=true": "planned",
    "--security-opt nesting=true": "planned as `nesting=true`",
    "SHARDS_READY_BY": "planned",
    "NOTIFY_SOCKET=/run/shards/notify": "planned as NOTIFY_SOCKET and /run/shards/notify",
    "READY=1": "planned",
    "/run/shards/gpus": "planned",
    "pivot_root": "a syscall's name (the filter's addition, f16)",
    "bpf": "a syscall's name",
    "perf_event_open": "a syscall's name",
    "AGENT": "the Agentfile's directive",
    "HARNESS": "the Agentfile's directive",
    "run": "the CLI's command",
    "create": "the CLI's command",
    "/proc": "the workload's existing mount (D115)",
    "/sys": "the workload's existing mount (D115)",
    "/dev": "the workload's existing mount (D44)",
    "/run/shards": "D181's directory, engine.sock's",
    "security.insecure": "BuildKit's entitlement (f32)",
    "security.nesting": "Incus's option (f32)",
    "socket": "Docker's profile's rule name (moby/profiles v0.2.3)",
    "--cgroupns host": "D156's flag, refused with the grant",
    "--pid host": "D115's flag, refused with the grant",
    "ready_by": "planned",
    "SHARDS_READY_BY=0": "planned (cold start, with SHARDS_READY_BY)",
    "WORKLOAD_READY": "planned (marker 7; ipc kind 90's name)",
    "ready": "planned (the run's event)",
    "ready-by-passed": "planned (the run's event)",
    "writable-cgroups=true": "D42's security option, the key's form",
}


def section(text, start, end):
    i = text.index(start)
    j = text.index(end, i + len(start))
    return text[i:j]


def main():
    arch = (ROOT / "docs/design/architecture.md").read_text(encoding="utf-8")
    d = section(arch, "### D234.", "\n### ")
    literals = section(d, "**Literals.**", "**Alternatives")
    failures = 0

    bullets = re.split(r"\n- ", literals)
    for name, opening in PLANNED:
        bullet = next((b for b in bullets if opening in b), None)
        # Its derivation: the words its bullet gives after naming it, at least a clause.
        rest = "" if bullet is None else bullet[bullet.index(opening) + len(opening):]
        derived = len(re.findall(r"[A-Za-z0-9']+", rest)) >= 3
        if not derived:
            failures += 1
        print(f"{'derived  ' if derived else 'UNDERIVED'} {name}")

    wire = section(d, "**Wire.**", "**Readiness**")
    decided = section(d, "**Decided.**", "**Wire.**")
    planned_names = {name for name, _ in PLANNED}
    for token in sorted(set(re.findall(r"`([^`]+)`", wire + decided))):
        if token in planned_names or token in EXISTING:
            continue
        failures += 1
        print(f"UNPLANNED `{token}`: in D234's grant text, neither planned nor another entry's")

    abi = (ROOT / "crates/abi/src/lib.rs").read_text(encoding="utf-8")
    markers = section(abi, "pub mod marker", "\n}")
    taken = {int(n) for n in re.findall(r"pub const \w+: u32 = (\d+);", markers)}
    run_rs = (ROOT / "crates/abi/src/run.rs").read_text(encoding="utf-8")
    kinds = section(run_rs, "pub mod kind", "\n}")
    run_kinds = {int(n) for n in re.findall(r"pub const \w+: u8 = (\d+);", kinds)}
    ipc = (ROOT / "crates/ipc/src/lib.rs").read_text(encoding="utf-8")
    ipc_kinds = {int(n) for n in re.findall(r"pub const [A-Z_0-9]+: u8 = (\d+);", ipc)}
    # Each identifier is free, its table's next, or already the grant's own (its constant
    # named as D234 names it, once its step has landed), and taken by nothing else.
    def holders(text, ty, value):
        return re.findall(rf"pub const (\w+): {ty} = {value};", text)
    checks = [
        ("marker 7", 7, markers, "u32", taken, r"^WORKLOAD_READY$"),
        ("run kind 49", 49, kinds, "u8", run_kinds, r"^READY$"),
        ("ipc kind 90", 90, ipc, "u8", ipc_kinds, r"^WORKLOAD_READY$"),
    ]
    for name, value, text, ty, used, own in checks:
        held = holders(text, ty, value)
        if held and all(re.search(own, h) for h in held):
            print(f"own      {name}: held by the grant's {', '.join(held)}")
            continue
        others = sorted(k for k in used if k < 128)
        next_free = max(k for k in others if k != value) + 1 if others else value
        if held or value != next_free:
            failures += 1
            print(f"TAKEN    {name}: held by {held or 'nothing'}; next free is {next_free}")
        else:
            print(f"free     {name}: the next free value")

    print(f"planned {len(PLANNED)}, underived or unplanned {failures}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
