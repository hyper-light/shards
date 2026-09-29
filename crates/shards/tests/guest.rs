//! The default guest (docs/design/architecture.md D28): with no guest recorded and none
//! named, `shards run IMAGE` boots the pinned kernel, downloaded on first need, and the
//! shards-init that `shardsd` carries. Kernels are served on loopback here, through
//! `SHARDS_KERNEL_URL`, as GitHub serves release assets: behind a redirect.

#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::{
    TempDir, cannot_run_vms, guest_init, kernel, kernel_artifact, run_shards_env, serve_file, served,
};

const TIMEOUT: Duration = Duration::from_secs(120);

#[test]
fn runs_boot_the_default_guest_fetched_and_checked_on_first_need() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let (server, requests) = serve_file(std::fs::read(kernel()).unwrap());
    let home = TempDir::new("default-guest");
    let url = format!("{server}/moved");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL_URL", OsStr::new(&url)),
    ];
    let pinned = kernel_artifact().sha256;

    let first = run_shards_env(&["run"], &[image.as_str()], &env, TIMEOUT);
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", first.stdout, first.stderr);
    assert_eq!(first.status, Some(0), "{shown}");
    assert!(first.stderr.contains(&format!("from {url}")), "{shown}");
    assert!(
        first.stderr.contains(&format!("Guest kernel: sha256:{pinned}")),
        "{shown}"
    );
    assert!(first.stdout.lines().any(|l| l == "uid 1000"), "{shown}");

    // The store holds the kernel, and the init this build carries: the same bytes as a
    // build of shards-init in another target directory.
    let stored = |digest: &str| home.join("guest").join(format!("sha256-{digest}"));
    assert_eq!(
        std::fs::read(stored(pinned)).unwrap(),
        std::fs::read(kernel()).unwrap()
    );
    let init = std::fs::read(guest_init()).unwrap();
    let init_digest = common::sha256_digest(&init);
    let init_hex = init_digest.strip_prefix("sha256:").unwrap();
    assert_eq!(std::fs::read(stored(init_hex)).unwrap(), init);
    let shown = run_shards_env(&["guest"], &[] as &[&str], &env, TIMEOUT);
    assert_eq!(
        shown.stdout,
        format!("kernel sha256:{pinned} (default)\ninit   {init_digest} (default)\n")
    );

    // Stored: the next run fetches nothing.
    let asked = requests.load(Ordering::SeqCst);
    let second = run_shards_env(&["run"], &["--pull", "never", image.as_str()], &env, TIMEOUT);
    assert_eq!(second.status, Some(0), "{}", second.stderr);
    assert!(!second.stderr.contains("Downloading"), "{}", second.stderr);
    assert_eq!(requests.load(Ordering::SeqCst), asked);
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
}

/// A download that is not the pinned kernel is refused before anything boots, and leaves
/// nothing in the store: one byte changed, one byte more, one byte less, or nothing at all.
#[test]
fn a_download_that_is_not_the_pinned_kernel_is_kept_nowhere() {
    let (image, _) = served();
    let good = std::fs::read(kernel()).unwrap();
    let mut changed = good.clone();
    changed[good.len() / 2] ^= 1;
    let mut longer = good.clone();
    longer.push(0);
    let shorter = good[..good.len() - 1].to_vec();
    let cases = [
        ("changed", changed, "/moved", "not the pinned kernel's"),
        ("longer", longer, "/moved", "more than the pinned kernel's"),
        ("shorter", shorter, "/moved", "not the pinned kernel's"),
        ("missing", Vec::new(), "/absent", "HTTP 404"),
    ];
    for (case, body, path, why) in cases {
        let (server, _) = serve_file(body);
        let home = TempDir::new(&format!("bad-kernel-{case}"));
        let url = format!("{server}{path}");
        let env = [
            ("SHARDS_HOME", home.as_os_str()),
            ("SHARDS_KERNEL_URL", OsStr::new(&url)),
        ];
        let run = run_shards_env(&["run"], &[image.as_str()], &env, TIMEOUT);
        let shown = format!("{case}\n--- stdout\n{}\n--- stderr\n{}", run.stdout, run.stderr);
        assert_ne!(run.status, Some(0), "{shown}");
        assert!(run.stderr.contains(why), "{shown}");
        assert!(
            run.stderr.contains("shards guest use --kernel FILE --init FILE"),
            "{shown}"
        );
        // Refused before the image was pulled, let alone booted.
        assert!(!run.stderr.contains("Pulling from"), "{shown}");
        let kept: Vec<String> = std::fs::read_dir(home.join("guest"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| !name.starts_with("sha256-") || name.ends_with(kernel_artifact().sha256))
            .collect();
        assert!(kept.is_empty(), "{case}: {kept:?}");
        let _ = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    }
}
