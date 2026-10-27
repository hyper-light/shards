//! Images' calls (D170): pulled, inspected and listed as shards' CLI asks for each.

use std::io::BufRead;

use base64::Engine as _;
use serde_json::{Value, json};
use shards_ipc::Stream as UnixStream;
use shards_ipc::fd::AsFd as _;
use shards_ipc::kind;

use super::http::{self, Answer, Stream};
use super::{Call, Done, plain, refused};
use crate::build::proxy::head::Body;
use crate::containers::Disk;
use crate::daemon::inspect::Missing;
use crate::daemon::{Daemon, Threads};

/// The registry credentials a pull's client gives (`X-Registry-Auth`, a registry's
/// AuthConfig as base64url JSON), as a client's environment gives them
/// (`DOCKER_AUTH_CONFIG`): in memory alone. `None` where it gives none.
fn auth_env(header: Option<&str>, reference: &str) -> Result<Option<String>, String> {
    let Some(header) = header.map(str::trim).filter(|h| !h.is_empty()) else {
        return Ok(None);
    };
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(header.trim_end_matches('='))
        .map_err(|e| format!("X-Registry-Auth: {e}"))?;
    let given: Value = serde_json::from_slice(&bytes).map_err(|e| format!("X-Registry-Auth: {e}"))?;
    let text = |k: &str| {
        given
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
    if entry.is_empty() {
        return Ok(None);
    }
    // Its registry: the one it names, else the reference's (Docker Hub's by its index).
    let server = match text("serveraddress") {
        s if !s.is_empty() => s,
        _ => match shards_image::reference::Reference::parse_normalized(reference) {
            Ok(r) if r.domain == "docker.io" => "https://index.docker.io/v1/".to_string(),
            Ok(r) => r.domain.clone(),
            Err(e) => return Err(format!("{reference}: {e}")),
        },
    };
    Ok(Some(json!({ "auths": { server: entry } }).to_string()))
}

/// The client's registry environment, with the credentials a call's client gives
/// (X-Registry-Auth) for `reference`'s registry as its `DOCKER_AUTH_CONFIG`: in memory
/// alone, never written.
fn with_auth(call: &Call<'_>, reference: &str) -> Result<Vec<String>, String> {
    with_auth_over(shards_ipc::registry_env(), call, reference)
}

/// [`with_auth`], over `env`: a microVM's own engine's run's client's (D181).
fn with_auth_over(mut env: Vec<String>, call: &Call<'_>, reference: &str) -> Result<Vec<String>, String> {
    if let Some(config) = auth_env(call.head.get("X-Registry-Auth"), reference)? {
        env.retain(|e| !e.starts_with("DOCKER_AUTH_CONFIG="));
        env.push(format!("DOCKER_AUTH_CONFIG={config}"));
    }
    Ok(env)
}

/// The image a call's `repo` and `tag` name (httputils.RepoTagReference): none without a
/// repository; its tag `latest` where neither names one.
fn repo_tag(call: &Call<'_>) -> Result<Option<String>, String> {
    let repo = call.query("repo").unwrap_or_default();
    if repo.is_empty() {
        return Ok(None);
    }
    let mut named = shards_image::reference::Reference::parse_normalized(repo)
        .map_err(|e| format!("repo {repo}: {e}"))?;
    if named.digest.is_some() {
        return Err(format!(
            "repo {repo}: an image is named by its repository and tag, not by a digest"
        ));
    }
    if let Some(tag) = call.query("tag").filter(|t| !t.is_empty()) {
        named.tag = Some(tag.to_string());
    }
    Ok(Some(named.tag_name_only().to_string()))
}

/// A line a pull or push says, as dockerd's JSON message of it (streamformatter): a
/// layer's `ID: STATUS` by its ID, with its progress; a tag's `TAG: Pulling from
/// REPOSITORY` by the tag; the rest a status alone. None for an empty line.
fn message(line: &str) -> Option<Value> {
    if line.trim().is_empty() {
        return None;
    }
    if let Some((id, status)) = line.split_once(": ") {
        if id.len() == 12
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Some(json!({ "status": status, "progressDetail": {}, "id": id }));
        }
        if status.starts_with("Pulling from ") {
            return Some(json!({ "status": status, "id": id }));
        }
    }
    Some(json!({ "status": line }))
}

/// What a push says, as dockerd's JSON messages of it: each line a [`message`]; a tag's
/// `TAG: digest: DIGEST size: SIZE` an `aux` too (PushResult); and its note of our
/// platform's manifest pushed alone the `aux` the docker CLI tells of it by
/// (auxprogress.ManifestPushedInsteadOfIndex).
fn push_messages(text: &str) -> Vec<Value> {
    if let Some(at) = text.find(crate::daemon::push::SINGLE_PLATFORM) {
        let digests = text
            .get(at..)
            .and_then(|rest| rest.lines().nth(1))
            .and_then(|line| line.trim().split_once(" -> "));
        if let Some((index, manifest)) = digests {
            return vec![json!({
                "progressDetail": {},
                "aux": {
                    "manifestPushedInsteadOfIndex": true,
                    "originalIndex": { "mediaType": "application/vnd.oci.image.index.v1+json", "digest": index.trim(), "size": 0 },
                    "selectedManifest": { "mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": manifest.trim(), "size": 0 },
                },
            })];
        }
    }
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(m) = message(line) else {
            continue;
        };
        out.push(m);
        let pushed = line.split_once(": digest: ").and_then(|(tag, rest)| {
            rest.split_once(" size: ")
                .map(|(digest, size)| (tag, digest, size))
        });
        if let Some((tag, digest, size)) = pushed
            && let Ok(size) = size.trim().parse::<i64>()
        {
            out.push(json!({ "progressDetail": {}, "aux": { "Tag": tag, "Digest": digest, "Size": size } }));
        }
    }
    out
}

/// The answer of command `ipc`, a pull's or push's, as a stream of JSON messages, each
/// line it says made messages by `made`: its head written with its first, so that one
/// refused before it says anything is answered by its status, as dockerd answers one
/// whose stream it has not flushed; a failure after that is the stream's last message.
fn messages(conn: &UnixStream, call: &Call<'_>, ipc: &UnixStream, made: &dyn Fn(&str) -> Vec<Value>) -> Done {
    messages_told(conn, call, ipc, made).0
}

/// [`messages`], and whether what they said was done (its status 0).
fn messages_told(
    conn: &UnixStream,
    call: &Call<'_>,
    ipc: &UnixStream,
    made: &dyn Fn(&str) -> Vec<Value>,
) -> (Done, bool) {
    let mut stream: Option<Stream<'_>> = None;
    let mut said = Vec::new();
    let status = loop {
        match shards_ipc::recv(ipc) {
            Ok(Some(m)) if m.kind == kind::OUT => {
                let made = made(&String::from_utf8_lossy(&m.payload));
                if made.is_empty() {
                    continue;
                }
                if stream.is_none() {
                    match Stream::start(conn, call.head, 200, "application/json") {
                        Ok(s) => stream = Some(s),
                        Err(_) => return (Done::Close, false),
                    }
                }
                let Some(out) = stream.as_mut() else {
                    return (Done::Close, false);
                };
                if made.iter().try_for_each(|m| out.json(m)).is_err() {
                    return (Done::Close, false);
                }
            }
            Ok(Some(m)) if m.kind == kind::ERR => said.extend(m.payload),
            Ok(Some(m)) if m.kind == kind::END => break m.payload.first().copied().unwrap_or(1),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return (Done::Close, false),
        }
    };
    let said = plain(&String::from_utf8_lossy(&said));
    let mut out = match stream {
        Some(out) => out,
        None if status != 0 => return (Done::Answer(refused(&said, 500)), false),
        None => match Stream::start(conn, call.head, 200, "application/json") {
            Ok(s) => s,
            Err(_) => return (Done::Close, status == 0),
        },
    };
    let sent = if status == 0 {
        said.lines().filter_map(message).try_for_each(|m| out.json(&m))
    } else {
        out.json(&json!({ "errorDetail": { "message": said }, "error": said }))
    };
    let done = match sent.and_then(|()| out.end()) {
        Ok(()) => Done::Keep,
        Err(_) => Done::Close,
    };
    (done, status == 0)
}

/// An OCI platform as the API gives one (JSON: `os`, `architecture`, `variant`), as the
/// CLI's `--platform` takes it (`os/arch[/variant]`); one that is not JSON as given.
fn platform_arg(given: &str) -> String {
    let Ok(p) = serde_json::from_str::<Value>(given) else {
        return given.to_string();
    };
    let part = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or_default();
    [part("os"), part("architecture"), part("variant")]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

impl<D: Disk> Daemon<D> {
    /// `POST /images/create`: `fromImage` pulled as `shards pull` pulls it, each line it
    /// says a JSON message of the stream, as dockerd's are; with `fromSrc`, an import.
    pub(super) fn engine_pull<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: &UnixStream,
        call: &Call<'_>,
    ) -> Done {
        if call.query("fromSrc").is_some_and(|s| !s.is_empty()) {
            return self.engine_import::<std::io::Empty>(threads, conn, call, None);
        }
        self.pull_with(threads, conn, call, shards_ipc::registry_env()).0
    }

    /// A pull of what `call` names, its registries reached with `env` and what its
    /// `X-Registry-Auth` gives over it; and, where it was done, the reference pulled and
    /// whether that was each of its repository's tags.
    pub(super) fn pull_with<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: &UnixStream,
        call: &Call<'_>,
        env: Vec<String>,
    ) -> (Done, Option<(String, bool)>) {
        // `pull -a`'s name may end in `:` (moby postImagesCreate's special case).
        let image = call.query("fromImage").unwrap_or_default().trim_end_matches(':');
        if image.is_empty() {
            return (
                Done::Answer(Answer::error(400, "a pull names its image (fromImage)")),
                None,
            );
        }
        // A tag or a digest given apart; none at all is every tag (moby postImagesCreate).
        let tag = call.query("tag").unwrap_or_default();
        let reference = match tag {
            "" => image.to_string(),
            t if t.contains(':') => format!("{image}@{t}"),
            t => format!("{image}:{t}"),
        };
        let named_only = tag.is_empty()
            && shards_image::reference::Reference::parse_normalized(image)
                .is_ok_and(|r| r.familiar() == r.familiar_name());
        let env = match with_auth_over(env, call, &reference) {
            Ok(env) => env,
            Err(e) => return (Done::Answer(Answer::error(400, e)), None),
        };
        let mut argv = vec!["pull"];
        if let Some(p) = call.query("platform").filter(|p| !p.is_empty()) {
            argv.extend(["--platform", p]);
        }
        if named_only {
            argv.push("--all-tags");
        }
        argv.push(&reference);
        let ipc = match self.command_conn_with(threads, &argv, env, &[]) {
            Ok(ipc) => ipc,
            Err(e) => return (Done::Answer(Answer::error(500, e)), None),
        };
        let (done, pulled) = messages_told(conn, call, &ipc, &|text| {
            text.lines().filter_map(message).collect()
        });
        (done, pulled.then_some((reference, named_only)))
    }

    /// `POST /auth`: the credentials a client gives, checked with their registry as
    /// `shards login registry` checks them (dockerd's RegistryLogin, loginV2), and kept by
    /// no one, as dockerd keeps none: the client keeps them. `Login Succeeded`, and the
    /// identity token its token server gave in place of the password, where it gave one
    /// (none said where none was, as `AuthResponse` omits it); credentials refused, 401,
    /// which the API's description gives them, by a token server or the registry itself,
    /// where dockerd answers the registry's own refusal 500.
    pub(super) fn engine_auth(&self, call: &Call<'_>) -> Answer {
        let body: Value = match serde_json::from_slice(&call.body) {
            Ok(body) => body,
            Err(e) => return Answer::error(400, format!("a login's body: {e}")),
        };
        let text = |k: &str| {
            body.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let server = match text("serveraddress").as_str() {
            "" | "docker.io" => crate::login::HUB.to_string(),
            s => s.to_string(),
        };
        let credentials = if !text("identitytoken").is_empty() {
            shards_registry::auth::Credentials::IdentityToken(text("identitytoken"))
        } else if !text("registrytoken").is_empty() {
            shards_registry::auth::Credentials::RegistryToken(text("registrytoken"))
        } else {
            shards_registry::auth::Credentials::Password {
                username: text("username"),
                password: text("password"),
            }
        };
        // Its certificates and proxies, as a pull's: the daemon's, which it has from the
        // client that started it.
        let env = |k: &str| std::env::var(k).ok();
        match crate::pull::check_login(crate::login::registry_host(&server), credentials, &env) {
            Ok(Some(token)) if !token.is_empty() => Answer::json(
                200,
                &json!({ "Status": "Login Succeeded", "IdentityToken": token }),
            ),
            Ok(_) => Answer::json(200, &json!({ "Status": "Login Succeeded" })),
            Err(e) if e.kind() == shards_registry::ErrorKind::Unauthorized => {
                Answer::error(401, e.to_string())
            }
            Err(e) => Answer::error(500, e.to_string()),
        }
    }

    /// `GET /images/search`: `shards search image`'s search (D172), as dockerd answers
    /// it: its `term`, `limit` and `filters`, its client's credentials (X-Registry-Auth)
    /// in memory alone. Docker Hub's results as its index gives them, and any other
    /// registry's through what it documents, where dockerd asks every registry for an
    /// index's search, which none serves now. A negative limit is refused, where dockerd
    /// answers 200 with no body (an errors.Wrap of no error).
    pub(super) fn engine_search(&self, call: &Call<'_>) -> Answer {
        let term = call.query("term").unwrap_or_default();
        let limit = match call.query("limit").filter(|l| !l.is_empty()) {
            None => 0,
            Some(given) => match given.parse::<i64>() {
                Ok(n) if n >= 0 => n,
                Ok(_) => return Answer::error(400, "invalid limit specified"),
                Err(_) => {
                    return Answer::error(
                        400,
                        format!(
                            "invalid limit specified: strconv.Atoi: parsing {}: invalid syntax",
                            shards_cmdline::go::quote(given)
                        ),
                    );
                }
            },
        };
        let filters = match super::containers::filters(call.query("filters")) {
            Ok(pairs) => {
                let given: Vec<String> = pairs.into_iter().map(|(k, v)| format!("{k}={v}")).collect();
                crate::filters::Filters::from_flags(&given)
            }
            Err(e) => return Answer::error(400, e),
        };
        // Credentials a client gives that cannot be read are none, as dockerd ignores
        // them for a search.
        let env = with_auth(call, term).unwrap_or_else(|_| shards_ipc::registry_env());
        let lookup = |k: &str| shards_ipc::env_value(&env, k);
        match crate::search::find(term, limit, &filters, &lookup) {
            Ok(found) => Answer::json(
                200,
                &Value::Array(
                    found
                        .into_iter()
                        .map(|f| {
                            json!({
                                "star_count": f.stars,
                                "is_official": f.official,
                                "name": f.name,
                                "is_automated": false,
                                "description": f.description,
                            })
                        })
                        .collect(),
                ),
            ),
            Err(refusal) => Answer::error(if refusal.asked { 400 } else { 500 }, refusal.text),
        }
    }

    /// `POST /images/{name}/push`: `shards push image`, its `tag` of `name`'s repository,
    /// or `name` as given, every tag of its repository where it names none; its client's
    /// credentials (X-Registry-Auth) in memory alone. Each layer said by its ID, the tag's
    /// digest and size an `aux` message too, and a push of our platform's manifest alone,
    /// where the index was not all here, the `aux` the docker CLI tells of it by, as
    /// dockerd's are (daemon/containerd/image_push.go; api/types/auxprogress).
    pub(super) fn engine_push<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: &UnixStream,
        name: &str,
        call: &Call<'_>,
    ) -> Done {
        use shards_image::reference::Reference;
        let reference = match call.query("tag").filter(|t| !t.is_empty()) {
            None => name.to_string(),
            Some(tag) => match Reference::parse_normalized(name) {
                Ok(mut r) => {
                    r.tag = Some(tag.to_string());
                    r.digest = None;
                    r.to_string()
                }
                Err(e) => return Done::Answer(Answer::error(400, format!("{name}: {e}"))),
            },
        };
        let name_only =
            Reference::parse_normalized(&reference).is_ok_and(|r| r.tag.is_none() && r.digest.is_none());
        let env = match with_auth(call, &reference) {
            Ok(env) => env,
            Err(e) => return Done::Answer(Answer::error(400, e)),
        };
        let platform = call.query("platform").filter(|p| !p.is_empty()).map(platform_arg);
        let mut argv = vec!["push"];
        if let Some(p) = &platform {
            argv.extend(["--platform", p]);
        }
        if name_only {
            argv.push("--all-tags");
        }
        argv.push(&reference);
        let ipc = match self.command_conn_with(threads, &argv, env, &[]) {
            Ok(ipc) => ipc,
            Err(e) => return Done::Answer(Answer::error(500, e)),
        };
        messages(conn, call, &ipc, &push_messages)
    }

    /// `GET /images/{name}/get`, `GET /images/get`: `shards save image`, the images as one
    /// archive, written onto the connection as it is made, never staged; each found
    /// before the head, as dockerd finds them all before it streams, so that one not here
    /// is answered 404.
    pub(super) fn engine_save<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: &UnixStream,
        names: &[String],
        call: &Call<'_>,
    ) -> Done {
        if names.is_empty() {
            return Done::Answer(Answer::error(400, "a save names its images (names)"));
        }
        let platforms: Vec<String> = call
            .all("platform")
            .filter(|p| !p.is_empty())
            .map(platform_arg)
            .collect();
        let mut argv = vec!["save"];
        for p in &platforms {
            argv.extend(["--platform", p]);
        }
        argv.extend(names.iter().map(String::as_str));
        // One the CLI does not serve, refused in its words before anything is written.
        if !platforms.is_empty() {
            return Done::Answer(match self.ask_command(threads, &argv) {
                Ok((_, _, err)) => refused(&String::from_utf8_lossy(&err), 400),
                Err(e) => Answer::error(500, e),
            });
        }
        let found = self.store().and_then(|store| {
            let Some(store) = store else {
                return Err(crate::daemon::images::not_found(
                    names.first().map_or("", String::as_str),
                ));
            };
            let named = store.named().map_err(|e| e.to_string())?;
            names
                .iter()
                .try_for_each(|n| crate::daemon::images::resolve(&named, n).map(drop))
        });
        if let Err(e) = found {
            return Done::Answer(refused(&e, 500));
        }
        if http::head_to_close(conn, 200, "application/x-tar", &[]).is_err() {
            return Done::Close;
        }
        let Ok(out) = shards_ipc::handed(conn) else {
            return Done::Close;
        };
        let _ = self.ask_command_with(threads, &argv, &[out.as_fd()]);
        shards_ipc::end(conn);
        Done::Close
    }

    /// `POST /images/load`: `shards load image`, the archive the body holds read as it
    /// comes, never staged; each image it held said (`stream`), as dockerd's containerd
    /// store says them, `quiet` or not, its failure the last message.
    pub(super) fn engine_load<'s, 'e, R: BufRead>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: &UnixStream,
        call: &Call<'_>,
        body: &mut Body<'_, R>,
    ) -> Done {
        let platforms: Vec<String> = call
            .all("platform")
            .filter(|p| !p.is_empty())
            .map(platform_arg)
            .collect();
        let mut argv = vec!["load"];
        for p in &platforms {
            argv.extend(["--platform", p]);
        }
        let (status, out, err) = match self.ask_with_body(threads, &argv, body) {
            Ok(said) => said,
            Err(e) => return Done::Answer(Answer::error(500, e)),
        };
        let Ok(mut stream) = Stream::start(conn, call.head, 200, "application/json") else {
            return Done::Close;
        };
        let mut sent = String::from_utf8_lossy(&out)
            .lines()
            .filter(|l| !l.is_empty())
            .try_for_each(|l| stream.json(&json!({ "stream": format!("{l}\n") })));
        if status != 0 {
            let said = plain(&String::from_utf8_lossy(&err));
            sent = sent
                .and_then(|()| stream.json(&json!({ "errorDetail": { "message": said }, "error": said })));
        }
        match sent.and_then(|()| stream.end()) {
            Ok(()) => Done::Keep,
            Err(_) => Done::Close,
        }
    }

    /// `POST /images/create` with `fromSrc`: `shards import`, the root filesystem the body
    /// holds (`-`), read as it comes, or one an http or https URL serves (moby: `http://`
    /// where it names no scheme); named by `repo` and `tag`, its `message` and Dockerfile
    /// `changes` its `-m` and `-c`, `platform` its own. What it says, then the image's ID,
    /// each a JSON message; refused before it says anything, answered by its status.
    pub(super) fn engine_import<'s, 'e, R: BufRead>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: &UnixStream,
        call: &Call<'_>,
        body: Option<&mut Body<'_, R>>,
    ) -> Done {
        let source = call.query("fromSrc").unwrap_or_default();
        let source = if source == "-" || source.contains("://") {
            source.to_string()
        } else {
            format!("http://{source}")
        };
        if source != "-" && !source.starts_with("http://") && !source.starts_with("https://") {
            return Done::Answer(Answer::error(
                400,
                format!("fromSrc {source}: an import reads an http or https URL, or the call's body (-)"),
            ));
        }
        let target = match repo_tag(call) {
            Ok(t) => t,
            Err(e) => return Done::Answer(Answer::error(400, e)),
        };
        let mut argv: Vec<String> = vec!["import".into()];
        if let Some(m) = call.query("message").filter(|m| !m.is_empty()) {
            argv.extend(["--message".into(), m.to_string()]);
        }
        for change in call.all("changes") {
            argv.extend(["--change".into(), change.to_string()]);
        }
        if let Some(p) = call.query("platform").filter(|p| !p.is_empty()) {
            argv.extend(["--platform".into(), p.to_string()]);
        }
        argv.push(source);
        argv.extend(target);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        let said = match body {
            Some(body) => self.ask_with_body(threads, &argv, body),
            None => self.ask_command(threads, &argv),
        };
        let (status, out, err) = match said {
            Ok(said) => said,
            Err(e) => return Done::Answer(Answer::error(500, e)),
        };
        let out = String::from_utf8_lossy(&out).into_owned();
        let err = plain(&String::from_utf8_lossy(&err));
        if status != 0 && out.trim().is_empty() {
            return Done::Answer(refused(&err, 500));
        }
        let Ok(mut stream) = Stream::start(conn, call.head, 200, "application/json") else {
            return Done::Close;
        };
        let mut sent = out
            .lines()
            .filter(|l| !l.is_empty())
            .try_for_each(|l| stream.json(&json!({ "status": l })));
        if status != 0 {
            sent =
                sent.and_then(|()| stream.json(&json!({ "errorDetail": { "message": err }, "error": err })));
        }
        match sent.and_then(|()| stream.end()) {
            Ok(()) => Done::Keep,
            Err(_) => Done::Close,
        }
    }

    /// `POST /commit`: `shards commit vm`, microVM `container` named by `repo` and `tag`,
    /// its `author`, `comment` and Dockerfile `changes` its `-a`, `-m` and `-c`, paused
    /// unless `pause` says not: the new image's ID. Its config is the microVM's, as the
    /// changes change it: a body that gives another is refused, as Dockerfile changes say
    /// each field a config has; an empty one, `{}`, which docker-py sends when given none,
    /// is none, where dockerd's merge of it drops the microVM's Shell and OnBuild.
    pub(super) fn engine_commit<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, call: &Call<'_>) -> Answer {
        let config: Value = if call.body.iter().all(u8::is_ascii_whitespace) {
            Value::Null
        } else {
            match serde_json::from_slice(&call.body) {
                Ok(v) => v,
                Err(e) => return Answer::error(400, format!("a commit's body: {e}")),
            }
        };
        if !(config.is_null() || config.as_object().is_some_and(serde_json::Map::is_empty)) {
            return Answer::error(
                400,
                "a commit's config is the microVM's, as its Dockerfile changes (changes) change it, as `shards commit vm --change` does: give each field as a change",
            );
        }
        let container = call.query("container").unwrap_or_default();
        let target = match repo_tag(call) {
            Ok(t) => t,
            Err(e) => return Answer::error(400, e),
        };
        let mut argv: Vec<String> = vec!["commit".into()];
        for (key, flag) in [("author", "--author"), ("comment", "--message")] {
            if let Some(v) = call.query(key).filter(|v| !v.is_empty()) {
                argv.extend([flag.into(), v.to_string()]);
            }
        }
        for change in call.all("changes") {
            argv.extend(["--change".into(), change.to_string()]);
        }
        if call.query("pause").is_some() && !call.flag("pause") {
            argv.push("--no-pause".into());
        }
        argv.push(container.to_string());
        argv.extend(target);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        match self.ask_command(threads, &argv) {
            Ok((0, out, _)) => Answer::json(201, &json!({ "Id": String::from_utf8_lossy(&out).trim() })),
            Ok((_, _, err)) => refused(&String::from_utf8_lossy(&err), 500),
            Err(e) => Answer::error(500, e),
        }
    }

    /// `POST /images/{name}/tag`: `shards tag image`, the query's `repo` named by its
    /// `tag`, `latest` where it gives none. One that names no repository is refused,
    /// where dockerd answers 200 and tags nothing (httputils.RepoTagReference's nil).
    pub(super) fn engine_image_tag<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        name: &str,
        call: &Call<'_>,
    ) -> Answer {
        let repo = call.query("repo").unwrap_or_default();
        if repo.is_empty() {
            return Answer::error(400, "a tag names its repository (repo)");
        }
        let mut target = match shards_image::reference::Reference::parse_normalized(repo) {
            Ok(r) => r,
            Err(e) => return Answer::error(400, format!("repo {repo}: {e}")),
        };
        if target.digest.is_some() {
            return Answer::error(400, "a tag names an image by name and tag, not by digest");
        }
        if let Some(tag) = call.query("tag").filter(|t| !t.is_empty()) {
            target.tag = Some(tag.to_string());
        }
        let target = target.to_string();
        match self.ask_command(threads, &["tag", name, &target]) {
            Ok((0, _, _)) => Answer::text(201, ""),
            Ok((_, _, err)) => refused(&String::from_utf8_lossy(&err), 400),
            Err(e) => Answer::error(500, e),
        }
    }

    /// `DELETE /images/{name}`: `shards remove image`, `force` its `-f` and `noprune` its
    /// `--no-prune`: each name untagged and each image deleted, as `rmi` says them. The
    /// microVM templates made of what goes, and their warm microVMs, go with the store's
    /// next collection, which only shards has.
    pub(super) fn engine_image_remove<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        name: &str,
        call: &Call<'_>,
    ) -> Answer {
        let platforms: Vec<String> = call
            .all("platforms")
            .filter(|p| !p.is_empty())
            .map(platform_arg)
            .collect();
        let mut argv = vec!["rmi"];
        if call.flag("force") {
            argv.push("--force");
        }
        if call.flag("noprune") {
            argv.push("--no-prune");
        }
        for p in &platforms {
            argv.extend(["--platform", p]);
        }
        argv.push(name);
        match self.ask_command(threads, &argv) {
            Ok((0, out, _)) => {
                let removed: Vec<Value> = String::from_utf8_lossy(&out)
                    .lines()
                    .filter_map(|l| {
                        l.strip_prefix("Untagged: ")
                            .map(|n| json!({ "Untagged": n }))
                            .or_else(|| l.strip_prefix("Deleted: ").map(|d| json!({ "Deleted": d })))
                    })
                    .collect();
                Answer::json(200, &Value::Array(removed))
            }
            Ok((_, _, err)) => refused(&String::from_utf8_lossy(&err), 500),
            Err(e) => Answer::error(500, e),
        }
    }

    /// `GET /images/{name}/history`: `shards history image`, each step the newest first,
    /// the image's ID and names on the newest, as dockerd's containerd store gives them; a
    /// layer's size its compressed size, where dockerd's is unpacked.
    pub(super) fn engine_image_history<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        name: &str,
        call: &Call<'_>,
    ) -> Answer {
        // One the CLI does not serve, refused in its words.
        if let Some(p) = call.query("platform").filter(|p| !p.is_empty()) {
            let p = platform_arg(p);
            return match self.ask_command(threads, &["history", "--platform", &p, name]) {
                Ok((_, _, err)) => refused(&String::from_utf8_lossy(&err), 400),
                Err(e) => Answer::error(500, e),
            };
        }
        let history = match self.history_of(name) {
            Ok(h) => h,
            Err(e) => return refused(&e, 500),
        };
        let tags = if history.tags.is_empty() {
            Value::Null
        } else {
            json!(history.tags)
        };
        let steps: Vec<Value> = history
            .steps
            .iter()
            .enumerate()
            .map(|(i, step)| {
                json!({
                    "Id": if i == 0 { history.id.as_str() } else { "<missing>" },
                    "Created": step.created,
                    "CreatedBy": step.by,
                    "Tags": if i == 0 { tags.clone() } else { Value::Null },
                    "Size": step.size,
                    "Comment": step.comment,
                })
            })
            .collect();
        Answer::json(200, &Value::Array(steps))
    }

    /// `POST /images/prune`: `shards prune image`. The names the filters keep, dangling
    /// ones alone unless `dangling=false`: what was untagged and deleted, and the bytes
    /// reclaimed, counted.
    pub(super) fn engine_image_prune(&self, call: &Call<'_>) -> Answer {
        let given = match super::containers::filters(call.query("filters")) {
            Ok(f) => f.into_iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(),
            Err(e) => return Answer::error(400, e),
        };
        let now = i128::try_from(crate::containers::now()).unwrap_or(i128::MAX);
        match self.prune_images(&given, now, i64::from(crate::cli::utc_offset())) {
            Ok((removed, bytes)) => {
                let removed: Vec<Value> = removed
                    .into_iter()
                    .map(|r| match r {
                        crate::daemon::rmi::Removed::Untagged(n) => json!({ "Untagged": n }),
                        crate::daemon::rmi::Removed::Deleted(d) => json!({ "Deleted": d }),
                    })
                    .collect();
                Answer::json(200, &json!({ "ImagesDeleted": removed, "SpaceReclaimed": bytes }))
            }
            Err(e) => refused(&e, 400),
        }
    }

    /// `GET /images/{name}/json`: its InspectResponse, as `image inspect` gives it.
    pub(super) fn engine_image_inspect(&self, name: &str) -> Answer {
        match self.image_doc(name) {
            Ok(doc) => Answer::document(200, crate::daemon::inspect_doc::json(&doc)),
            Err(Missing::Not(e)) => Answer::error(404, plain(&e)),
            Err(Missing::Failed(e)) => Answer::error(500, plain(&e)),
        }
    }

    /// `GET /images/json`: the store's images, as the API sums each up (image.Summary),
    /// by the query's filters, the intermediate ones with `all`.
    pub(super) fn engine_images(&self, call: &Call<'_>) -> Answer {
        let given = match super::containers::filters(call.query("filters")) {
            Ok(f) => f.into_iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(),
            Err(e) => return Answer::error(400, e),
        };
        let filters = crate::daemon::filters::Filters::from_flags(&given);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i128::try_from(d.as_nanos()).unwrap_or(i128::MAX))
            .unwrap_or(0);
        let images = match self.listed(&filters, call.flag("all"), now, 0) {
            Ok(images) => images,
            Err(e) => return Answer::error(400, plain(&e)),
        };
        let summaries: Vec<Value> = images.iter().map(|i| self.image_summary(i, -1)).collect();
        Answer::json(200, &Value::Array(summaries))
    }

    /// Image `i` as the API sums one up (image.Summary), its size shared with others
    /// `shared` (-1 where not counted).
    pub(super) fn image_summary(&self, i: &crate::daemon::images::Summary, shared: i64) -> Value {
        let labels = self
            .image_doc(&i.id)
            .ok()
            .and_then(|doc| serde_json::from_str::<Value>(&crate::daemon::inspect_doc::json(&doc)).ok())
            .and_then(|doc| doc.pointer("/Config/Labels").cloned())
            .unwrap_or_else(|| json!({}));
        json!({
            "Id": i.id,
            "ParentId": "",
            "RepoTags": i.tags,
            "RepoDigests": i.digests,
            "Created": i.created,
            "Size": i.size,
            "SharedSize": shared,
            "Labels": if labels.is_null() { json!({}) } else { labels },
            "Containers": i.containers,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_pulls_credentials_are_a_clients_auth_config() {
        let header = |json: &str| base64::engine::general_purpose::URL_SAFE.encode(json);
        assert_eq!(auth_env(None, "alpine"), Ok(None));
        assert_eq!(auth_env(Some(&header("{}")), "alpine"), Ok(None));
        let hub = auth_env(Some(&header(r#"{"username":"u","password":"p"}"#)), "alpine")
            .unwrap()
            .unwrap();
        assert_eq!(
            hub,
            r#"{"auths":{"https://index.docker.io/v1/":{"auth":"dTpw"}}}"#
        );
        let own = auth_env(
            Some(&header(r#"{"identitytoken":"t","serveraddress":"r.example"}"#)),
            "r.example/a",
        )
        .unwrap()
        .unwrap();
        assert_eq!(own, r#"{"auths":{"r.example":{"identitytoken":"t"}}}"#);
        assert!(auth_env(Some("!!"), "alpine").is_err());
    }
}
