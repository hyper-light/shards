//! `shards api-build`: an Agentfile's build for shards-api (docs/design/architecture.md D263;
//! `shards_api::Builds`): its request read from stdin, built in this process as `shards build
//! image` builds (`build::for_api`), each step's progress told as a frame on stdout, then its
//! image's ID or its error. Not for people to run.

use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

use shards_api::builds::wire::{self, Request, Source, to_host};

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

/// The build its request asks for: its image's ID, or its error.
fn serve() -> Result<String, String> {
    let (kind, payload) = shards_api::image::wire::read_frame(&mut std::io::stdin().lock())
        .map_err(|e| format!("its request: {e}"))?
        .ok_or("its request: none came")?;
    if kind != to_host::REQUEST {
        return Err(format!("its request: a frame of kind {kind}"));
    }
    let request = wire::read_request(&payload).map_err(|e| e.to_string())?;
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
    for (id, source) in &request.secrets {
        let at = match source {
            Source::File(path) => format!("src={path}"),
            Source::Env(name) => format!("env={name}"),
        };
        flag(
            "secret",
            format!("{},{}", quoted(&format!("id={id}")), quoted(&at)),
        );
    }
    for (id, paths) in &request.ssh {
        flag(
            "ssh",
            if paths.is_empty() {
                id.clone()
            } else {
                format!("{id}={}", paths.join(","))
            },
        );
    }
    let iid = iid
        .to_str()
        .ok_or_else(|| format!("{}: not UTF-8", iid.display()))?;
    flag("iidfile", iid.to_string());
    flag("progress", "plain".to_string());
    argv.push(request.context.clone());
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
}
