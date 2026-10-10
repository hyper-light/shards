//! How long shards takes to compile Docker's default seccomp profile for a run, by guest
//! architecture (docs/research/platform-measurements.md M167): the profile with Docker's
//! default capabilities, for Linux 6.18, as the daemon compiles it for a run's command
//! (shards/src/setup.rs `seccomp`) and, in its variants, for an Agentfile's domains
//! (`domain_seccomp`). Each sample is one whole compile, from the profile's JSON to the
//! program; the architectures alternate a sample at a time.
//!
//!     cargo run --release -- [N]
use std::time::{Duration, Instant};

use shards_seccomp::{Arch, Container, DEFAULT, Kernel};

/// Docker's default capabilities (moby oci/caps/defaults.go), as the daemon names them.
const CAPS: [&str; 14] = [
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_FSETID",
    "CAP_FOWNER",
    "CAP_MKNOD",
    "CAP_NET_RAW",
    "CAP_SETGID",
    "CAP_SETUID",
    "CAP_SETFCAP",
    "CAP_SETPCAP",
    "CAP_NET_BIND_SERVICE",
    "CAP_SYS_CHROOT",
    "CAP_KILL",
    "CAP_AUDIT_WRITE",
];

fn pct(xs: &[Duration], p: usize) -> Duration {
    let mut v = xs.to_vec();
    v.sort();
    v[(p * v.len() / 100).min(v.len() - 1)]
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .map_or(100, |a| a.parse().expect("N, a count"));
    let caps: Vec<String> = CAPS.iter().map(|c| c.to_string()).collect();
    let arches = [Arch::Amd64, Arch::Arm64];
    let mut times: [Vec<Duration>; 2] = [Vec::new(), Vec::new()];
    let mut insns = [0usize; 2];
    for i in 0..n {
        for j in 0..2 {
            // Which goes first alternates too.
            let k = (i + j) % 2;
            let c = Container {
                arch: arches[k],
                caps: &caps,
                kernel: Kernel(6, 18),
            };
            let start = Instant::now();
            let program = shards_seccomp::compile(DEFAULT, &c)
                .expect("Docker's default profile compiles")
                .expect("and asks for a filter");
            times[k].push(start.elapsed());
            insns[k] = program.insns.len();
        }
    }
    for k in 0..2 {
        let t = &times[k];
        println!(
            "{:<5} n={} p50={:?} p90={:?} p99={:?} max={:?} insns={}",
            arches[k].go_name(),
            t.len(),
            pct(t, 50),
            pct(t, 90),
            pct(t, 99),
            t.iter().max().copied().unwrap_or_default(),
            insns[k]
        );
    }
}
