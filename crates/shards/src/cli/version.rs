//! `shards version` (docker/cli cli/command/system/version.go): on a colour terminal
//! shards' own page; otherwise, or with `--format`, docker/cli's versionInfo of this
//! shards. Both halves are this binary's, which is the daemon too (D36): nothing is asked
//! of a daemon, and none is started.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::process::ExitCode;

use shards_cmdline::commands::VERSION;
use shards_cmdline::format::version::{Client, Component, Server, render};

const VERSION_TEXT: &str = env!("CARGO_PKG_VERSION");

/// What the client has where Docker's has an Engine API version: none, as shards' CLI asks
/// its daemon over its own socket.
const NO_API: &str = "none";

/// The Engine API the daemon serves (D170): docker/cli v29.8.1's (its moby/moby/api
/// v1.56.0, moby's MaxAPIVersion), from moby's oldest by default; none where the daemon is
/// not yet.
#[cfg(unix)]
pub(crate) const ENGINE_API: (&str, &str) = ("1.56", "1.40");
#[cfg(not(unix))]
pub(crate) const ENGINE_API: (&str, &str) = (NO_API, NO_API);

/// The compiler that built this binary (build.rs), where Docker names Go's.
fn compiler() -> String {
    option_env!("SHARDS_RUSTC").unwrap_or_default().to_owned()
}

/// The commit it was built from, where the build said one (SHARDS_GIT_COMMIT), else as
/// docker/cli says it knows none (cli/version/version.go).
fn commit() -> String {
    option_env!("SHARDS_GIT_COMMIT")
        .unwrap_or("unknown-commit")
        .to_owned()
}

/// When it was built, where the build said (SHARDS_BUILD_TIME), else as docker/cli says
/// it knows not: shards' builds are reproducible, and carry no time of their own.
fn build_time() -> String {
    option_env!("SHARDS_BUILD_TIME")
        .unwrap_or("unknown-buildtime")
        .to_owned()
}

/// This host's OS and architecture as Go names them.
fn platform() -> (String, String) {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    (os.to_owned(), arch.to_owned())
}

/// The host kernel's release, as uname says it.
fn host_kernel() -> String {
    #[cfg(unix)]
    {
        // SAFETY: utsname is plain arrays of bytes, for which zero is a value.
        let mut u: libc::utsname = unsafe { std::mem::zeroed() };
        // SAFETY: uname(2) fills the struct it is given.
        if unsafe { libc::uname(&mut u) } == 0 {
            // SAFETY: uname NUL-terminates release within the array.
            let release = unsafe { std::ffi::CStr::from_ptr(u.release.as_ptr()) };
            return release.to_string_lossy().into_owned();
        }
    }
    String::new()
}

/// The client's half and the server's, of this shards.
pub(crate) fn info() -> (Client, Server) {
    let (os, arch) = platform();
    let client = Client {
        platform: None,
        version: VERSION_TEXT.to_owned(),
        api_version: NO_API.to_owned(),
        default_api_version: NO_API.to_owned(),
        git_commit: commit(),
        go_version: compiler(),
        os: os.clone(),
        arch: arch.clone(),
        build_time: build_time(),
        context: "default".to_owned(),
    };
    let details = |pairs: &[(&str, String)]| -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect()
    };
    let backend = if cfg!(target_os = "macos") {
        "Hypervisor.framework"
    } else if cfg!(target_os = "linux") {
        "KVM"
    } else {
        "Windows Hypervisor Platform"
    };
    let mut components = vec![
        Component {
            name: "Engine".to_owned(),
            version: VERSION_TEXT.to_owned(),
            details: details(&[
                ("ApiVersion", ENGINE_API.0.to_owned()),
                ("Arch", arch.clone()),
                ("BuildTime", build_time()),
                // Its checkpoints, Docker's experimental feature, a microVM's snapshots (D176).
                ("Experimental", "true".to_owned()),
                ("GitCommit", commit()),
                ("GoVersion", compiler()),
                ("KernelVersion", host_kernel()),
                ("MinAPIVersion", ENGINE_API.1.to_owned()),
                ("Os", os.clone()),
            ]),
        },
        Component {
            name: "VMM".to_owned(),
            version: VERSION_TEXT.to_owned(),
            details: details(&[("Hypervisor", backend.to_owned())]),
        },
    ];
    if let Some(k) = crate::kernel::KERNEL {
        components.push(Component {
            name: "Guest kernel".to_owned(),
            version: k.name.split('-').nth(1).unwrap_or(k.name).to_owned(),
            details: details(&[("Sha256", k.sha256.to_owned())]),
        });
    }
    components.push(Component {
        name: "shards-init".to_owned(),
        version: VERSION_TEXT.to_owned(),
        details: BTreeMap::new(),
    });
    let server = Server {
        platform: "shards".to_owned(),
        version: VERSION_TEXT.to_owned(),
        api_version: ENGINE_API.0.to_owned(),
        min_api_version: ENGINE_API.1.to_owned(),
        os,
        arch,
        components,
    };
    (client, server)
}

/// `shards --version`: the root flag's line (docker/cli cobra.go, `Docker version %s,
/// build %s`).
pub fn short() -> ExitCode {
    let _ = writeln!(
        std::io::stdout(),
        "shards version {VERSION_TEXT}, build {}",
        commit()
    );
    ExitCode::SUCCESS
}

/// `shards version [OPTIONS]`, the words after it `args`.
pub fn run(args: &[OsString]) -> ExitCode {
    let argv = match super::utf8(args) {
        Ok(argv) => argv,
        Err(e) => return super::failed(&e),
    };
    let parsed = match super::read(&VERSION, "shards version", &argv, &shards_cmdline::flags::value) {
        Ok(parsed) => parsed,
        Err(answered) => return answered,
    };
    let format = parsed.string("format");
    if format.is_empty()
        && let Some(p) = super::look::styled()
    {
        super::look::version(&p, &mut std::io::stdout().lock());
        return ExitCode::SUCCESS;
    }
    let (client, server) = info();
    let east_asian = shards_cmdline::width::east_asian(|name| {
        std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
    });
    let (out, failed) = render(format, &client, Some(&server), east_asian);
    let _ = std::io::stdout().write_all(out.as_bytes());
    match failed {
        None => ExitCode::SUCCESS,
        Some((status, said)) => {
            let _ = writeln!(std::io::stderr(), "{said}");
            ExitCode::from(status)
        }
    }
}
