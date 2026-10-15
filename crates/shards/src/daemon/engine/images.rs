//! Images' calls (D170): pulled, inspected and listed as shards' CLI asks for each.

use std::os::unix::net::UnixStream;

use base64::Engine as _;
use serde_json::{Value, json};
use shards_ipc::kind;

use super::http::{Answer, Stream};
use super::{Call, Done, plain};
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

impl<D: Disk> Daemon<D> {
    /// `POST /images/create`: `fromImage` pulled as `shards pull` pulls it, each line it
    /// says a JSON message of the stream, as dockerd's are, its failure the last.
    pub(super) fn engine_pull<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: &UnixStream,
        call: &Call<'_>,
    ) -> Done {
        if call.query("fromSrc").is_some_and(|s| !s.is_empty()) {
            return Done::Answer(Answer::error(
                501,
                "fromSrc: an image is imported with `shards import`; the API's import is not served yet",
            ));
        }
        let image = call.query("fromImage").unwrap_or_default();
        if image.is_empty() {
            return Done::Answer(Answer::error(400, "a pull names its image (fromImage)"));
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
        let mut env = shards_ipc::registry_env();
        match auth_env(call.head.get("X-Registry-Auth"), &reference) {
            Ok(Some(config)) => {
                env.retain(|e| !e.starts_with("DOCKER_AUTH_CONFIG="));
                env.push(format!("DOCKER_AUTH_CONFIG={config}"));
            }
            Ok(None) => {}
            Err(e) => return Done::Answer(Answer::error(400, e)),
        }
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
            Err(e) => return Done::Answer(Answer::error(500, e)),
        };
        let Ok(mut stream) = Stream::start(conn, call.head, 200, "application/json") else {
            return Done::Close;
        };
        let mut said = Vec::new();
        let status = loop {
            match shards_ipc::recv(&ipc) {
                Ok(Some(m)) if m.kind == kind::OUT => {
                    for line in String::from_utf8_lossy(&m.payload)
                        .lines()
                        .filter(|l| !l.is_empty())
                    {
                        if stream.json(&json!({ "status": line })).is_err() {
                            return Done::Close;
                        }
                    }
                }
                Ok(Some(m)) if m.kind == kind::ERR => said.extend(m.payload),
                Ok(Some(m)) if m.kind == kind::END => break m.payload.first().copied().unwrap_or(1),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return Done::Close,
            }
        };
        let said = plain(&String::from_utf8_lossy(&said));
        let sent = if status == 0 {
            said.lines()
                .filter(|l| !l.is_empty())
                .try_for_each(|l| stream.json(&json!({ "status": l })))
        } else {
            stream.json(&json!({ "errorDetail": { "message": said }, "error": said }))
        };
        match sent.and_then(|()| stream.end()) {
            Ok(()) => Done::Keep,
            Err(_) => Done::Close,
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
        let summaries: Vec<Value> = images
            .iter()
            .map(|i| {
                let labels = self
                    .image_doc(&i.id)
                    .ok()
                    .and_then(|doc| {
                        serde_json::from_str::<Value>(&crate::daemon::inspect_doc::json(&doc)).ok()
                    })
                    .and_then(|doc| doc.pointer("/Config/Labels").cloned())
                    .unwrap_or_else(|| json!({}));
                json!({
                    "Id": i.id,
                    "ParentId": "",
                    "RepoTags": i.tags,
                    "RepoDigests": i.digests,
                    "Created": i.created,
                    "Size": i.size,
                    "SharedSize": -1,
                    "Labels": if labels.is_null() { json!({}) } else { labels },
                    "Containers": i.containers,
                })
            })
            .collect();
        Answer::json(200, &Value::Array(summaries))
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
