//! `shards api-build`: an Agentfile's build for shards-api (docs/design/architecture.md D263;
//! `shards_api::Builds`): its request read from stdin, built in this process as `shards build
//! image` builds (`build::for_api`), each step's progress told as a frame on stdout, then its
//! image's ID or its error. Not for people to run.
//!
//! A component's request (D270) is built as an Agentfile of its one directive over `scratch`,
//! written to a context of its own, whose files go to a local output there; what the directive
//! laid becomes its OSI artifact (`agent::component`), whose digest is its end.

use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

use shards_api::builds::wire::{self, ComponentRequest, Request, Source, to_host};

pub(crate) fn command() -> ExitCode {
    let built = serve();
    let said = wire::end(&built).map_err(|e| e.to_string()).and_then(|frame| {
        let mut out = std::io::stdout().lock();
        out.write_all(&frame)
            .and_then(|()| out.flush())
            .map_err(|e| format!("its end: {e}"))
    });
    match said {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "shards api-build: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The build its request asks for: its image's ID, or its component's, or its error.
fn serve() -> Result<String, String> {
    let (kind, payload) = shards_api::image::wire::read_frame(&mut std::io::stdin().lock())
        .map_err(|e| format!("its request: {e}"))?
        .ok_or("its request: none came")?;
    match kind {
        to_host::REQUEST => image(&payload),
        to_host::COMPONENT => component(&payload),
        kind => Err(format!("its request: a frame of kind {kind}")),
    }
}

/// An image's build: its ID, or its error.
fn image(payload: &[u8]) -> Result<String, String> {
    let request = wire::read_request(payload).map_err(|e| e.to_string())?;
    let dir = shards_ipc::home()?.join("tmp");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let iid = dir.join(format!("api-build-{}.iid", std::process::id()));
    let built = argv(&request, &iid).and_then(|argv| crate::build::for_api(&argv));
    let id =
        built.and_then(|()| std::fs::read_to_string(&iid).map_err(|e| format!("{}: {e}", iid.display())));
    let _ = std::fs::remove_file(&iid);
    Ok(id?.trim().to_string())
}

/// `request` as `shards build image`'s flags, its image's ID written to `iid`: each secret a
/// `--secret` of quoted CSV fields (buildx reads it as CSV), each agent an `--ssh`.
fn argv(request: &Request, iid: &Path) -> Result<Vec<String>, String> {
    let mut argv = Vec::new();
    let mut flag = |name: &str, value: String| {
        argv.push(format!("--{name}"));
        argv.push(value);
    };
    if let Some(file) = &request.file {
        flag("file", file.clone());
    }
    if let Some(platform) = &request.platform {
        flag("platform", platform.clone());
    }
    if let Some(tag) = &request.tag {
        flag("tag", tag.clone());
    }
    access(&mut argv, &request.secrets, &request.ssh);
    let iid = iid
        .to_str()
        .ok_or_else(|| format!("{}: not UTF-8", iid.display()))?;
    argv.extend(["--iidfile".to_string(), iid.to_string()]);
    argv.extend(["--progress".to_string(), "plain".to_string()]);
    argv.push(request.context.clone());
    Ok(argv)
}

/// Each secret as a `--secret` of quoted CSV fields (buildx reads it as CSV), each agent an
/// `--ssh`.
fn access(argv: &mut Vec<String>, secrets: &[(String, Source)], ssh: &[(String, Vec<String>)]) {
    for (id, source) in secrets {
        let at = match source {
            Source::File(path) => format!("src={path}"),
            Source::Env(name) => format!("env={name}"),
        };
        argv.push("--secret".into());
        argv.push(format!("{},{}", quoted(&format!("id={id}")), quoted(&at)));
    }
    for (id, paths) in ssh {
        argv.push("--ssh".into());
        argv.push(if paths.is_empty() {
            id.clone()
        } else {
            format!("{id}={}", paths.join(","))
        });
    }
}

/// A component's build (D270): its OSI artifact's digest, or its error. The request is
/// checked again here, as the client's word is not this side's; its source is one the build
/// fetches and pins (Git, http(s), s3), a path or an OCI reference refused, each saying why.
fn component(payload: &[u8]) -> Result<String, String> {
    let request = wire::read_component(payload).map_err(|e| e.to_string())?;
    request.check().map_err(|e| e.to_string())?;
    built_source(&request.source)?;
    let host = crate::build::host_platform();
    let platform = match &request.platform {
        Some(p) => crate::build::target_platforms(std::slice::from_ref(p), &host)?
            .into_iter()
            .next()
            .ok_or_else(|| format!("{p}: no platform"))?,
        None => host,
    };
    let dir = shards_ipc::home()?
        .join("tmp")
        .join(format!("api-component-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let made = (|| {
        let file = dir.join("Agentfile");
        std::fs::write(&file, agentfile(&request)).map_err(|e| format!("{}: {e}", file.display()))?;
        let out = dir.join("out");
        crate::build::for_api(&component_argv(&request, &dir, &file, &out)?)?;
        let kind = match request.kind {
            shards_api::ComponentKind::Agent => shards_image::osi::Kind::Agent,
            shards_api::ComponentKind::Harness => shards_image::osi::Kind::Harness,
            shards_api::ComponentKind::Mcp => shards_image::osi::Kind::Mcp,
        };
        let platform = shards_image::osi::Platform {
            os: String::from_utf8_lossy(&platform.os).into_owned(),
            architecture: String::from_utf8_lossy(&platform.architecture).into_owned(),
            variant: (!platform.variant.is_empty())
                .then(|| String::from_utf8_lossy(&platform.variant).into_owned()),
        };
        crate::agent::component(kind, &request.name, &out, &request.dir(), platform, &request.tag)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    made
}

/// Whether `source` is one a component is built of: what its directive fetches and pins. A
/// context path is no source of the build's own to pin (`shards build agent DIR` makes an
/// artifact of a directory), and an OCI reference names an artifact already, swapped by its
/// digest as it is.
fn built_source(source: &str) -> Result<(), String> {
    use shards_agentfile::agentfile::{Source as Kind, source_of};
    match source_of(source.as_bytes()).map_err(|e| String::from_utf8_lossy(&e).into_owned())? {
        Kind::Git(_) | Kind::Http(_) | Kind::S3 { .. } => Ok(()),
        Kind::Path(_) => Err(format!(
            "{source}: a context path is no source a component is built of: `shards build agent DIR` makes an artifact of a directory"
        )),
        Kind::Oci(_) => Err(format!(
            "{source}: an OCI reference names an artifact already, which a run is swapped to by its digest"
        )),
    }
}

/// The Agentfile of `request`'s one directive, over nothing (D270).
fn agentfile(request: &ComponentRequest) -> String {
    format!("FROM scratch\n{}\n", request.directive())
}

/// The flags of `request`'s build of the Agentfile `file` in the context `context`, its files
/// written to `out` as a local output (D52) and no image kept.
fn component_argv(
    request: &ComponentRequest,
    context: &Path,
    file: &Path,
    out: &Path,
) -> Result<Vec<String>, String> {
    let utf8 = |p: &Path| {
        p.to_str()
            .map(str::to_string)
            .ok_or_else(|| format!("{}: not UTF-8", p.display()))
    };
    let mut argv = vec!["--file".to_string(), utf8(file)?];
    if let Some(platform) = &request.platform {
        argv.extend(["--platform".to_string(), platform.clone()]);
    }
    argv.extend([
        "--output".to_string(),
        format!(
            "{},{}",
            quoted("type=local"),
            quoted(&format!("dest={}", utf8(out)?))
        ),
    ]);
    access(&mut argv, &request.secrets, &request.ssh);
    argv.extend(["--progress".to_string(), "plain".to_string(), utf8(context)?]);
    Ok(argv)
}

/// `field` as a quoted CSV field (RFC 4180 §2, as Go's encoding/csv reads one).
fn quoted(field: &str) -> String {
    format!("\"{}\"", field.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_the_build_commands_flags() {
        let request = Request {
            context: "/ctx".into(),
            file: Some("/ctx/Agentfile".into()),
            platform: Some("linux/arm64".into()),
            tag: Some("me/agent:1".into()),
            secrets: vec![
                ("token".into(), Source::Env("GIT_AUTH_TOKEN".into())),
                ("key".into(), Source::File("/a,b/\"k\"".into())),
            ],
            ssh: vec![
                ("default".into(), vec![]),
                ("deploy".into(), vec!["/k1".into(), "/k2".into()]),
            ],
        };
        let argv = argv(&request, Path::new("/home/tmp/api-build-1.iid")).unwrap();
        assert_eq!(
            argv,
            [
                "--file",
                "/ctx/Agentfile",
                "--platform",
                "linux/arm64",
                "--tag",
                "me/agent:1",
                "--secret",
                "\"id=token\",\"env=GIT_AUTH_TOKEN\"",
                "--secret",
                "\"id=key\",\"src=/a,b/\"\"k\"\"\"",
                "--ssh",
                "default",
                "--ssh",
                "deploy=/k1,/k2",
                "--iidfile",
                "/home/tmp/api-build-1.iid",
                "--progress",
                "plain",
                "/ctx",
            ]
        );
        // As the build reads them back: the quoted secret whole, its comma and quotes kept.
        let secrets = shards_cmdline::buildflags::parse_secrets(&[argv[7].clone(), argv[9].clone()]).unwrap();
        assert_eq!(
            (secrets[0].id.as_str(), secrets[0].env.as_str()),
            ("token", "GIT_AUTH_TOKEN")
        );
        assert_eq!(
            (secrets[1].id.as_str(), secrets[1].file.as_str()),
            ("key", "/a,b/\"k\"")
        );
        let agents = shards_cmdline::buildflags::parse_ssh(&[argv[11].clone(), argv[13].clone()]);
        assert_eq!(agents[1].paths, ["/k1", "/k2"]);
    }
    fn harness() -> ComponentRequest {
        ComponentRequest::of(&shards_api::ComponentSpec {
            kind: shards_api::ComponentKind::Harness,
            name: "codex".into(),
            source: "https://example.com/codex.tar.gz".into(),
            checksum: Some(format!("sha256:{}", "d".repeat(64))),
            redirects: vec!["cdn.example.com".into()],
            s3: shards_api::S3Source::default(),
            platform: Some("linux/arm64".into()),
            tag: "kiln/harness-codex:1".into(),
            secrets: vec![shards_api::BuildSecret {
                id: "HTTP_AUTH_TOKEN_example.com".into(),
                source: shards_api::SecretSource::Env("TOKEN".into()),
            }],
            ssh: Vec::new(),
        })
        .unwrap()
    }

    #[test]
    fn a_component_is_built_as_an_agentfile_of_its_one_directive_to_a_local_output() {
        let request = harness();
        assert_eq!(
            agentfile(&request),
            format!(
                "FROM scratch\nHARNESS --checksum=sha256:{} --redirect=cdn.example.com codex FROM https://example.com/codex.tar.gz\n",
                "d".repeat(64)
            )
        );
        let argv = component_argv(
            &request,
            Path::new("/home/tmp/c"),
            Path::new("/home/tmp/c/Agentfile"),
            Path::new("/home/tmp/c/out"),
        )
        .unwrap();
        assert_eq!(
            argv,
            [
                "--file",
                "/home/tmp/c/Agentfile",
                "--platform",
                "linux/arm64",
                "--output",
                "\"type=local\",\"dest=/home/tmp/c/out\"",
                "--secret",
                "\"id=HTTP_AUTH_TOKEN_example.com\",\"env=TOKEN\"",
                "--progress",
                "plain",
                "/home/tmp/c",
            ]
        );
        // The build reads the one directive, its name, source and pins as the request gave them.
        use shards_agentfile::{agentfile as af, instructions, lint::Linter, parser};
        let parsed = parser::parse_as(agentfile(&request).as_bytes(), parser::Dialect::Agentfile).unwrap();
        let stages = instructions::parse(&parsed, &Linter::default()).unwrap().stages;
        let [stage] = stages.as_slice() else {
            panic!("{stages:?}")
        };
        let [command] = stage.commands.as_slice() else {
            panic!("{:?}", stage.commands)
        };
        let instructions::Kind::Agentfile(af::Directive::Harness(h)) = &command.kind else {
            panic!("{:?}", command.kind)
        };
        assert_eq!(h.name, b"codex");
        assert_eq!(h.source, b"https://example.com/codex.tar.gz");
        assert_eq!(h.to, None);
        assert_eq!(
            h.fetch.checksum,
            format!("sha256:{}", "d".repeat(64)).into_bytes()
        );
        assert_eq!(h.fetch.redirects, [b"cdn.example.com".to_vec()]);
    }

    #[test]
    fn an_unpinned_source_is_refused_in_the_builds_words() {
        // As a whole build refuses it (D54): an http(s) source needs --checksum.
        use shards_agentfile::{instructions, lint::Linter, parser};
        let mut request = harness();
        request.checksum = None;
        let parsed = parser::parse_as(agentfile(&request).as_bytes(), parser::Dialect::Agentfile).unwrap();
        let refused = instructions::parse(&parsed, &Linter::default()).unwrap_err();
        let said = String::from_utf8_lossy(&refused.message);
        assert!(said.contains("--checksum"), "{said}");
    }

    #[test]
    fn a_component_is_built_of_a_fetched_source_alone() {
        assert!(built_source("https://example.com/a.tar.gz").is_ok());
        assert!(built_source("https://example.com/org/repo.git#v1").is_ok());
        assert!(built_source("ssh://git@example.com/org/repo.git#v1").is_ok());
        assert!(built_source("git@example.com:org/repo.git").is_ok());
        assert!(built_source("s3://bucket/key.tar").is_ok());
        let path = built_source("./harness").unwrap_err();
        assert!(path.contains("context path"), "{path}");
        let oci = built_source("registry.example.com/kiln/codex:1").unwrap_err();
        assert!(oci.contains("OCI reference"), "{oci}");
        let s3 = built_source("s3://bucket").unwrap_err();
        assert!(s3.contains("names no object"), "{s3}");
    }
}
