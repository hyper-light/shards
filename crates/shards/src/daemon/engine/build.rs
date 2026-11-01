//! Building images through the API (D170): `shards build image`, for the programs that
//! make their sandboxes' images (docker-py's build, docker-java's, which Testcontainers'
//! images from Dockerfiles use). Their context, the call's body or a URL, is built by
//! shards' builder in a process of its own, as `shards build -` builds one: each RUN step
//! in a microVM, never a container on the host's kernel, by BuildKit's plan and cache,
//! where dockerd answers the API's first version, which those clients ask for, with its
//! legacy builder.

use std::ffi::OsStr;
use std::io::{BufRead, Read as _};
#[cfg(unix)]
use std::os::fd::AsRawFd as _;

use base64::Engine as _;
use serde_json::{Value, json};
use shards_ipc::Stream as UnixStream;
use shards_ipc::fd::AsFd as _;

use super::http::{Answer, Stream};
use super::{Call, Done, at_least, plain, refused};
use crate::build::proxy::head::Body;
use crate::containers::Disk;
use crate::daemon::{Daemon, log};

/// A JSON query parameter, `key`'s, read as `T`: none where it is not given.
fn json_query<T: serde::de::DeserializeOwned>(
    call: &Call<'_>,
    key: &str,
    what: &str,
) -> Result<Option<T>, String> {
    match call.query(key).filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(v) => serde_json::from_str(v)
            .map(Some)
            .map_err(|e| format!("error reading {what}: {e}")),
    }
}

/// The build's command line, as `shards build image` takes a build's options, of the
/// call's query (moby newImageBuildOptions): its context last, `-` for the body.
fn build_args(call: &Call<'_>, iid: &OsStr) -> Result<Vec<std::ffi::OsString>, String> {
    let mut argv: Vec<std::ffi::OsString> = vec!["build".into(), "--progress".into(), "plain".into()];
    argv.extend(["--iidfile".into(), iid.to_os_string()]);
    let mut flag = |name: &str, value: Option<&str>| {
        argv.push(format!("--{name}").into());
        if let Some(v) = value {
            argv.push(v.into());
        }
    };
    for (key, name) in [
        ("dockerfile", "file"),
        ("target", "target"),
        ("platform", "platform"),
        ("networkmode", "network"),
        ("shmsize", "shm-size"),
        ("isolation", "isolation"),
        ("cgroupparent", "cgroup-parent"),
        ("cpusetcpus", "cpuset-cpus"),
        ("cpusetmems", "cpuset-mems"),
    ] {
        if let Some(v) = call.query(key).filter(|v| !v.is_empty()) {
            flag(name, Some(v));
        }
    }
    for (key, name) in [
        ("memory", "memory"),
        ("memswap", "memory-swap"),
        ("cpushares", "cpu-shares"),
        ("cpuperiod", "cpu-period"),
        ("cpuquota", "cpu-quota"),
    ] {
        if let Some(v) = call.query(key).filter(|v| !v.is_empty() && *v != "0") {
            flag(name, Some(v));
        }
    }
    for (key, name) in [
        ("t", "tag"),
        ("extrahosts", "add-host"),
        ("securityopt", "security-opt"),
    ] {
        for v in call.all(key).filter(|v| !v.is_empty()) {
            flag(name, Some(v));
        }
    }
    for (key, name) in [
        ("nocache", "no-cache"),
        ("pull", "pull"),
        ("squash", "squash"),
        ("forcerm", "force-rm"),
    ] {
        if call.flag(key) {
            flag(name, None);
        }
    }
    if call.query("rm").is_some_and(|v| !v.is_empty()) && !call.flag("rm") {
        flag("rm=false", None);
    }
    // `--build-arg NAME` alone, where its value is null: one the client named, its value
    // from no environment, which the build warns of where no ARG takes it.
    if let Some(args) =
        json_query::<std::collections::BTreeMap<String, Option<String>>>(call, "buildargs", "build args")?
    {
        for (k, v) in args {
            flag("build-arg", Some(&v.map_or(k.clone(), |v| format!("{k}={v}"))));
        }
    }
    if let Some(labels) = json_query::<std::collections::BTreeMap<String, String>>(call, "labels", "labels")?
    {
        for (k, v) in labels {
            flag("label", Some(&format!("{k}={v}")));
        }
    }
    if let Some(from) = json_query::<Vec<String>>(call, "cachefrom", "cache-from")? {
        for f in from {
            flag("cache-from", Some(&f));
        }
    }
    if let Some(limits) = json_query::<Vec<Value>>(call, "ulimits", "ulimit settings")? {
        for l in limits {
            let name = l.get("Name").and_then(Value::as_str).unwrap_or_default();
            let (soft, hard) = (
                l.get("Soft").and_then(Value::as_i64).unwrap_or(0),
                l.get("Hard").and_then(Value::as_i64).unwrap_or(0),
            );
            flag("ulimit", Some(&format!("{name}={soft}:{hard}")));
        }
    }
    if let Some(outputs) = json_query::<Vec<Value>>(call, "outputs", "outputs")? {
        for o in outputs {
            let mut parts = vec![format!(
                "type={}",
                o.get("Type").and_then(Value::as_str).unwrap_or_default()
            )];
            for (k, v) in o.get("Attrs").and_then(Value::as_object).into_iter().flatten() {
                parts.push(format!("{k}={}", v.as_str().unwrap_or_default()));
            }
            flag("output", Some(&parts.join(",")));
        }
    }
    argv.push(
        call.query("remote")
            .filter(|r| !r.is_empty())
            .unwrap_or("-")
            .into(),
    );
    Ok(argv)
}

/// The registry credentials a build's client gives (`X-Registry-Config`, base64url JSON of
/// an AuthConfig by registry), as a client's environment gives them (`DOCKER_AUTH_CONFIG`):
/// in memory alone. None where it gives none; dockerd ignores one it cannot read.
fn auth_configs(header: Option<&str>) -> Option<String> {
    let header = header.map(str::trim).filter(|h| !h.is_empty())?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(header.trim_end_matches('='))
        .ok()?;
    let given: serde_json::Map<String, Value> = serde_json::from_slice(&bytes).ok()?;
    let mut auths = serde_json::Map::new();
    for (registry, config) in given {
        let text = |k: &str| {
            config
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let mut entry = serde_json::Map::new();
        let auth = match text("auth") {
            a if !a.is_empty() => a,
            _ if !text("username").is_empty() || !text("password").is_empty() => {
                base64::engine::general_purpose::STANDARD.encode(format!(
                    "{}:{}",
                    text("username"),
                    text("password")
                ))
            }
            _ => String::new(),
        };
        if !auth.is_empty() {
            entry.insert("auth".into(), json!(auth));
        }
        for key in ["identitytoken", "registrytoken"] {
            if !text(key).is_empty() {
                entry.insert(key.into(), json!(text(key)));
            }
        }
        if !entry.is_empty() {
            auths.insert(registry, Value::Object(entry));
        }
    }
    (!auths.is_empty()).then(|| json!({ "auths": auths }).to_string())
}

/// Whether the peer of `conn`, which has something to read, has gone: what it has is its
/// end, which a pipe's stream says as readable with nothing to peek.
#[cfg(windows)]
fn peer_gone(conn: &UnixStream) -> bool {
    conn.readable() && matches!(conn.peek(), Ok(None))
}

/// Whether the peer of `conn`, which has something to read, has gone: what it has is its
/// end.
#[cfg(unix)]
fn peer_gone(conn: &UnixStream) -> bool {
    let mut byte = [0u8; 1];
    // SAFETY: recv(2) of at most one byte into a buffer of one, peeked, without blocking.
    let n = unsafe {
        libc::recv(
            conn.as_raw_fd(),
            byte.as_mut_ptr().cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    n == 0
}

/// Whether a read of `said` would not wait: Unix's reads of it do not (O_NONBLOCK), and
/// on Windows, whose anonymous pipes have no such mode but a LAN Manager one Microsoft
/// says not to use (SetNamedPipeHandleState), whether it has bytes or has ended.
#[cfg(unix)]
fn reads_now(_: &std::io::PipeReader) -> bool {
    true
}

#[cfg(windows)]
fn reads_now(said: &std::io::PipeReader) -> bool {
    shards_ipc::readable_raw(std::os::windows::io::AsRawHandle::as_raw_handle(said))
}

/// What a failed build said last: its `ERROR:` lines, else its last line.
fn failure(said: &[String]) -> String {
    let errors: Vec<&str> = said.iter().filter_map(|l| l.strip_prefix("ERROR: ")).collect();
    match errors.last() {
        Some(e) => (*e).to_string(),
        None => said
            .last()
            .cloned()
            .unwrap_or_else(|| "the build failed".to_string()),
    }
}

impl<D: Disk> Daemon<D> {
    /// `POST /build`: `shards build image` of the context the body holds, or `remote`'s,
    /// by the query's options (each the flag `shards build image` takes it by; one it does
    /// not serve refused in its words). Each line it says a `stream` message, from its
    /// first, so that one refused before it says anything is answered by its status; then
    /// the image's ID (`aux`), and that it was built and tagged, as clients find a build's
    /// image by; with `q`, the ID alone. Its client's registry credentials
    /// (`X-Registry-Config`) stay in memory. A client that goes ends the build. A build that
    /// takes its context through a BuildKit session (buildx's) is refused: its session API
    /// is not served yet.
    pub(super) fn engine_build<R: BufRead>(
        &self,
        conn: &UnixStream,
        call: &Call<'_>,
        body: &mut Body<'_, R>,
    ) -> Done {
        match call.query("version").unwrap_or_default() {
            "" | "1" | "2" => {}
            v => return Done::Answer(Answer::error(400, format!("invalid version \"{v}\""))),
        }
        if call.query("session").is_some_and(|s| !s.is_empty()) {
            return Done::Answer(Answer::error(
                400,
                "session: a context sent through a BuildKit session (buildx's) is not served yet: send it as the call's body",
            ));
        }
        let iid = self.home.join(crate::daemon::PRIVATE).join(format!(
            "build-{}.iid",
            crate::containers::new_id().unwrap_or_default()
        ));
        if let Some(dir) = iid.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            return Done::Answer(Answer::error(500, format!("{}: {e}", dir.display())));
        }
        let args = match build_args(call, iid.as_os_str()) {
            Ok(a) => a,
            Err(e) => return Done::Answer(Answer::error(400, e)),
        };
        let mut env: Vec<(String, std::ffi::OsString)> = shards_ipc::registry_env()
            .into_iter()
            .chain(crate::cloud_environment())
            .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.into())))
            .collect();
        if let Some(config) = auth_configs(call.head.get("X-Registry-Config")) {
            env.retain(|(k, _)| k != "DOCKER_AUTH_CONFIG");
            env.push(("DOCKER_AUTH_CONFIG".into(), config.into()));
        }
        env.push(("SHARDS_HOME".into(), self.home.clone().into()));
        for name in ["SHARDS_KERNEL", "SHARDS_INIT"] {
            if let Some(v) = std::env::var_os(name).filter(|v| std::path::Path::new(v).is_absolute()) {
                env.push((name.into(), v));
            }
        }
        let built = self.build_in_child(conn, call, body, &args, &env, &iid);
        let _ = std::fs::remove_file(&iid);
        built
    }

    /// Runs `shards build` with `args` and `env`, the body its stdin, and answers as
    /// [`engine_build`](Self::engine_build) says.
    fn build_in_child<R: BufRead>(
        &self,
        conn: &UnixStream,
        call: &Call<'_>,
        body: &mut Body<'_, R>,
        args: &[std::ffi::OsString],
        env: &[(String, std::ffi::OsString)],
        iid: &std::path::Path,
    ) -> Done {
        let remote = call.query("remote").is_some_and(|r| !r.is_empty());
        let pieces = std::io::pipe().and_then(|input| Ok((input, std::io::pipe()?)));
        let ((stdin_r, mut stdin_w), (said_r, said_w)) = match pieces {
            Ok(p) => p,
            Err(e) => return Done::Answer(Answer::error(500, format!("a pipe: {e}"))),
        };
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => return Done::Answer(Answer::error(500, format!("this binary: {e}"))),
        };
        let argv: Vec<&OsStr> = args.iter().map(std::ffi::OsString::as_os_str).collect();
        let vars: Vec<(&str, &OsStr)> = env.iter().map(|(k, v)| (k.as_str(), v.as_os_str())).collect();
        let child = match shards_ipc::spawn_in(
            &exe,
            &argv,
            &[(stdin_r.as_fd(), 0), (said_w.as_fd(), 1), (said_w.as_fd(), 2)],
            false,
            &vars,
        ) {
            Ok(child) => child,
            Err(e) => return Done::Answer(Answer::error(500, format!("starting the build: {e}"))),
        };
        drop((stdin_r, said_w));
        // Its context in as it comes, before anything is answered, as dockerd holds its
        // answer until the call's body is read.
        if !remote {
            let _ = std::io::copy(body, &mut stdin_w);
        }
        drop(stdin_w);
        let quiet = call.flag("q");
        let mut said: Vec<String> = Vec::new();
        let mut stream: Option<Stream<'_>> = None;
        // Each line it says as it comes, until it has ended: read without blocking, so
        // that a process of its own still holding its output (a builder's VM) holds up
        // nothing once the build has ended.
        // SAFETY: fcntl(2) of a descriptor this process holds.
        #[cfg(unix)]
        unsafe {
            let flags = libc::fcntl(said_r.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(said_r.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let (mut pending, mut buf) = (Vec::new(), vec![0u8; 1 << 14]);
        let mut status: Option<i32> = None;
        // Its client watched until it sends something (its next call), which it reads after.
        let mut watched = true;
        let gone = loop {
            let mut gone = false;
            while let Some(at) = pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = pending.drain(..=at).collect();
                let text = String::from_utf8_lossy(line.get(..at).unwrap_or_default()).into_owned();
                if !quiet {
                    if stream.is_none() {
                        match Stream::start(conn, call.head, 200, "application/json") {
                            Ok(s) => stream = Some(s),
                            Err(_) => gone = true,
                        }
                    }
                    if let Some(out) = stream.as_mut()
                        && out.json(&json!({ "stream": format!("{text}\n") })).is_err()
                    {
                        gone = true;
                    }
                }
                said.push(text);
                if gone {
                    break;
                }
            }
            if gone || status.is_some() {
                break gone;
            }
            // What it says, and its client, which says nothing more until its answer but by
            // going. On Windows each is looked at, and 100 ms slept where neither has anything:
            // a line comes up to 100 ms after it is said, where poll(2) wakes as it comes. The
            // plan's step 8, which serves the Engine API there, waits on the pipe itself.
            #[cfg(windows)]
            {
                let said_ready = reads_now(&said_r);
                if watched && conn.readable() {
                    if peer_gone(conn) {
                        break true;
                    }
                    watched = false;
                } else if !said_ready {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            #[cfg(unix)]
            let mut polled = [
                libc::pollfd {
                    fd: said_r.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if watched { conn.as_raw_fd() } else { -1 },
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: poll(2) on two pollfds of descriptors this process holds (one -1, which
            // poll skips).
            #[cfg(unix)]
            unsafe {
                libc::poll(polled.as_mut_ptr(), 2, 100)
            };
            #[cfg(unix)]
            {
                let [_, client] = polled;
                if client.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                    break true;
                }
                if client.revents & libc::POLLIN != 0 {
                    if peer_gone(conn) {
                        break true;
                    }
                    watched = false;
                }
            }
            let mut closed = false;
            loop {
                if !reads_now(&said_r) {
                    break;
                }
                match (&said_r).read(&mut buf) {
                    Ok(0) => {
                        closed = true;
                        break;
                    }
                    Ok(n) => pending.extend_from_slice(buf.get(..n).unwrap_or_default()),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        closed = true;
                        break;
                    }
                }
            }
            if closed {
                status = Some(child.wait().unwrap_or(-1));
            } else if let Some(ended) = child.try_wait() {
                // Ended: what it said before, read to its end, and nothing after.
                while reads_now(&said_r)
                    && let Ok(n) = (&said_r).read(&mut buf)
                {
                    if n == 0 {
                        break;
                    }
                    pending.extend_from_slice(buf.get(..n).unwrap_or_default());
                }
                status = Some(ended);
            }
            if status.is_some() && !pending.is_empty() && !pending.ends_with(b"\n") {
                pending.push(b'\n');
            }
        };
        if gone {
            // Its client has gone: the build ends, as dockerd's ends with its request.
            let _ = child.kill(shards_ipc::signal::SIGTERM);
            if let Err(e) = child.wait() {
                log(format!("a build whose client went: {e}"));
            }
            return Done::Close;
        }
        if status != Some(0) {
            let why = plain(&failure(&said));
            return match stream {
                None => Done::Answer(refused(&why, 500)),
                Some(mut out) => match out
                    .json(&json!({ "errorDetail": { "message": why }, "error": why }))
                    .and_then(|()| out.end())
                {
                    Ok(()) => Done::Keep,
                    Err(_) => Done::Close,
                },
            };
        }
        let mut id = String::new();
        if let Err(e) = std::fs::File::open(iid).and_then(|mut f| f.read_to_string(&mut id)) {
            return Done::Answer(Answer::error(500, format!("the built image's ID: {e}")));
        }
        let id = id.trim().to_string();
        let mut out = match stream {
            Some(out) => out,
            None => match Stream::start(conn, call.head, 200, "application/json") {
                Ok(s) => s,
                Err(_) => return Done::Close,
            },
        };
        let mut messages = Vec::new();
        if quiet {
            messages.push(json!({ "stream": format!("{id}\n") }));
        } else {
            if at_least(&call.version, "1.30") {
                messages.push(json!({ "aux": { "ID": id } }));
            }
            // As docker-py and docker-java find a build's image by.
            let short = id.trim_start_matches("sha256:").get(..12).unwrap_or_default();
            messages.push(json!({ "stream": format!("Successfully built {short}\n") }));
            for tag in call.all("t").filter(|t| !t.is_empty()) {
                let shown = shards_image::reference::Reference::parse_normalized(tag)
                    .map(|r| r.tag_name_only().familiar())
                    .unwrap_or_else(|_| tag.to_string());
                messages.push(json!({ "stream": format!("Successfully tagged {shown}\n") }));
            }
        }
        match messages
            .iter()
            .try_for_each(|m| out.json(m))
            .and_then(|()| out.end())
        {
            Ok(()) => Done::Keep,
            Err(_) => Done::Close,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A build's credentials by registry, as a client's environment gives them: a user's
    /// name and password as its `auth`, a token as itself, an empty or unread one none.
    #[test]
    fn a_builds_credentials_are_a_clients_auth_config() {
        let header = |json: &str| base64::engine::general_purpose::URL_SAFE.encode(json);
        assert_eq!(auth_configs(None), None);
        assert_eq!(auth_configs(Some("!!")), None);
        assert_eq!(auth_configs(Some(&header("{}"))), None);
        assert_eq!(
            auth_configs(Some(&header(
                r#"{"r.example":{"username":"u","password":"p"},"t.example":{"identitytoken":"t"}}"#
            )))
            .unwrap(),
            r#"{"auths":{"r.example":{"auth":"dTpw"},"t.example":{"identitytoken":"t"}}}"#
        );
    }

    /// What a failed build is said to have failed by: its last ERROR line.
    #[test]
    fn a_failed_build_says_its_last_error() {
        let said = [
            "#1 step".to_string(),
            "ERROR: first".into(),
            "ERROR: last".into(),
            "done".into(),
        ];
        assert_eq!(failure(&said), "last");
        assert_eq!(failure(&["only".to_string()]), "only");
        assert_eq!(failure(&[]), "the build failed");
    }
}
