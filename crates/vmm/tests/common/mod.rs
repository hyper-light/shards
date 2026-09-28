//! E2E fixtures: pinned real kernels, real guest binaries, a signed VMM, bounded runs.
//! Test-support code: failing loudly is the point, so the no-panic lints are off here.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// A downloadable test artifact pinned by content hash.
pub struct Artifact {
    pub name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
}

/// Firecracker CI's aarch64 guest kernel (uncompressed Image with virtio built in).
pub const KERNEL_6_18: Artifact = Artifact {
    name: "vmlinux-6.18.48-aarch64",
    url: "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/20260923-6f82ac4cf331-0/aarch64/vmlinux-6.18.48",
    sha256: "a80108af80d9549b357ea7e00bd5c12f80686869541d135a8a67f6fe1ec3451e",
};

fn sha256(path: &Path) -> String {
    let out = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .expect("running shasum");
    String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Returns the artifact's cached path, downloading and verifying it if needed.
pub fn fetch(a: &Artifact) -> PathBuf {
    let dir = workspace().join("target/artifacts");
    let path = dir.join(a.name);
    if path.exists() && sha256(&path) == a.sha256 {
        return path;
    }
    std::fs::create_dir_all(&dir).unwrap();
    let part = dir.join(format!("{}.{}.part", a.name, std::process::id()));
    let ok = Command::new("curl")
        .args(["-fsSL", "--retry", "3", "-o"])
        .arg(&part)
        .arg(a.url)
        .status()
        .unwrap();
    assert!(ok.success(), "downloading {}", a.url);
    let got = sha256(&part);
    assert_eq!(got, a.sha256, "{} does not match its pinned hash", a.url);
    std::fs::rename(&part, &path).unwrap();
    path
}

pub fn kernel() -> &'static Path {
    static K: OnceLock<PathBuf> = OnceLock::new();
    K.get_or_init(|| fetch(&KERNEL_6_18))
}

/// Builds `shards-init` for the guest (static musl, `guest` profile).
pub fn guest_init() -> &'static Path {
    static I: OnceLock<PathBuf> = OnceLock::new();
    I.get_or_init(|| {
        let target_dir = workspace().join("target/guest");
        // Go through the rustup proxy on PATH (not $CARGO, the bare cargo binary) and drop
        // the dyld paths cargo injects into test processes: the proxy's environment is
        // what lets rust-lld find the toolchain's libLLVM.
        let st = Command::new("cargo")
            .env_remove("DYLD_FALLBACK_LIBRARY_PATH")
            .env_remove("DYLD_LIBRARY_PATH")
            .current_dir(workspace())
            .args([
                "build",
                "-p",
                "shards-init",
                "--profile",
                "guest",
                "--target",
                "aarch64-unknown-linux-musl",
            ])
            .arg("--target-dir")
            .arg(&target_dir)
            .status()
            .unwrap();
        assert!(st.success(), "building shards-init");
        target_dir.join("aarch64-unknown-linux-musl/guest/shards-init")
    })
}

/// A private copy of the VMM binary, ad-hoc signed with the hypervisor entitlement.
pub fn vmm() -> &'static Path {
    static V: OnceLock<PathBuf> = OnceLock::new();
    V.get_or_init(|| {
        let dir = workspace().join("target/e2e");
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join(format!("shards-vmm-{}", std::process::id()));
        std::fs::copy(env!("CARGO_BIN_EXE_shards-vmm"), &copy).unwrap();
        let st = Command::new("codesign")
            .arg("--entitlements")
            .arg(workspace().join("resources/hvf.entitlements"))
            .args(["--force", "-s", "-"])
            .arg(&copy)
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(st.success(), "codesign");
        copy
    })
}

pub struct Run {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
}

impl fmt::Display for Run {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "status {:?} after {:?}\n--- stdout\n{}\n--- stderr\n{}",
            self.status, self.elapsed, self.stdout, self.stderr
        )
    }
}

impl Run {
    /// Parses `shards-timing {"exit_us":N,"markers":[[m,t],...]}` from stderr.
    fn timing(&self) -> Option<&str> {
        self.stderr.lines().find_map(|l| l.strip_prefix("shards-timing "))
    }

    pub fn exit_us(&self) -> Option<u128> {
        let t = self.timing()?;
        let v = t.split("\"exit_us\":").nth(1)?.split(',').next()?;
        v.parse().ok()
    }

    pub fn marker_us(&self, marker: u32) -> Option<u128> {
        let t = self.timing()?;
        let list = t.split("\"markers\":").nth(1)?;
        list.split("],[").find_map(|pair| {
            let pair = pair.trim_matches(|c| c == '[' || c == ']' || c == '}');
            let (m, at) = pair.split_once(',')?;
            (m.parse::<u32>().ok()? == marker)
                .then(|| at.parse().ok())
                .flatten()
        })
    }
}

/// Runs the VMM with `args`, killing it (and failing) after `timeout`.
pub fn run_vmm<S: AsRef<std::ffi::OsStr>>(args: &[S], timeout: Duration) -> Run {
    let start = Instant::now();
    let mut child = Command::new(vmm())
        .args(args)
        .env("SHARDS_TIMING", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning shards-vmm");
    let collect = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = r.read_to_string(&mut s);
            s
        })
    };
    let out = collect(Box::new(child.stdout.take().unwrap()));
    let err = collect(Box::new(child.stderr.take().unwrap()));
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "shards-vmm did not exit within {timeout:?}\n--- stdout\n{}\n--- stderr\n{}",
                out.join().unwrap(),
                err.join().unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    Run {
        status: status.code(),
        stdout: out.join().unwrap(),
        stderr: err.join().unwrap(),
        elapsed: start.elapsed(),
    }
}
