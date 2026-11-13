use super::{ImageIdentity, NativeBoot, NativeImages, verify_native_image};
use crate::{GuestSpec, Init, LaunchError, Refusal, RuntimePaths, Share};
use sha2::{Digest as _, Sha256};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn image(bytes: &[u8]) -> ImageIdentity {
    ImageIdentity::new(u64::try_from(bytes.len()).unwrap(), Sha256::digest(bytes).into()).unwrap()
}

fn images(gpu: bool) -> NativeImages {
    NativeImages::new(
        [1; 32],
        image(b"cli"),
        image(b"kernel"),
        Some(image(b"cpu")),
        if gpu { Some(image(b"gpu")) } else { None },
    )
    .unwrap()
}

// Pure preparation creates/dials no socket. Keep only this syntactic locator
// short even where a test's TMPDIR is longer than sockaddr_un holds.
fn socket(root: &Path) -> PathBuf {
    root.ancestors().last().unwrap().join("native-model.v")
}

fn spec(root: &Path) -> GuestSpec {
    GuestSpec {
        kernel: root.join("kernel"),
        init: Init::Initrd(root.join("init")),
        cmdline: String::new(),
        vcpus: 1,
        memory_mib: 256,
        gpu: false,
        gpu_devices: Vec::new(),
        disks: Vec::new(),
        shares: vec![
            Share {
                dir: root.join("first-Ω"),
                read_only: true,
            },
            Share {
                dir: root.join("second"),
                read_only: false,
            },
        ],
        ports: vec![1],
        binary: None,
    }
}

/// D215/R01/R04: typed rights survive source mutation in original order, including
/// kept0; neither flattened argv nor a replacement runtime root grants rights.
#[test]
fn native_preparation_retains_exact_inputs_and_legacy_has_no_worker_plan() {
    let dir = shards_testdir::TempDir::new("native-plan").unwrap();
    let paths = RuntimePaths::new(dir.path().join("cache"), dir.path().join("home")).unwrap();
    let mut original = spec(dir.path());
    let bound = images(false);
    let plan = crate::prepare_native_boot(
        &original,
        &dir.path().join("cli"),
        &socket(dir.path()),
        NativeBoot {
            paths: &paths,
            images: bound,
            console_kept: 0,
        },
    )
    .unwrap();
    let expected = original.shares.clone();
    original.kernel = PathBuf::from("foreign");
    original.shares.clear();
    let view = plan.native_startup().unwrap();
    assert_eq!(view.kernel(), dir.path().join("kernel"));
    assert_eq!(view.shares(), expected);
    assert_eq!(view.console_kept(), 0);
    assert_eq!(view.console_pin(), image(b"kernel").sha256());
    assert_eq!(view.images(), bound);
    assert_eq!(view.paths(), &paths);
    assert!(!view.gpu());
    let foreign = RuntimePaths::new(dir.path().join("other-cache"), dir.path().join("home")).unwrap();
    assert!(matches!(
        plan.with_runtime_paths(&foreign),
        Err(Refusal::Launch(LaunchError::NativeInputs))
    ));
    let legacy =
        crate::prepare_boot(&spec(dir.path()), &dir.path().join("cli"), &socket(dir.path()), 0).unwrap();
    assert!(matches!(
        legacy.native_startup(),
        Err(Refusal::Launch(LaunchError::ImageMissing))
    ));
}

/// A CPU image declaration is never a fallback for a GPU image, and relative
/// share/kernel inputs cannot become a native fixed-worker allowlist.
#[test]
fn native_preparation_refuses_missing_variant_and_unbound_paths() {
    let dir = shards_testdir::TempDir::new("native-inputs").unwrap();
    let paths = RuntimePaths::new(dir.path().join("cache"), dir.path().join("home")).unwrap();
    let mut input = spec(dir.path());
    input.gpu = true;
    let prepare = |input: &GuestSpec, images| {
        crate::prepare_native_boot(
            input,
            &dir.path().join("cli"),
            &socket(dir.path()),
            NativeBoot {
                paths: &paths,
                images,
                console_kept: 7,
            },
        )
    };
    assert!(matches!(
        prepare(&input, images(false)),
        Err(Refusal::Launch(LaunchError::ImageMissing))
    ));
    assert!(
        prepare(&input, images(true))
            .unwrap()
            .native_startup()
            .unwrap()
            .gpu()
    );
    input.shares.first_mut().unwrap().dir = PathBuf::from("relative");
    assert!(matches!(
        prepare(&input, images(true)),
        Err(Refusal::Launch(LaunchError::NativeInputs))
    ));
    input.shares.clear();
    input.kernel = PathBuf::from("relative");
    assert!(matches!(
        prepare(&input, images(true)),
        Err(Refusal::Launch(LaunchError::NativeInputs))
    ));
    assert!(ImageIdentity::new(0, [1; 32]).is_err());
}

/// An image is read in the pieces its file system prefers, not in SHA-256's 64-byte blocks:
/// a launch hashes the whole `shards` binary and kernel (tens of MiB), which at 64 bytes a
/// read was a read(2) or ReadFile per 64 bytes, about a million a launch.
#[test]
fn a_verified_image_is_read_in_its_file_systems_pieces() {
    let dir = shards_testdir::TempDir::new("native-reads").unwrap();
    let path = dir.path().join("image");
    let bytes: Vec<u8> = (0..1u32 << 20).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &bytes).unwrap();
    super::READS.with(|reads| reads.set(0));
    verify_native_image(&path, image(&bytes), Instant::now() + Duration::from_secs(60)).unwrap();
    let reads = super::READS.with(std::cell::Cell::get);
    assert!(
        reads < bytes.len() / 64,
        "{reads} reads of a {}-byte image",
        bytes.len()
    );
}

/// Actual private files exercise full digest/length/end/deadline and retained
/// handle ownership. A checked file is not authority for a later replaced path.
#[test]
fn verified_native_image_retains_exact_file_and_refuses_hostile_content() {
    let dir = shards_testdir::TempDir::new("native-file").unwrap();
    let path = dir.path().join("image");
    std::fs::write(&path, b"actual image").unwrap();
    let deadline = || Instant::now() + Duration::from_secs(1);
    let mut verified = verify_native_image(&path, image(b"actual image"), deadline()).unwrap();
    assert!(matches!(
        verify_native_image(&path, image(b"foreign data"), deadline()),
        Err(Refusal::Launch(LaunchError::ImageDigest))
    ));
    assert!(matches!(
        verify_native_image(&path, image(b"short"), deadline()),
        Err(Refusal::Launch(LaunchError::ImageChanged))
    ));
    assert!(matches!(
        verify_native_image(&path, image(b"actual image"), Instant::now()),
        Err(Refusal::Launch(LaunchError::ImageDeadline))
    ));
    assert!(verify_native_image(dir.path(), image(b"actual image"), deadline()).is_err());
    assert!(verify_native_image(Path::new("relative"), image(b"actual image"), deadline()).is_err());
    let renamed = dir.path().join("held");
    std::fs::rename(&path, &renamed).unwrap();
    std::fs::write(&path, b"other image!").unwrap();
    let mut bytes = Vec::new();
    verified.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"actual image");
    assert!(verify_native_image(&path, image(b"actual image"), deadline()).is_err());
    #[cfg(unix)]
    {
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&renamed, &link).unwrap();
        assert!(verify_native_image(&link, image(b"actual image"), deadline()).is_err());
    }
}
