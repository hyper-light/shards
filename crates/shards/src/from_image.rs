//! An Agentfile made from an image (§10, §12.16): `FROM` the image by its manifest's
//! digest, so that a build of it reproduces the image's layers exactly, its config written
//! out as the instructions that set it (which the image already has, so the config the
//! build makes is the image's), and its history as comments. Its ports and volumes are
//! declarations, granted to nothing: under default deny (§3) a network grants them.
//!
//! `shards make agentfile IMAGE [-o FILE]` (or `shards agentfile`).

use std::io::Write as _;
use std::process::ExitCode;

use shards_dockerfile::image::{Healthcheck, Image};
use shards_image::oci::{self, Document};
use shards_image::reference::Reference;

/// A value for a double-quoted Dockerfile word: `\`, `"` and `$` escaped, so that
/// nothing in it is expanded or ends it. `None` where it holds a line break, which no
/// Dockerfile line can.
fn quoted(v: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(v);
    if s.contains(['\n', '\r']) {
        return None;
    }
    let mut out = String::from("\"");
    for c in s.chars() {
        if matches!(c, '\\' | '"' | '$') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Some(out)
}

/// A bare word for `WORKDIR`, `USER` and `STOPSIGNAL`: `$` and `\` escaped, so that it is
/// not expanded; `None` where it holds a line break.
fn word(v: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(v);
    if s.contains(['\n', '\r']) {
        return None;
    }
    Some(s.replace('\\', "\\\\").replace('$', "\\$"))
}

/// A JSON array, the exec form.
fn exec_form(items: &[Vec<u8>]) -> String {
    let items: Vec<String> = items
        .iter()
        .map(|i| shards_dockerfile::export::json_string(i))
        .collect();
    format!("[{}]", items.join(", "))
}

/// A line that could not be written, said as a comment.
fn unwritable(what: &str) -> String {
    format!("# {what}: holds a line break, which no Dockerfile line can, and is left out\n")
}

/// `HEALTHCHECK`, its options where they are set, then its test.
fn healthcheck(h: &Healthcheck) -> Option<String> {
    let first = h.test.first().map(|t| String::from_utf8_lossy(t).into_owned());
    if first.as_deref() == Some("NONE") {
        return Some("HEALTHCHECK NONE\n".into());
    }
    let mut out = String::from("HEALTHCHECK");
    for (flag, d) in [
        ("interval", h.interval),
        ("timeout", h.timeout),
        ("start-period", h.start_period),
        ("start-interval", h.start_interval),
    ] {
        if d != 0 {
            out.push_str(&format!(
                " --{flag}={}",
                shards_cmdline::gotime::format_duration(d)
            ));
        }
    }
    if h.retries != 0 {
        out.push_str(&format!(" --retries={}", h.retries));
    }
    let rest = h.test.get(1..).unwrap_or_default();
    match first.as_deref() {
        Some("CMD") => out.push_str(&format!(" CMD {}", exec_form(rest))),
        Some("CMD-SHELL") => {
            let cmd = rest
                .first()
                .map(|c| String::from_utf8_lossy(c).into_owned())
                .unwrap_or_default();
            if cmd.contains(['\n', '\r']) {
                return None;
            }
            out.push_str(&format!(" CMD {cmd}"));
        }
        // No test of its own: it takes the base's, which is the image itself.
        _ => return Some(String::new()),
    }
    out.push('\n');
    Some(out)
}

/// The Agentfile of `image`, which `name` names, its manifest `digest`.
pub fn agentfile(name: &Reference, digest: &str, image: &Image) -> String {
    let c = &image.config;
    let mut out = format!(
        "# An Agentfile of {name}, made by shards: the image itself, by its manifest's\n\
         # digest, then its settings, which it has already, written out.\n"
    );
    if !image.history.is_empty() {
        out.push_str("#\n# How the image was made, as it records it:\n");
        for h in &image.history {
            let when = h
                .created
                .as_ref()
                .and_then(|t| t.rfc3339_nano().ok())
                .unwrap_or_default();
            let by = String::from_utf8_lossy(&h.created_by);
            let mut lines = by.lines();
            let first = lines.next().unwrap_or("");
            let empty = if h.empty_layer { " (no layer)" } else { "" };
            out.push_str(&format!("#   {when}{empty} {first}\n"));
            for l in lines {
                out.push_str(&format!("#     {l}\n"));
            }
        }
    }
    let mut bare = name.clone();
    bare.digest = None;
    out.push_str(&format!("\nFROM {bare}@{digest}\n"));
    let mut put = |line: Option<String>, what: &str| match line {
        Some(l) => out.push_str(&l),
        None => out.push_str(&unwritable(what)),
    };
    for e in &c.env {
        let (k, v) = match e.iter().position(|&b| b == b'=') {
            Some(at) => (
                e.get(..at).unwrap_or_default(),
                e.get(at + 1..).unwrap_or_default(),
            ),
            None => (e.as_slice(), &b""[..]),
        };
        let k = String::from_utf8_lossy(k);
        put(quoted(v).map(|v| format!("ENV {k}={v}\n")), &format!("ENV {k}"));
    }
    for (k, v) in &c.labels {
        let key = quoted(k);
        put(
            key.zip(quoted(v)).map(|(k, v)| format!("LABEL {k}={v}\n")),
            &format!("LABEL {}", String::from_utf8_lossy(k)),
        );
    }
    if !c.shell.is_empty() {
        put(Some(format!("SHELL {}\n", exec_form(&c.shell))), "SHELL");
    }
    if !c.working_dir.is_empty() {
        put(word(&c.working_dir).map(|w| format!("WORKDIR {w}\n")), "WORKDIR");
    }
    if !c.user.is_empty() {
        put(word(&c.user).map(|u| format!("USER {u}\n")), "USER");
    }
    if !c.stop_signal.is_empty() {
        put(
            word(&c.stop_signal).map(|s| format!("STOPSIGNAL {s}\n")),
            "STOPSIGNAL",
        );
    }
    if let Some(h) = &c.healthcheck {
        put(healthcheck(h), "HEALTHCHECK");
    }
    for o in &c.on_build {
        let o = String::from_utf8_lossy(o);
        put(
            (!o.contains(['\n', '\r'])).then(|| format!("ONBUILD {o}\n")),
            "ONBUILD",
        );
    }
    if !c.exposed_ports.is_empty() || !c.volumes.is_empty() {
        out.push_str(
            "# Declared by the image, granted to nothing: a network grants a port, a run\n\
             # mounts a volume (default deny).\n",
        );
    }
    for p in c.exposed_ports.keys() {
        out.push_str(&format!("EXPOSE {}\n", String::from_utf8_lossy(p)));
    }
    if !c.volumes.is_empty() {
        let v: Vec<Vec<u8>> = c.volumes.keys().cloned().collect();
        out.push_str(&format!("VOLUME {}\n", exec_form(&v)));
    }
    if !c.entrypoint.is_empty() {
        out.push_str(&format!("ENTRYPOINT {}\n", exec_form(&c.entrypoint)));
    }
    if !c.cmd.is_empty() {
        out.push_str(&format!("CMD {}\n", exec_form(&c.cmd)));
    }
    out
}

const USAGE: &str = "Usage:  shards make agentfile [OPTIONS] IMAGE

Write an Agentfile of an image: the image by its digest, its settings written out, its
history as comments. The image is pulled if it is not here.

Options:
  -h, --help          Print usage
  -o, --output FILE   Write the Agentfile to FILE, not to standard output
";

/// `shards agentfile IMAGE [-o FILE]`.
pub fn command(args: impl Iterator<Item = std::ffi::OsString>) -> ExitCode {
    let args: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
    let mut output = None;
    let mut image = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                let _ = write!(std::io::stdout(), "{USAGE}");
                return ExitCode::SUCCESS;
            }
            "-o" | "--output" => output = it.next().cloned(),
            o if o.starts_with("--output=") => output = Some(o.trim_start_matches("--output=").to_string()),
            o if o.starts_with('-') => return fail(&format!("unknown flag: {o}\n\n{USAGE}")),
            name if image.is_none() => image = Some(name.to_string()),
            _ => return fail(&format!("one image at a time\n\n{USAGE}")),
        }
    }
    let Some(image) = image else {
        return fail(&format!("an image is needed\n\n{USAGE}"));
    };
    match made(&image) {
        Ok(text) => {
            let written = match &output {
                Some(path) => std::fs::write(path, &text).map_err(|e| format!("{path}: {e}")),
                None => std::io::stdout()
                    .write_all(text.as_bytes())
                    .map_err(|e| e.to_string()),
            };
            match written {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => fail(&e),
            }
        }
        Err(e) => fail(&e),
    }
}

fn fail(why: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "Error: {why}");
    ExitCode::FAILURE
}

/// The Agentfile of the image `given` names, here or pulled.
fn made(given: &str) -> Result<String, String> {
    let reference = Reference::parse(given).map_err(|e| format!("{given}: {e}"))?;
    let home = shards_ipc::home()?;
    let store = crate::pull::store(&home)?;
    let limits = crate::pull::limits()?;
    let targets = shards_image::platform::guest();
    let here =
        shards_registry::pull::local(&store, &reference, &targets, &limits).map_err(|e| e.to_string())?;
    if here.is_none() {
        crate::pull::fetch(
            &home,
            &reference,
            &targets,
            &|_| {},
            &|_| {},
            None,
            &|k| std::env::var(k).ok(),
            false,
        )?;
    }
    let desc = store
        .tagged(&reference.to_string())
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{given}: not in the store after its pull"))?;
    let bytes = store
        .content(&desc, oci::MAX_MANIFEST)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{given}: its manifest is missing"))?;
    let Document::Manifest(m) = oci::parse_document(&bytes, &desc.media_type).map_err(|e| e.to_string())?
    else {
        return Err(format!("{given}: its record names an index"));
    };
    let config = store
        .content(&m.config, oci::MAX_CONFIG)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{given}: its config is missing"))?;
    let image = Image::from_json(&config).map_err(|e| String::from_utf8_lossy(&e).into_owned())?;
    let os = String::from_utf8_lossy(&image.platform.os).into_owned();
    if os != "linux" {
        return Err(format!(
            "{given} is for {os}: shards makes Agentfiles of Linux images alone"
        ));
    }
    Ok(agentfile(&reference, &desc.digest, &image))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_written_so_that_nothing_in_them_expands() {
        assert_eq!(quoted(b"a $HOME \"q\" \\b").unwrap(), r#""a \$HOME \"q\" \\b""#);
        assert!(quoted(b"two\nlines").is_none());
        assert_eq!(word(b"/w/$x").unwrap(), "/w/\\$x");
    }

    #[test]
    fn an_images_config_is_written_as_its_instructions() {
        let config = br#"{"architecture":"amd64","os":"linux","config":{"User":"app","Env":["PATH=/bin","A=a b"],"Entrypoint":["/bin/e"],"Cmd":["x","y"],"WorkingDir":"/w","Labels":{"k":"v"},"ExposedPorts":{"80/tcp":{}},"Volumes":{"/data":{}},"StopSignal":"SIGTERM","Healthcheck":{"Test":["CMD","/bin/hc"],"Interval":5000000000,"Retries":2},"Shell":["/bin/bash","-c"],"OnBuild":["COPY . /x"]},"rootfs":{"type":"layers","diff_ids":[]},"history":[{"created":"1970-01-01T00:00:00Z","created_by":"COPY a /a # buildkit"}]}"#;
        let image = Image::from_json(config).unwrap();
        let name = Reference::parse("example.com/app:1").unwrap();
        let text = agentfile(&name, "sha256:abc", &image);
        for want in [
            "FROM example.com/app:1@sha256:abc\n",
            "ENV PATH=\"/bin\"\n",
            "ENV A=\"a b\"\n",
            "LABEL \"k\"=\"v\"\n",
            "SHELL [\"/bin/bash\", \"-c\"]\n",
            "WORKDIR /w\n",
            "USER app\n",
            "STOPSIGNAL SIGTERM\n",
            "HEALTHCHECK --interval=5s --retries=2 CMD [\"/bin/hc\"]\n",
            "ONBUILD COPY . /x\n",
            "EXPOSE 80/tcp\n",
            "VOLUME [\"/data\"]\n",
            "ENTRYPOINT [\"/bin/e\"]\n",
            "CMD [\"x\", \"y\"]\n",
            "#   1970-01-01T00:00:00Z COPY a /a # buildkit\n",
        ] {
            assert!(text.contains(want), "{want:?} in:\n{text}");
        }
    }
}
