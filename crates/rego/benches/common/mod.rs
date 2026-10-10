//! What the rego benches share: buildx's functions, a host that answers none of them,
//! nearest-rank percentiles, the machine's stamp and the process's peak memory.

#![allow(dead_code)]

use std::process::Command;

use shards_rego::ast::Term;
use shards_rego::compile::Function;
use shards_rego::eval::{Host, HostError};
use shards_rego::types::Type;
use shards_rego::value::Value;

pub struct NoHost;

impl Host for NoHost {
    fn call(&mut self, _: &str, _: &[Value]) -> Result<Option<Value>, HostError> {
        Ok(None)
    }
}

/// buildx's functions (policy/funcs.go), as shards declares them.
pub fn functions() -> Vec<Function> {
    let s = || Type::String;
    let a = || Type::Any(Vec::new());
    let f = |args: Vec<Type>, result: Type| Type::Function {
        args,
        result: Some(Box::new(result)),
        variadic: None,
    };
    vec![
        Function {
            name: "load_json".into(),
            decl: f(vec![s()], a()),
        },
        Function {
            name: "verify_git_signature".into(),
            decl: f(vec![a(), s()], Type::Boolean),
        },
        Function {
            name: "verify_http_pgp_signature".into(),
            decl: f(vec![a(), s(), s()], Type::Boolean),
        },
        Function {
            name: "pin_image".into(),
            decl: f(vec![a(), s()], Type::Boolean),
        },
        Function {
            name: "artifact_attestation".into(),
            decl: f(vec![a(), s()], a()),
        },
        Function {
            name: "github_attestation".into(),
            decl: f(vec![a(), s()], a()),
        },
    ]
}

pub fn ref_term(path: &str) -> Term {
    let parts = path
        .split('.')
        .enumerate()
        .map(|(i, seg)| {
            if i == 0 {
                Term::var(seg, None)
            } else {
                Term::string(seg, None)
            }
        })
        .collect();
    Term::reference(parts, None)
}

pub fn percentile(v: &[f64], p: f64) -> f64 {
    let rank = ((p / 100.0) * v.len() as f64).ceil().max(1.0) as usize;
    v[rank.min(v.len()) - 1]
}

pub fn stats(name: &str, mut v: Vec<f64>) -> (String, String) {
    v.sort_by(f64::total_cmp);
    let (p50, p90, p99, max) = (
        percentile(&v, 50.0),
        percentile(&v, 90.0),
        percentile(&v, 99.0),
        v[v.len() - 1],
    );
    (
        format!(
            "{name:<34} {:>6} {p50:>10.1} {p90:>10.1} {p99:>10.1} {max:>10.1}  us",
            v.len()
        ),
        format!(
            "\"{name}\":{{\"unit\":\"us\",\"n\":{},\"p50\":{p50:.1},\"p90\":{p90:.1},\"p99\":{p99:.1},\"max\":{max:.1}}}",
            v.len()
        ),
    )
}

pub fn output(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

pub fn stamp() -> (String, String, String, String) {
    let (host, os, load) = if cfg!(target_os = "macos") {
        (
            format!(
                "{} ({})",
                output("sysctl", &["-n", "machdep.cpu.brand_string"]),
                output("sysctl", &["-n", "hw.model"])
            ),
            format!(
                "macOS {} ({})",
                output("sw_vers", &["-productVersion"]),
                output("sw_vers", &["-buildVersion"])
            ),
            output("sysctl", &["-n", "vm.loadavg"])
                .trim_matches(|c: char| c == '{' || c == '}' || c.is_whitespace())
                .to_string(),
        )
    } else {
        (
            std::fs::read_to_string("/proc/cpuinfo")
                .ok()
                .and_then(|c| {
                    c.lines()
                        .find(|l| l.starts_with("model name"))
                        .and_then(|l| l.split(':').nth(1))
                        .map(|s| s.trim().to_string())
                })
                .unwrap_or_else(|| "unknown".into()),
            format!("{} {}", output("uname", &["-s"]), output("uname", &["-r"])),
            std::fs::read_to_string("/proc/loadavg")
                .ok()
                .map(|l| l.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
                .unwrap_or_else(|| "unknown".into()),
        )
    };
    let dir = env!("CARGO_MANIFEST_DIR");
    let mut rev = output("git", &["-C", dir, "rev-parse", "--short", "HEAD"]);
    if Command::new("git")
        .args(["-C", dir, "status", "--porcelain", "--untracked-files=no"])
        .output()
        .is_ok_and(|o| !o.stdout.is_empty())
    {
        rev.push_str("-dirty");
    }
    (host, os, rev, load)
}

/// The process's peak resident memory in MiB (getrusage(2)); none off Unix.
#[cfg(unix)]
pub fn peak_rss_mib() -> Option<u64> {
    // SAFETY: getrusage(2) into a zeroed struct.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut ru) };
    let max = u64::try_from(ru.ru_maxrss).ok()?;
    Some(if cfg!(target_os = "macos") {
        max >> 20
    } else {
        max >> 10
    })
}

#[cfg(not(unix))]
pub fn peak_rss_mib() -> Option<u64> {
    None
}
