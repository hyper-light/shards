//! shards-cmdline answers `build` command lines as buildx does: every answer buildx gave in
//! buildx.json (scripts/buildx/oracle_test.go), shards gives byte for byte, but those
//! [`BETTER`] lists. Where buildx ran its build, shards names the same flags and arguments,
//! and what the build would be given: its secrets, entitlements and ulimits; or it refuses
//! the flags it does not serve.

#![allow(clippy::unwrap_used, clippy::panic)]

use shards_cmdline::buildflags;
use shards_cmdline::commands::{BUILD, BUILDER_PRUNE};
use shards_cmdline::flags::{self, Outcome};

/// What a build step carries at most, the run protocol's frame (shards-abi `MAX_PAYLOAD`).
const STEP: u64 = 1 << 20;

/// Where shards answers better than buildx, each for its reason (buildflags.rs): the
/// command line, and shards' stdout, stderr and status.
const BETTER: &[(&[&str], &str, &str, u8)] = &[
    // A secret as large as a step carries, not BuildKit's gRPC session's 500 KiB.
    (
        &["--secret", "id=big,src=big.txt", "."],
        "RUN --secret=[\"id=big,src=big.txt\"] \".\"\nSECRET big: 512001 bytes, \"ssssssssssssssss\"\n",
        "",
        0,
    ),
    // `as`, which go-units leaves out for the way Docker starts a container.
    (
        &["--ulimit", "as=1", "."],
        "RUN --ulimit=[as=1:1] \".\"\nULIMIT as=1:1\n",
        "",
        0,
    ),
];

/// What buildx's build is given, as oracle_test.go's `built` says it, from shards'
/// reading of the same flags: stdout's lines, or the error.
fn built(parsed: &flags::Parsed, out: &mut String) -> Result<(), String> {
    // The attestations, first, as toBuildOptions reads them.
    let mut attests: Vec<String> = parsed.many("attest").to_vec();
    for kind in ["provenance", "sbom"] {
        let v = parsed.string(kind);
        if !v.is_empty() {
            attests.push(buildflags::canonicalize_attest(kind, v));
        }
    }
    for (kind, value) in buildflags::attests_map(&buildflags::parse_attests(&attests)?) {
        match value {
            None => out.push_str(&format!("ATTEST {kind} disabled\n")),
            Some(v) => out.push_str(&format!("ATTEST {kind} {}\n", shards_cmdline::go::quote(&v))),
        }
    }
    let familiar = |n: &str| {
        shards_image::reference::Reference::parse_normalized(n)
            .map(|r| r.familiar())
            .map_err(|e| e.to_string())
    };
    for (name, value) in buildflags::parse_contexts(parsed.many("build-context"), &familiar)? {
        out.push_str(&format!("CONTEXT {name} {}\n", shards_cmdline::go::quote(&value)));
    }
    buildflags::check_iidfile(
        &buildflags::parse_exports(parsed.many("output"))?,
        parsed.string("iidfile"),
    )?;
    let env = |name: &str| (name == "SHARDS_ORACLE_SECRET").then(|| b"from the environment".to_vec());
    let secrets = buildflags::parse_secrets(parsed.many("secret"))?;
    for (id, value) in buildflags::store(secrets, &env, STEP)? {
        let b = value.bytes();
        let head = String::from_utf8_lossy(b.get(..16).unwrap_or(b));
        out.push_str(&format!(
            "SECRET {id}: {} bytes, {}\n",
            b.len(),
            shards_cmdline::go::quote(&head)
        ));
    }
    // The SSH agents, as BuildKit's provider takes them, with no SSH_AUTH_SOCK.
    let ssh = buildflags::parse_ssh(parsed.many("ssh"));
    buildflags::ssh_agents(&ssh, &|_| None, &|_| Ok(()))?;
    for s in &ssh {
        let paths: Vec<String> = s.paths.iter().map(|p| shards_cmdline::go::quote(p)).collect();
        out.push_str(&format!("SSH {} [{}]\n", s.id, paths.join(" ")));
    }
    let allowed = buildflags::parse_entitlements(parsed.many("allow"))?;
    let exports = buildflags::parse_exports(parsed.many("output"))?;
    let stat = |p: &str| match std::fs::metadata(p) {
        Ok(m) if m.is_dir() => Ok(buildflags::Found::Dir),
        Ok(_) => Ok(buildflags::Found::File),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(buildflags::Found::Nothing),
        Err(e) => Err(e.to_string()),
    };
    // isSafeLocalDeleteDest: under the working directory, and not it.
    let safe = |d: &std::path::Path| {
        let wd = std::env::current_dir().unwrap();
        let at = if d.is_absolute() {
            d.to_path_buf()
        } else {
            wd.join(d)
        };
        at.starts_with(&wd) && at != wd
    };
    let outputs = buildflags::create_exports(
        &exports,
        parsed.bool("push"),
        parsed.bool("load"),
        allowed.local_delete,
        &stat,
        false,
        &safe,
    )?;
    for o in &outputs {
        let attrs: Vec<String> = o.attrs.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let (dir, file) = match &o.dest {
            buildflags::Dest::Dir(d) => (d.to_string_lossy().into_owned(), false),
            buildflags::Dest::File(_) | buildflags::Dest::Stdout => (String::new(), true),
            buildflags::Dest::Store => (String::new(), false),
        };
        out.push_str(&format!(
            "OUTPUT {} [{}] dir={} file={file}\n",
            o.kind,
            attrs.join(" "),
            shards_cmdline::go::quote(&dir)
        ));
    }
    if !allowed.granted.is_empty() || allowed.local_delete {
        let granted: Vec<String> = allowed
            .granted
            .iter()
            .map(|g| shards_cmdline::go::quote(g))
            .collect();
        out.push_str(&format!(
            "ALLOW [{}] local.delete={}\n",
            granted.join(" "),
            allowed.local_delete
        ));
    }
    if !parsed.many("ulimit").is_empty() {
        let list: Vec<String> = buildflags::ulimits(parsed.many("ulimit"))?
            .iter()
            .map(|u| u.to_string())
            .collect();
        out.push_str(&format!("ULIMIT {}\n", list.join(",")));
    }
    let mut lines: Vec<String> = buildflags::parse_annotations(parsed.many("annotation"))?
        .into_iter()
        .map(|a| {
            let p = a.platform.map(|p| format!("[{p}]")).unwrap_or_default();
            format!("ANNOTATION {}{p} {}={}\n", a.kind, a.key, a.value)
        })
        .collect();
    lines.sort();
    out.push_str(&lines.concat());
    Ok(())
}

#[test]
fn build_answers_as_buildx() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/buildx.json");
    let answers: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    // The oracle's directory of secrets (oracle_test.go, TestShardsOracle).
    let dir = std::env::temp_dir().join(format!("shards-buildx-oracle-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("small.txt"), "a small secret\n").unwrap();
    std::fs::write(dir.join("edge.txt"), vec![b's'; 500 * 1024]).unwrap();
    std::fs::write(dir.join("big.txt"), vec![b's'; 500 * 1024 + 1]).unwrap();
    std::env::set_current_dir(&dir).unwrap();
    let mut failures = Vec::new();
    for a in answers.as_array().unwrap() {
        let argv: Vec<String> = a["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let (mut stdout, mut stderr, mut status) = (
            a["stdout"].as_str().unwrap(),
            a["stderr"].as_str().unwrap(),
            a["status"].as_u64().unwrap() as u8,
        );
        if let Some(&(_, out, err, code)) = BETTER.iter().find(|(line, ..)| *line == argv.as_slice()) {
            (stdout, stderr, status) = (out, err, code);
        }
        // `prune` first: buildx's prune, which `shards builder prune` runs.
        let (command, path, words) = match argv.split_first() {
            Some((first, rest)) if first == "prune" => (&BUILDER_PRUNE, "shards buildx prune", rest),
            _ => (&BUILD, "shards buildx build", argv.as_slice()),
        };
        let (got_out, got_err, got_status, refused) =
            match flags::parse(command, path, words, &buildflags::validate) {
                Outcome::Run(parsed) => {
                    let mut line = String::from("RUN");
                    for (name, value) in parsed.given() {
                        line.push_str(&format!(" --{name}={value}"));
                    }
                    for arg in &parsed.args {
                        line.push(' ');
                        line.push_str(&shards_cmdline::go::quote(arg));
                    }
                    let mut out = format!("{}{line}\n", parsed.notices);
                    let done = if std::ptr::eq(command, &BUILD) {
                        built(&parsed, &mut out)
                    } else {
                        Ok(())
                    };
                    match done {
                        Ok(()) => (out, String::new(), 0, false),
                        Err(e) => (out, format!("ERROR: {e}\n"), 1, false),
                    }
                }
                Outcome::Help { notices } => (
                    format!("{notices}{}", flags::help(command, path, 80)),
                    String::new(),
                    0,
                    false,
                ),
                Outcome::Fail {
                    notices,
                    text,
                    status,
                } => {
                    let refused = text.contains("is not supported by shards yet");
                    (notices, format!("{text}\n"), status, refused)
                }
            };
        // shards keeps `-h` for help, where buildx's root deprecates it: a deliberate
        // difference, its notice the only one.
        let stdout = stdout.replace("Flag shorthand -h has been deprecated, use --help\n", "");
        // An answer that carries the host's own error text (a file not found) is buildx's
        // on the host the oracle ran on, Linux: on Windows the text is Windows', which the
        // oracle does not record, so there the rest of the answer alone is compared.
        let os_text = "no such file or directory";
        let ok = if refused {
            stdout.starts_with("RUN") && got_status == 1
        } else if cfg!(windows) && stderr.contains(os_text) {
            let head = |s: &str| s.split(": stat ").next().map(str::to_string);
            got_out == stdout && head(&got_err) == head(stderr) && got_status == status
        } else {
            got_out == stdout && got_err == stderr && got_status == status
        };
        if !ok {
            failures.push(format!(
                "{argv:?}\n  got:  {got_status} {got_out:?} {got_err:?}\n  want: {status} {stdout:?} {stderr:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} answers differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
