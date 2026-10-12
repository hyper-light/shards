//! A run given this Mac's GPU (D144): Vulkan through Venus, in a real microVM.
//!
//! It needs what only a host prepared for it has: SHARDS_TEST_GPU_KERNEL, a kernel built
//! with resources/kernel/gpu.config, and SHARDS_TEST_GPU_VM, the VM process built with the
//! GPU's renderer (scripts/gpu-libs, `--features gpu`) and signed as build.rs signs it.
//! Elsewhere it says SKIP.

mod common;

use std::path::PathBuf;

use common::{TempDir, cannot_run_vms, guest_init, run_shards_env};

/// `shards run --gpus all` of an image with Mesa's Venus: `vulkaninfo` finds the Mac's GPU
/// through Venus, its render node given the run as `--device` gives one; and a build step
/// given the render node by a CDI device (`RUN --device`) finds it too (D145).
#[test]
fn the_gpu_renders_through_venus_in_runs_and_build_steps() {
    let (Some(kernel), Some(vm)) = (
        std::env::var_os("SHARDS_TEST_GPU_KERNEL").map(PathBuf::from),
        std::env::var_os("SHARDS_TEST_GPU_VM").map(PathBuf::from),
    ) else {
        println!("SKIP: SHARDS_TEST_GPU_KERNEL and SHARDS_TEST_GPU_VM name no GPU kernel and VM process");
        return;
    };
    if cannot_run_vms() {
        return;
    }
    let home = TempDir::new("gpu-home");
    let ctx = TempDir::new("gpu-ctx");
    std::fs::write(
        ctx.join("Dockerfile"),
        "FROM debian:trixie-slim\nRUN apt-get update -q && apt-get install -y -q --no-install-recommends mesa-vulkan-drivers vulkan-tools\n",
    )
    .unwrap();
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
        ("SHARDS_VM_BINARY", vm.as_os_str()),
    ];
    let timeout = std::time::Duration::from_secs(900);
    let built = run_shards_env(
        &["build"],
        &["-q", "-t", "gpu-venus:1", ctx.to_str().unwrap()],
        &env,
        timeout,
    );
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = run_shards_env(
        &["run"],
        &["--rm", "--gpus", "all", "gpu-venus:1", "vulkaninfo", "--summary"],
        &env,
        timeout,
    );
    assert_eq!(ran.status, Some(0), "{}\n{}", ran.stdout, ran.stderr);
    assert!(
        ran.stdout.contains("driverName         = venus"),
        "{}",
        ran.stdout
    );
    assert!(ran.stdout.contains("Virtio-GPU Venus ("), "{}", ran.stdout);

    // A step given the render node by a CDI device has a builder with the GPU (D145).
    let cdi = TempDir::new("gpu-cdi");
    std::fs::write(
        cdi.join("gpu.json"),
        r#"{"cdiVersion":"0.6.0","kind":"test.example/gpu","annotations":{"org.mobyproject.buildkit.device.autoallow":"true"},"devices":[{"name":"0","containerEdits":{"deviceNodes":[{"path":"/dev/dri/renderD128"}]}}]}"#,
    )
    .unwrap();
    std::fs::write(
        ctx.join("Dockerfile"),
        "FROM gpu-venus:1\nRUN --device=test.example/gpu=0 vulkaninfo --summary > /venus.txt\n",
    )
    .unwrap();
    let mut step_env = env.to_vec();
    step_env.push(("SHARDS_CDI_SPEC_DIRS", cdi.as_os_str()));
    let built = run_shards_env(
        &["build"],
        &["-q", "-t", "gpu-venus:2", ctx.to_str().unwrap()],
        &step_env,
        timeout,
    );
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let ran = run_shards_env(
        &["run"],
        &["--rm", "gpu-venus:2", "cat", "/venus.txt"],
        &env,
        timeout,
    );
    assert_eq!(ran.status, Some(0), "{}\n{}", ran.stdout, ran.stderr);
    assert!(ran.stdout.contains("Virtio-GPU Venus ("), "{}", ran.stdout);
}
