//! The awslogs driver's sending (D168): its events (logging/awslogs.rs) put to CloudWatch
//! Logs as moby's awslogs driver puts them, over its JSON 1.1 API (`X-Amz-Target:
//! Logs_20140328.*`), signed for `logs` (SigV4) with the credentials the environment of the
//! run's client finds (crate::aws::credentials), or `awslogs-credentials-endpoint` gives;
//! in the region the options, that environment or EC2's instance metadata say. Its stream
//! is made first, and its group where it is not there and the options say to make it.
//!
//! A put is delivered once CloudWatch answers 200, and one that fails is put again, never
//! dropped, where moby's logs the failure and drops the put. A put goes once full, and
//! whenever the shipper has caught up with its log (Driver::flush), where moby's waits for
//! its 5 s tick; lines joined into an event wait for more as long as
//! `awslogs-force-flush-interval-seconds` says, the shipper woken for them (Driver::due).

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, SystemTime};

use serde_json::Value;
use shards_registry::http::{Client, Patience};
use shards_registry::url::Url;

use super::{Driver, Message, Wait};
use crate::aws::credentials::{Chain, ECS, Found};
use crate::aws::sigv4::{Rule, Scope};
use crate::logging::awslogs::{self, Batch, Config, EVENT, Event, Events, Rendered};

/// How long before they expire credentials are found again: a put signed with them takes
/// a while to send, as the SDK gives a container's (resolveHTTPCredProvider's ExpiryWindow).
const RENEW: Duration = Duration::from_secs(5 * 60);

pub(super) struct Awslogs {
    config: Config,
    /// The environment of the run's client, `NAME=VALUE`: what its credentials, its region
    /// and CloudWatch's address are found by.
    env: Vec<String>,
    stream: String,
    entity: Option<Rendered>,
    /// Where puts go and what signs them, found as they are first needed, and again as
    /// the credentials expire or are refused.
    to: Option<To>,
    /// The stream is there: made, or found made.
    made: bool,
    events: Events,
    /// Events made, not yet in the batch.
    pending: VecDeque<Event>,
    batch: Batch,
    /// The token the last put was answered with, which the next is sent with, as moby
    /// sends it (CloudWatch no longer asks for one).
    token: Option<String>,
}

/// CloudWatch Logs, as a run's environment finds it.
struct To {
    client: Client,
    url: Url,
    region: String,
    found: Found,
}

impl Awslogs {
    pub(super) fn new(
        config: Config,
        env: Vec<String>,
        stream: String,
        entity: Option<Rendered>,
    ) -> Result<Awslogs, String> {
        let pattern = config
            .multiline
            .as_deref()
            .map(regex::bytes::Regex::new)
            .transpose()
            .map_err(|e| e.to_string())?;
        Ok(Awslogs {
            config,
            env,
            stream,
            entity,
            to: None,
            made: false,
            events: Events::new(pattern),
            pending: VecDeque::new(),
            batch: Batch::default(),
            token: None,
        })
    }

    /// Where puts go, and their credentials, found again where these expire within
    /// [`RENEW`].
    fn to(&mut self) -> io::Result<&mut To> {
        if self.to.as_ref().is_some_and(|to| !to.found.expiring(RENEW)) {
            return self
                .to
                .as_mut()
                .ok_or_else(|| io::Error::other("no CloudWatch Logs"));
        }
        let to = self.find().map_err(io::Error::other)?;
        Ok(self.to.insert(to))
    }

    /// CloudWatch Logs as the options and the run's client's environment find it: its
    /// region the options', `AWS_REGION`'s, `AWS_DEFAULT_REGION`'s, the profile's, else
    /// EC2's instance metadata's; its address `awslogs-endpoint`, the configuration's for
    /// it, else its own in that region; its credentials `awslogs-credentials-endpoint`'s,
    /// else the chain's.
    fn find(&self) -> Result<To, String> {
        let env = |k: &str| shards_ipc::env_value(&self.env, k);
        let chain = Chain::new(&env).listing(&self.env);
        let top = chain.profile()?;
        let region = match self.config.region.clone().or_else(|| chain.configured_region(top.as_ref())) {
            Some(r) => r,
            None => chain.instance_region(top.as_ref()).map_err(|e| {
                format!(
                    "no region to send the log to: give --log-opt awslogs-region, set AWS_REGION, or the profile's region ({e})"
                )
            })?,
        };
        let chain = chain.in_region(&region);
        let url = match &self.config.endpoint {
            Some(e) => e.clone(),
            None => chain.service(top.as_ref(), "CloudWatch Logs", "logs", &region)?,
        };
        let url = Url::parse(&url).map_err(|e| format!("{url}: {e}"))?;
        let found = match &self.config.credentials_endpoint {
            Some(path) => chain.endpoint(&format!("{ECS}{path}"), None)?,
            None => chain.resolve()?,
        };
        let client = crate::aws::client(&env, top.as_ref(), Patience::default())?;
        Ok(To {
            client,
            url,
            region,
            found,
        })
    }

    /// Asks CloudWatch Logs for `op` with `body`: its answer, or what it said is wrong:
    /// its type (`__type`'s, its namespace left out) and its message.
    fn call(&mut self, op: &str, body: &[u8]) -> io::Result<Result<Value, Refused>> {
        let target = format!("Logs_20140328.{op}");
        let mut fields = vec![
            ("Content-Type", "application/x-amz-json-1.1"),
            ("X-Amz-Target", target.as_str()),
        ];
        if self.config.emf {
            fields.push(("x-amzn-logs-format", awslogs::EMF));
        }
        let to = self.to()?;
        let (status, answer) = crate::aws::post_signed(
            &to.client,
            &to.url,
            &fields,
            body,
            &to.found.creds,
            &Scope {
                region: &to.region,
                service: "logs",
                rule: Rule::Ignored,
            },
        )
        .map_err(io::Error::other)?;
        let v: Value = serde_json::from_slice(&answer).unwrap_or(Value::Null);
        if status == 200 {
            return Ok(Ok(v));
        }
        let kind = crate::aws::text(&v, "__type")
            .or_else(|| crate::aws::text(&v, "code"))
            .unwrap_or_default();
        let kind = kind.rsplit('#').next().unwrap_or_default();
        let kind = kind.split(':').next().unwrap_or_default().to_string();
        let message = crate::aws::text(&v, "message")
            .unwrap_or_else(|| String::from_utf8_lossy(&answer).trim().to_string());
        // Credentials refused or expired: found again for the next request.
        if status == 403
            || matches!(
                kind.as_str(),
                "ExpiredTokenException"
                    | "ExpiredToken"
                    | "UnrecognizedClientException"
                    | "InvalidSignatureException"
            )
        {
            self.to = None;
        }
        Ok(Err(Refused {
            status,
            kind,
            message,
        }))
    }

    /// The stream, made unless the options say it is there (createLogStream): its group
    /// made first where it is not there and the options say to (create).
    fn make(&mut self) -> io::Result<()> {
        if self.made || !self.config.create_stream {
            self.made = true;
            return Ok(());
        }
        let body = awslogs::create_stream(&self.config.group, &self.stream);
        match self.call("CreateLogStream", &body)? {
            Ok(_) => {}
            Err(r) if r.kind == "ResourceAlreadyExistsException" => {}
            Err(r) if r.kind == "ResourceNotFoundException" && self.config.create_group => {
                match self.call("CreateLogGroup", &awslogs::create_group(&self.config.group))? {
                    Ok(_) => {}
                    Err(r) if r.kind == "ResourceAlreadyExistsException" => {}
                    Err(r) => return Err(r.of("CreateLogGroup", &self.config.group)),
                }
                match self.call("CreateLogStream", &body)? {
                    Ok(_) => {}
                    Err(r) if r.kind == "ResourceAlreadyExistsException" => {}
                    Err(r) => return Err(r.of("CreateLogStream", &self.stream)),
                }
            }
            Err(r) => return Err(r.of("CreateLogStream", &self.stream)),
        }
        self.made = true;
        Ok(())
    }

    /// Puts the batch: delivered once CloudWatch takes it, or says it has (publishBatch).
    fn put(&mut self) -> io::Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        if !self.batch.sends() {
            self.batch.clear();
            return Ok(());
        }
        self.make()?;
        let mut answer = self.put_once()?;
        // A token CloudWatch no longer asks for, as an older one may: the one it expects.
        if let Err(r) = &answer
            && r.kind == "InvalidSequenceTokenException"
        {
            self.token = None;
            answer = self.put_once()?;
        }
        match answer {
            Ok(v) => {
                self.token = crate::aws::text(&v, "nextSequenceToken");
                self.rejected(&v);
            }
            Err(r) if r.kind == "DataAlreadyAcceptedException" => {}
            Err(r) => {
                // A stream or group taken away since: made again before the next put.
                if r.kind == "ResourceNotFoundException" {
                    self.made = false;
                }
                return Err(r.of("PutLogEvents", &self.stream));
            }
        }
        self.batch.clear();
        Ok(())
    }

    fn put_once(&mut self) -> io::Result<Result<Value, Refused>> {
        let body = awslogs::put(
            &self.config.group,
            &self.stream,
            &self.batch.sorted(),
            self.token.as_deref(),
            self.entity.as_ref(),
        );
        self.call("PutLogEvents", &body)
    }

    /// What CloudWatch took and did not store of a put (rejectedLogEventsInfo): events too
    /// old for it or the group's retention, or too far ahead; and an entity it refused,
    /// left out of the next puts, as moby leaves it out.
    fn rejected(&mut self, answer: &Value) {
        let info = answer.get("rejectedLogEventsInfo").unwrap_or(&Value::Null);
        for (k, what) in [
            ("tooOldLogEventEndIndex", "older than CloudWatch takes"),
            ("expiredLogEventEndIndex", "older than the group keeps"),
            ("tooNewLogEventStartIndex", "too far ahead of CloudWatch's clock"),
        ] {
            if let Some(i) = info.get(k).and_then(Value::as_u64) {
                super::log(format!(
                    "{}/{}: CloudWatch Logs did not keep the put's events {} index {i}: they are {what}",
                    self.config.group,
                    self.stream,
                    if k.starts_with("tooNew") { "from" } else { "to" },
                ));
            }
        }
        if answer.get("rejectedEntityInfo").is_some_and(|e| !e.is_null()) && self.entity.take().is_some() {
            super::log(format!(
                "{}/{}: CloudWatch Logs refused the entity; the next puts go without it",
                self.config.group, self.stream
            ));
        }
    }

    /// Moves the events made into the batch, putting it each time it is full.
    fn drain(&mut self) -> io::Result<()> {
        while let Some(e) = self.pending.front() {
            if self.batch.fits(e) {
                if let Some(e) = self.pending.pop_front() {
                    self.batch.add(e);
                }
            } else {
                self.put()?;
            }
        }
        Ok(())
    }

    /// Moves the events made into the batch, as long as they fit: nothing put.
    fn fill(&mut self) {
        while let Some(e) = self.pending.front() {
            if !self.batch.fits(e) {
                break;
            }
            if let Some(e) = self.pending.pop_front() {
                self.batch.add(e);
            }
        }
    }

    /// How long the lines held, joined, have waited, as moby measures it on its tick
    /// (collectBatch): from the first's time, in milliseconds, to now's; where now is
    /// before it, past waiting.
    fn waited(&self) -> Option<Duration> {
        let (at, _) = self.events.held()?;
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        Some(match now.checked_sub(at) {
            Some(ms) if ms >= 0 => Duration::from_millis(ms.unsigned_abs()),
            _ => Duration::MAX,
        })
    }
}

/// What CloudWatch Logs said of a request it refused.
struct Refused {
    status: u16,
    kind: String,
    message: String,
}

impl Refused {
    fn of(&self, op: &str, name: &str) -> io::Error {
        io::Error::other(format!(
            "CloudWatch Logs: {op} {name}: {} {}: {}",
            self.status, self.kind, self.message
        ))
    }
}

impl Driver for Awslogs {
    /// Lines are cut where an event would be too long, as moby's copier is given awslogs'
    /// BufSize.
    fn part(&self) -> usize {
        EVENT
    }

    fn send(&mut self, m: &Message<'_>, _wait: &Wait<'_>) -> io::Result<()> {
        // What was made before goes first: a message is taken only once nothing waits.
        self.drain()?;
        let at = i64::try_from(m.at / 1_000_000).unwrap_or(i64::MAX);
        let mut made = Vec::new();
        self.events.take(m.line, at, &mut made);
        self.pending.extend(made);
        self.fill();
        Ok(())
    }

    fn flush(&mut self, _wait: &Wait<'_>) -> io::Result<()> {
        // Joined lines that have waited as long as the options say go as they are.
        if self.waited().is_some_and(|w| w >= self.config.flush) {
            let mut made = Vec::new();
            self.events.finish(&mut made);
            self.pending.extend(made);
        }
        self.drain()?;
        self.put()?;
        // A run that writes nothing has its stream all the same, as moby's makes it as
        // the run starts.
        self.make()
    }

    fn due(&self) -> Option<Duration> {
        self.waited().map(|w| self.config.flush.saturating_sub(w))
    }

    fn undelivered(&mut self) -> usize {
        let held = self.events.held().map_or(0, |(_, lines)| lines);
        held + self.pending.iter().map(|e| e.lines).sum::<usize>() + self.batch.lines()
    }

    fn close(&mut self, _wait: &Wait<'_>) -> io::Result<()> {
        let mut made = Vec::new();
        self.events.finish(&mut made);
        self.pending.extend(made);
        self.drain()?;
        self.put()?;
        self.make()
    }
}
