//! The gcplogs driver's sending (D169): its entries (logging/gcplogs.rs) written to Cloud
//! Logging (entries:write), with an access token the environment of the run's client finds
//! (crate::google: its Application Default Credentials, or GCE's metadata server), for
//! the project the options, the instance or the credentials say.
//!
//! A write is delivered once Cloud Logging answers 200, and one that fails is written
//! again, never dropped: moby's client drops what it cannot buffer (1 GiB) and what a
//! write fails with. It asks partial success, so that an entry Cloud Logging refuses holds
//! up none of the others; those it refuses are said in the daemon's log. A batch goes once
//! 1,000 entries or 8 MiB, the client's bundle, and whenever the shipper has caught up
//! with its log, where moby's waits a second.

use std::io;
use std::time::Duration;

use serde_json::Value;
use shards_registry::http::{Client, Patience};
use shards_registry::url::Url;

use super::{Driver, Message, Wait};
use crate::google::{self, Env, Token};
use crate::logging::gcplogs::{Config, From, Instance, SCOPE};

/// A batch's most entries and bytes (DefaultEntryCountThreshold, DefaultEntryByteThreshold).
const ENTRIES: usize = 1000;
const BYTES: usize = 8 << 20;
/// How long before it expires a token is asked for again.
const RENEW: Duration = Duration::from_secs(5 * 60);

pub(super) struct Gcplogs {
    config: Config,
    /// The environment of the run's client, `NAME=VALUE`.
    env: Vec<String>,
    details: crate::logging::Details,
    attributes: std::collections::BTreeMap<String, String>,
    /// The run's log and resource, found as first needed.
    from: Option<From>,
    token: Option<Token>,
    client: Client,
    url: Url,
    batch: Vec<String>,
    bytes: usize,
}

impl Gcplogs {
    pub(super) fn new(
        config: Config,
        env: Vec<String>,
        details: crate::logging::Details,
        attributes: std::collections::BTreeMap<String, String>,
    ) -> Result<Gcplogs, String> {
        let url = Url::parse(&format!("{}/v2/entries:write", config.endpoint)).map_err(|e| e.to_string())?;
        let tls = shards_registry::tls::client_config(Vec::new(), None).map_err(|e| e.to_string())?;
        let lookup = |k: &str| shards_ipc::env_value(&env, k);
        let client = Client::new(
            Box::new(move |_| Ok(tls.clone())),
            &format!("shards/{}", env!("CARGO_PKG_VERSION")),
        )
        .with_proxies(shards_registry::proxy::Proxies::from_env(&lookup))
        .with_patience(Patience::default());
        Ok(Gcplogs {
            config,
            env,
            details,
            attributes,
            from: None,
            token: None,
            client,
            url,
            batch: Vec::new(),
            bytes: 0,
        })
    }

    /// The run's log, resource and instance (moby gcplogs New): on GCE, its metadata
    /// server's project and instance; else the instance the options name, where they name
    /// one. The project: `gcp-project`, else the instance's, else the credentials' JSON's,
    /// else GOOGLE_CLOUD_PROJECT's, where moby's asks the option and the instance alone.
    fn from(&mut self) -> io::Result<&From> {
        if self.from.is_none() {
            let env = |k: &str| shards_ipc::env_value(&self.env, k);
            let env: Env<'_> = &env;
            let on_gce = google::on_gce(env);
            let meta = |suffix: &str| google::metadata_get(env, suffix).map(|v| v.trim().to_string());
            let instance = if on_gce {
                Some(Instance {
                    // `projects/N/zones/Z`: its last part, as ZoneWithContext gives it.
                    zone: meta("instance/zone")
                        .map(|z| z.rsplit('/').next().unwrap_or_default().to_string())
                        .unwrap_or_default(),
                    name: meta("instance/name").unwrap_or_default(),
                    id: meta("instance/id").unwrap_or_default(),
                })
            } else if self.config.zone.is_some()
                || self.config.instance_name.is_some()
                || self.config.instance_id.is_some()
            {
                Some(Instance {
                    zone: self.config.zone.clone().unwrap_or_default(),
                    name: self.config.instance_name.clone().unwrap_or_default(),
                    id: self.config.instance_id.clone().unwrap_or_default(),
                })
            } else {
                None
            };
            let project = self
                .config
                .project
                .clone()
                .or_else(|| on_gce.then(|| meta("project/project-id").ok()).flatten().filter(|p| !p.is_empty()))
                .or_else(|| google::credentials_project(env))
                .or_else(|| env("GOOGLE_CLOUD_PROJECT").filter(|p| !p.is_empty()))
                .ok_or_else(|| {
                    io::Error::other(
                        "no project to log to: give --log-opt gcp-project, or run where GCE's metadata server, the credentials or GOOGLE_CLOUD_PROJECT name one",
                    )
                })?;
            self.from = Some(From::new(
                &self.config,
                &project,
                instance.as_ref(),
                &self.details,
                &self.attributes,
            ));
        }
        self.from
            .as_ref()
            .ok_or_else(|| io::Error::other("no log to write to"))
    }

    /// A token for Cloud Logging, asked for again before it expires.
    fn token(&mut self) -> io::Result<String> {
        if self.token.as_ref().is_none_or(|t| t.expiring(RENEW)) {
            let env = |k: &str| shards_ipc::env_value(&self.env, k);
            self.token = Some(google::access(&env, SCOPE).map_err(io::Error::other)?);
        }
        self.token
            .as_ref()
            .map(|t| t.value.to_string())
            .ok_or_else(|| io::Error::other("no token"))
    }

    /// Writes the batch: delivered once Cloud Logging takes it; the entries it refuses
    /// said in the daemon's log.
    fn write(&mut self) -> io::Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        self.from()?;
        let body = self
            .from
            .as_ref()
            .ok_or_else(|| io::Error::other("no log to write to"))?
            .write(&self.batch);
        let token = self.token()?;
        let auth = format!("Bearer {token}");
        let env = |k: &str| shards_ipc::env_value(&self.env, k);
        let quota = google::quota_project(&env);
        let mut headers = vec![
            ("Authorization", auth.as_str()),
            ("Content-Type", "application/json"),
        ];
        if let Some(q) = &quota {
            headers.push(("x-goog-user-project", q.as_str()));
        }
        let (status, answer) = send(&self.client, &self.url, &headers, &body)?;
        match status {
            200 => {}
            // Partial success: what it took is written; what it refused, said.
            400 if refused(&answer).is_some() => {
                for (index, why) in refused(&answer).unwrap_or_default() {
                    super::log(format!(
                        "{}: Cloud Logging refused entry {index} of a write: {why}",
                        self.from
                            .as_ref()
                            .map(|f| f.log_name.as_str())
                            .unwrap_or_default()
                    ));
                }
            }
            _ => {
                // A token refused or expired: asked for again before the next write.
                if status == 401 {
                    self.token = None;
                }
                return Err(io::Error::other(format!(
                    "Cloud Logging: {status} {}",
                    String::from_utf8_lossy(&answer).trim()
                )));
            }
        }
        self.batch.clear();
        self.bytes = 0;
        Ok(())
    }
}

/// Posts `body` to `url`: the answer's status and its body, at most 64 KiB of it.
fn send(client: &Client, url: &Url, headers: &[(&str, &str)], body: &[u8]) -> io::Result<(u16, Vec<u8>)> {
    use std::io::Read as _;
    let mut answer = client
        .send(&shards_registry::http::Request {
            method: "POST",
            url,
            headers,
            body,
            file: None,
        })
        .map_err(|e| io::Error::other(e.to_string()))?;
    let status = answer.status;
    let mut out = Vec::new();
    (&mut answer).take(64 << 10).read_to_end(&mut out)?;
    // The rest read and let go, so that the connection serves the next write.
    io::copy(&mut answer, &mut io::sink())?;
    Ok((status, out))
}

/// The entries a partial success refused: each its index in the write and why, from the
/// answer's WriteLogEntriesPartialErrors; none where the answer is no partial success.
fn refused(answer: &[u8]) -> Option<Vec<(String, String)>> {
    let v: Value = serde_json::from_slice(answer).ok()?;
    let details = v.get("error")?.get("details")?.as_array()?;
    let partial = details.iter().find(|d| {
        d.get("@type").and_then(Value::as_str)
            == Some("type.googleapis.com/google.logging.v2.WriteLogEntriesPartialErrors")
    })?;
    let errors = partial.get("logEntryErrors")?.as_object()?;
    Some(
        errors
            .iter()
            .map(|(i, e)| {
                (
                    i.clone(),
                    e.get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                )
            })
            .collect(),
    )
}

impl Driver for Gcplogs {
    fn send(&mut self, m: &Message<'_>, _wait: &Wait<'_>) -> io::Result<()> {
        // A full batch goes first: a message is taken only where there is room for it.
        if self.batch.len() >= ENTRIES || self.bytes >= BYTES {
            self.write()?;
        }
        let entry = self.from()?.entry(m.stream, m.at, m.line);
        self.bytes += entry.len();
        self.batch.push(entry);
        Ok(())
    }

    fn flush(&mut self, _wait: &Wait<'_>) -> io::Result<()> {
        self.write()
    }

    fn undelivered(&mut self) -> usize {
        self.batch.len()
    }

    fn close(&mut self, _wait: &Wait<'_>) -> io::Result<()> {
        self.write()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A partial success's refusals are found, each with its index and why; anything else
    /// is no partial success.
    #[test]
    fn partial_successes_say_what_they_refused() {
        let answer = br#"{"error":{"code":400,"message":"Log entry with size 300K exceeds maximum size","status":"INVALID_ARGUMENT","details":[{"@type":"type.googleapis.com/google.logging.v2.WriteLogEntriesPartialErrors","logEntryErrors":{"3":{"code":3,"message":"Log entry with size 300K exceeds maximum size"}}}]}}"#;
        assert_eq!(
            refused(answer).unwrap(),
            [(
                "3".to_string(),
                "Log entry with size 300K exceeds maximum size".to_string()
            )]
        );
        assert!(refused(br#"{"error":{"code":400,"message":"bad","status":"INVALID_ARGUMENT"}}"#).is_none());
        assert!(refused(b"not json").is_none());
    }
}
