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
3. The identifiers are their tables' next free values: marker 7 above shards_abi::marker,
   run kind 49 above shards_abi::run::kind, ipc kind 90 above shards_ipc's kinds.

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
    ("nesting", "the setup words `nesting`"),
    ("ready-by=", "`ready-by=`"),
    ("gpus=", "`gpus=` are shards' own"),
    ("NOTIFY_SOCKET", "`NOTIFY_SOCKET`"),
    ("READY=1", "`READY=1` are sd_notify's"),
    ("SHARDS_READY_BY", "`SHARDS_READY_BY` takes the place of"),
    ("CAP_SYS_ADMIN (21)", "CAP_SYS_ADMIN (21)"),
    ("CAP_NET_ADMIN (12)", "CAP_NET_ADMIN (12)"),
    ("the controllers", "The controllers the workload may enable"),
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
    checks = [
        ("marker 7", 7, taken, max(taken) + 1),
        ("run kind 49", 49, run_kinds, max(run_kinds) + 1),
        ("ipc kind 90", 90, ipc_kinds, max(k for k in ipc_kinds if k < 128) + 1),
    ]
    for name, value, used, next_free in checks:
        if value in used or value != next_free:
            failures += 1
            print(f"TAKEN    {name}: next free is {next_free}")
        else:
            print(f"free     {name}: the next free value")

    print(f"planned {len(PLANNED)}, underived or unplanned {failures}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
