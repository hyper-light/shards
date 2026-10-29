//! A run given its host's GPU (D144, D201): Vulkan through Venus, in a real microVM, on a
//! Mac's GPU through MoltenVK and on a Linux host's through its own Vulkan driver.
//!
//! It boots the pinned gpu kernel (SHARDS_TEST_GPU_KERNEL names another to try) in the
//! GPU's VM process that `shards` carries where it was built with the renderer
//! (SHARDS_GPU_LIBS; SHARDS_TEST_GPU_VM names another). Where there is neither, it says
//! SKIP.

mod common;

use std::ffi::OsStr;
use std::path::PathBuf;

use common::{TempDir, cannot_run_vms, guest_init, run_shards_env};

/// Whether this host has a GPU to give a microVM: a Mac's, which Metal has wherever its
/// VMs run; on Linux a GPU's device node, a DRM render node or NVIDIA's (a CI runner's
/// virtual machine has none).
fn host_gpu() -> Result<(), &'static str> {
    if cfg!(target_os = "macos") {
        return Ok(());
    }
    let render = std::fs::read_dir("/dev/dri").is_ok_and(|d| {
        d.flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("renderD"))
    });
    if render || std::path::Path::new("/dev/nvidia0").exists() {
        Ok(())
    } else {
        Err("this host has no GPU: no render node (/dev/dri/renderD*), and no /dev/nvidia0")
    }
}

/// The GPU's kernel, and the VM process the test names, if it names one; or why there is
/// no GPU to test.
fn gpu() -> Result<(PathBuf, Option<PathBuf>), &'static str> {
    host_gpu()?;
    let vm = std::env::var_os("SHARDS_TEST_GPU_VM").map(PathBuf::from);
    if vm.is_none() && !common::carries_gpu_vm() {
        return Err("this shards carries no GPU VM process (SHARDS_GPU_LIBS)");
    }
    let kernel = match std::env::var_os("SHARDS_TEST_GPU_KERNEL") {
        Some(k) => PathBuf::from(k),
        None => common::gpu_kernel()
            .ok_or("no gpu kernel is pinned for this architecture")?
            .to_path_buf(),
    };
    Ok((kernel, vm))
}

/// `shards run --gpus all` of an image with Mesa's Venus: `vulkaninfo` finds the host's GPU
/// through Venus, its render node given the run as `--device` gives one; a build step
/// given the render node by a CDI device (`RUN --device`) finds it too (D145); and the
/// GPU's runs restore a template of their own, apart from the same image's without it
/// (D150).
#[test]
fn the_gpu_renders_through_venus_in_runs_and_build_steps() {
    if cannot_run_vms() {
        return;
    }
    let (kernel, vm) = match gpu() {
        Ok(found) => found,
        Err(why) => {
            println!("SKIP: {why}");
            return;
        }
    };
    let home = TempDir::new("gpu-home");
    let ctx = TempDir::new("gpu-ctx");
    std::fs::write(
        ctx.join("Dockerfile"),
        "FROM debian:trixie-slim\nRUN apt-get update -q && apt-get install -y -q --no-install-recommends mesa-vulkan-drivers vulkan-tools\n",
    )
    .unwrap();
    let mut env: Vec<(&str, &OsStr)> = vec![
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    if let Some(vm) = &vm {
        env.push(("SHARDS_VM_BINARY", vm.as_os_str()));
    }
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
        "FROM gpu-venus:1\nRUN --device=test.example/gpu=0 vulkaninfo --summary > /venus.txt\nUSER 1000\nRUN --device=test.example/gpu=0 vulkaninfo --summary > /tmp/venus.txt\n",
    )
    .unwrap();
    let mut step_env = env.clone();
    step_env.push(("SHARDS_CDI_SPEC_DIRS", cdi.as_os_str()));
    let built = run_shards_env(
        &["build"],
        &["-q", "-t", "gpu-venus:2", ctx.to_str().unwrap()],
        &step_env,
        timeout,
    );
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    // Root's step, and a user's, whose render node is udev's 0666 as a host's is.
    for found in ["/venus.txt", "/tmp/venus.txt"] {
        let ran = run_shards_env(&["run"], &["--rm", "gpu-venus:2", "cat", found], &env, timeout);
        assert_eq!(ran.status, Some(0), "{found}: {}\n{}", ran.stdout, ran.stderr);
        assert!(
            ran.stdout.contains("Virtio-GPU Venus ("),
            "{found}: {}",
            ran.stdout
        );
    }
    // A run given the render node by CDI name has the GPU (D146): its daemon started again,
    // with the spec's directory.
    let stopped = run_shards_env(&["stop"], &["daemon"], &env, timeout);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
    let ran = run_shards_env(
        &["run"],
        &[
            "--rm",
            "--device",
            "test.example/gpu=0",
            "gpu-venus:1",
            "vulkaninfo",
            "--summary",
        ],
        &step_env,
        timeout,
    );
    assert_eq!(ran.status, Some(0), "{}\n{}", ran.stdout, ran.stderr);
    assert!(ran.stdout.contains("Virtio-GPU Venus ("), "{}", ran.stdout);
    // A run's user other than root has the GPU too.
    let ran = run_shards_env(
        &["run"],
        &[
            "--rm",
            "--gpus",
            "all",
            "--user",
            "1000",
            "gpu-venus:1",
            "vulkaninfo",
            "--summary",
        ],
        &env,
        timeout,
    );
    assert_eq!(ran.status, Some(0), "{}\n{}", ran.stdout, ran.stderr);
    assert!(ran.stdout.contains("Virtio-GPU Venus ("), "{}", ran.stdout);

    // Runs of the recorded guest restore templates (a daemon started with SHARDS_KERNEL
    // boots every run), a GPU's its own, saved before its guest made anything of the
    // GPU's (D150). Its key is apart from the same image's runs without the GPU: with the
    // gpu kernel recorded, one saved first is not the GPU run's.
    let stopped = run_shards_env(&["stop"], &["daemon"], &env, timeout);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
    let mut recorded_env: Vec<(&str, &OsStr)> = vec![("SHARDS_HOME", home.as_os_str())];
    if let Some(vm) = &vm {
        recorded_env.push(("SHARDS_VM_BINARY", vm.as_os_str()));
    }
    let record = |kernel: &std::path::Path| {
        let used = run_shards_env(
            &["guest"],
            &[
                "use",
                "--kernel",
                kernel.to_str().unwrap(),
                "--init",
                guest_init().to_str().unwrap(),
            ],
            &recorded_env,
            timeout,
        );
        assert_eq!(used.status, Some(0), "{}", used.stderr);
    };
    record(&kernel);
    let ran = run_shards_env(&["run"], &["--rm", "gpu-venus:1", "true"], &recorded_env, timeout);
    assert_eq!(ran.status, Some(0), "{}\n{}", ran.stdout, ran.stderr);
    let gpu_run = |which: &str, booted: bool| {
        let ran = run_shards_env(
            &["run"],
            &["--rm", "--gpus", "all", "gpu-venus:1", "vulkaninfo", "--summary"],
            &recorded_env,
            timeout,
        );
        assert_eq!(ran.status, Some(0), "{which}: {}\n{}", ran.stdout, ran.stderr);
        assert!(
            ran.stdout.contains("Virtio-GPU Venus ("),
            "{which}: {}",
            ran.stdout
        );
        // Booted, init started in the run; restored, in the template.
        assert_eq!(
            ran.marker_us(shards_abi::marker::INIT_STARTED).is_some(),
            booted,
            "{which}: {}",
            ran.stderr
        );
        // Each resumed: one restored, as it was; the one booted, once its template was saved.
        assert!(
            ran.marker_us(shards_abi::marker::RESUMED).is_some(),
            "{which}: {}",
            ran.stderr
        );
    };
    gpu_run("saving its template", true);
    gpu_run("restored from it", false);
    // The GPU's runs boot the pinned gpu kernel whatever guest is recorded: with the
    // usual kernel recorded, they restore the same template.
    record(common::kernel());
    gpu_run("restored with the usual kernel recorded", false);
    let origins: Vec<String> = std::fs::read_dir(home.join("templates"))
        .unwrap()
        .filter_map(|e| {
            let path = e.unwrap().path();
            (path.extension()? == "origin").then(|| std::fs::read_to_string(path).unwrap())
        })
        .collect();
    for gpu in ["\"gpu\":true", "\"gpu\":false"] {
        assert!(origins.iter().any(|o| o.contains(gpu)), "no {gpu}: {origins:?}");
    }
}
