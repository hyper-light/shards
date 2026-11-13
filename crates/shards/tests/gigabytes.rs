//! An image of gigabytes, alone: its 2 GiB written, built into an image and read by a run
//! take a CI runner's disk, and on Windows' runners the tests run beside them stalled with
//! it, their microVMs not ready or not ended in their time (PM M419). Cargo runs a crate's
//! test binaries one after another, so this one's test runs with none beside it.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

mod common;

use std::ffi::OsStr;

use common::{TempDir, cannot_run_vms, guest_init, kernel, noise, run_shards_env, served};

/// An image of gigabytes boots as a small one does. Its root is a pmem region, which
/// libnvdimm probes asynchronously, longest for the largest; init waits for its node
/// (init's pmem.rs), where a 2.1 GB image's runs found none and failed every time.
#[test]
fn an_image_of_gigabytes_boots() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("gigabytes-big");
    let (kernel, init) = (kernel(), guest_init());
    let env: [(&str, &OsStr); 3] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
    ];
    let pulled = run_shards_env(&[], &["pull", "-q", &image], &env);
    assert_eq!(pulled.status, Some(0), "{}", pulled.stderr);
    let ctx = home.join("big");
    std::fs::create_dir(&ctx).unwrap();
    std::fs::write(ctx.join("Dockerfile"), format!("FROM {image}\nCOPY big /big\n")).unwrap();
    noise(&ctx.join("big"), 2048);
    // Its build copies 2 GiB through the builder: past a minute on CI's x86_64 runners,
    // where it stays a guard against a hang.
    let built = run_shards_env(&[], &["build", "-q", "-t", "big:1", ctx.to_str().unwrap()], &env);
    assert_eq!(built.status, Some(0), "{built}");
    std::fs::remove_file(ctx.join("big")).unwrap();
    let ran = run_shards_env(
        &[],
        &["run", "--pull", "never", "--rm", "big:1", "fs", "open:r:/big"],
        &env,
    );
    assert_eq!(ran.status, Some(0), "{ran}");
}
