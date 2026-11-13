//! `Secrets`: secrets by path, after AWS Secrets Manager and Vault. Each secret has versions, and
//! the stages `AWSCURRENT`, `AWSPREVIOUS` and `AWSPENDING` move between them (rotation); a value
//! never shows in a log, a describe or a listing; a delete is scheduled, with a window to restore
//! it in; a lease reads a secret until it expires by the clock. Veil is the real one (CP-T8.1);
//! [`SimSecrets`] keeps them in memory.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use parking_lot::Mutex;

use crate::clock::{Clock, SimClock};
use crate::conformance::{Case, Failure, ensure, fine, same};

/// A secret's value. It never shows: `Debug` and `Display` say only how long it is; `expose` is
/// the one way to the bytes. Dropped, its buffer is zeroed by writes the compiler keeps
/// (`zeroize`); a copy made of it (a clone, the bytes it was made from) is the copy's to clear.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(Vec<u8>);

impl SecretValue {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretValue(<{} bytes, redacted>)", self.0.len())
    }
}

impl fmt::Display for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<redacted>")
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

/// A version's stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// `AWSCURRENT`: what a read gets.
    Current,
    /// `AWSPREVIOUS`: the one before it.
    Previous,
    /// `AWSPENDING`: staged by a rotation, not yet current.
    Pending,
}

/// A version, without its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub id: String,
    pub stages: Vec<Stage>,
    pub created_ms: i64,
}

/// A secret, without its values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Described {
    pub name: String,
    pub versions: Vec<Version>,
    /// When it goes for good, when its delete is scheduled.
    pub purge_at_ms: Option<i64>,
}

/// Which version a read gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Select {
    Stage(Stage),
    Version(String),
}

/// A lease on a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub id: String,
    pub name: String,
    pub expires_ms: i64,
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("no secret {0:?}")]
    NotFound(String),
    #[error("a secret {0:?} exists")]
    Exists(String),
    #[error("{what} isn't valid: {why}")]
    Invalid { what: &'static str, why: String },
    #[error("{what} is {size}, over its limit of {limit}")]
    TooLarge { what: &'static str, size: usize, limit: usize },
    #[error("secret {name:?} has no such version or stage")]
    NoSuchVersion { name: String },
    #[error("secret {name:?} is scheduled for deletion (for good at {purge_at_ms})")]
    Deleted { name: String, purge_at_ms: i64 },
    #[error("the lease has expired or was revoked")]
    Expired,
    #[error("the store holds its most ({what}: {limit})")]
    Full { what: &'static str, limit: usize },
    #[error("the store is unavailable: {0}")]
    Unavailable(String),
}

/// Secrets, versions, stages and leases.
pub trait Secrets: Send + Sync + 'static {
    /// A new secret, its first version current.
    fn create<'a>(&'a self, name: &'a str, value: SecretValue) -> BoxFuture<'a, Result<Version, Refusal>>;
    /// A new version: `Current` makes it current (the current one becomes previous); `Pending`
    /// stages it (the pending one before it loses the stage).
    fn put<'a>(&'a self, name: &'a str, value: SecretValue, stage: Stage) -> BoxFuture<'a, Result<Version, Refusal>>;
    fn get<'a>(&'a self, name: &'a str, select: Select) -> BoxFuture<'a, Result<(Version, SecretValue), Refusal>>;
    /// Makes version `id` current: the current one becomes previous, and the pending stage, on
    /// it, goes (a rotation's last step).
    fn promote<'a>(&'a self, name: &'a str, id: &'a str) -> BoxFuture<'a, Result<(), Refusal>>;
    fn describe<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Described, Refusal>>;
    /// Names starting with `prefix`, after `after`, at most `limit`, in name order.
    fn list<'a>(&'a self, prefix: &'a str, after: Option<&'a str>, limit: usize) -> BoxFuture<'a, Result<Vec<String>, Refusal>>;
    /// Schedules the secret's deletion `window` from now (7 to 30 days, as Secrets Manager);
    /// reads are refused `Deleted` meanwhile. When it goes for good.
    fn delete<'a>(&'a self, name: &'a str, window: Duration) -> BoxFuture<'a, Result<i64, Refusal>>;
    /// Cancels a scheduled deletion.
    fn restore<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), Refusal>>;
    /// A lease on the secret's current value, for `ttl` (at most a day).
    fn lease<'a>(&'a self, name: &'a str, ttl: Duration) -> BoxFuture<'a, Result<Lease, Refusal>>;
    /// The current value, through a lease that hasn't expired.
    fn read_leased<'a>(&'a self, lease: &'a str) -> BoxFuture<'a, Result<SecretValue, Refusal>>;
    /// The lease, for `ttl` more from now.
    fn renew<'a>(&'a self, lease: &'a str, ttl: Duration) -> BoxFuture<'a, Result<Lease, Refusal>>;
    fn revoke<'a>(&'a self, lease: &'a str) -> BoxFuture<'a, Result<(), Refusal>>;
}

/// Largest value (Secrets Manager's).
pub const VALUE_MAX: usize = 64 << 10;
/// Longest name (Secrets Manager's).
pub const NAME_MAX: usize = 512;
/// Versions kept per secret (Secrets Manager keeps about 100).
pub const VERSIONS_MAX: usize = 100;
/// Most names a listing gives.
pub const LIST_MAX: usize = 100;
/// A delete's window: 7 to 30 days (Secrets Manager's).
pub const WINDOW_MIN: Duration = Duration::from_secs(7 * 24 * 3600);
pub const WINDOW_MAX: Duration = Duration::from_secs(30 * 24 * 3600);
/// The longest a lease lasts.
pub const LEASE_MAX: Duration = Duration::from_secs(24 * 3600);

fn check_name(name: &str) -> Result<(), Refusal> {
    let ok = !name.is_empty() && name.len() <= NAME_MAX && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'+' | b'=' | b'.' | b'@' | b'-'));
    if ok { Ok(()) } else { Err(Refusal::Invalid { what: "a secret's name", why: format!("1 to {NAME_MAX} of A-Z a-z 0-9 / _ + = . @ - ({} bytes)", name.len()) }) }
}

fn check_value(v: &SecretValue) -> Result<(), Refusal> {
    if v.len() > VALUE_MAX { Err(Refusal::TooLarge { what: "a value", size: v.len(), limit: VALUE_MAX }) } else { Ok(()) }
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

// ---------------------------------------------------------------------------------- simulator

/// Most secrets, and leases, a [`SimSecrets`] holds.
pub const SIM_SECRETS: usize = 100_000;

/// Secrets in memory, timed by a clock. Clones share them.
#[derive(Clone)]
pub struct SimSecrets {
    inner: Arc<Mutex<SimState>>,
    clock: Arc<dyn Clock>,
}

#[derive(Default)]
struct SimState {
    secrets: BTreeMap<String, SimSecret>,
    leases: BTreeMap<String, Lease>,
    next: u64,
}

struct SimSecret {
    versions: Vec<(Version, SecretValue)>,
    purge_at_ms: Option<i64>,
}

impl SimSecret {
    fn with(&self, stage: Stage) -> Option<&(Version, SecretValue)> {
        self.versions.iter().find(|(v, _)| v.stages.contains(&stage))
    }
    fn drop_stage(&mut self, stage: Stage) {
        for (v, _) in &mut self.versions {
            v.stages.retain(|s| *s != stage);
        }
    }
}

impl Default for SimSecrets {
    fn default() -> Self {
        Self::new(Arc::new(SimClock::new()))
    }
}

impl SimSecrets {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { inner: Arc::default(), clock }
    }

    fn next_id(st: &mut SimState, what: &str) -> String {
        st.next = st.next.saturating_add(1);
        format!("{what}-{:08}", st.next)
    }

    /// A live secret: not there is `NotFound`; scheduled for deletion is `Deleted`; one past its
    /// window is gone for good.
    fn live<'s>(st: &'s mut SimState, name: &str, now: i64) -> Result<&'s mut SimSecret, Refusal> {
        if st.secrets.get(name).and_then(|s| s.purge_at_ms).is_some_and(|p| now >= p) {
            st.secrets.remove(name);
        }
        let s = st.secrets.get_mut(name).ok_or_else(|| Refusal::NotFound(name.to_owned()))?;
        if let Some(purge_at_ms) = s.purge_at_ms {
            return Err(Refusal::Deleted { name: name.to_owned(), purge_at_ms });
        }
        Ok(s)
    }

    fn lease_of(st: &SimState, id: &str, now: i64) -> Result<Lease, Refusal> {
        st.leases.get(id).filter(|l| now < l.expires_ms).cloned().ok_or(Refusal::Expired)
    }
}

impl Secrets for SimSecrets {
    fn create<'a>(&'a self, name: &'a str, value: SecretValue) -> BoxFuture<'a, Result<Version, Refusal>> {
        let now = self.clock.unix_ms();
        let out = (|| {
            check_name(name)?;
            check_value(&value)?;
            let mut st = self.inner.lock();
            if st.secrets.get(name).is_some_and(|s| s.purge_at_ms.is_none_or(|p| now < p)) {
                return Err(Refusal::Exists(name.to_owned()));
            }
            if st.secrets.len() >= SIM_SECRETS {
                return Err(Refusal::Full { what: "secrets", limit: SIM_SECRETS });
            }
            let v = Version { id: Self::next_id(&mut st, "v"), stages: vec![Stage::Current], created_ms: now };
            st.secrets.insert(name.to_owned(), SimSecret { versions: vec![(v.clone(), value)], purge_at_ms: None });
            Ok(v)
        })();
        Box::pin(async move { out })
    }

    fn put<'a>(&'a self, name: &'a str, value: SecretValue, stage: Stage) -> BoxFuture<'a, Result<Version, Refusal>> {
        let now = self.clock.unix_ms();
        let out = (|| {
            check_value(&value)?;
            if stage == Stage::Previous {
                return Err(Refusal::Invalid { what: "a new version's stage", why: "Current or Pending".into() });
            }
            let mut st = self.inner.lock();
            let id = Self::next_id(&mut st, "v");
            let s = Self::live(&mut st, name, now)?;
            if stage == Stage::Current {
                s.drop_stage(Stage::Previous);
                for (v, _) in &mut s.versions {
                    if let Some(c) = v.stages.iter_mut().find(|x| **x == Stage::Current) {
                        *c = Stage::Previous;
                    }
                }
            } else {
                s.drop_stage(Stage::Pending);
            }
            let v = Version { id, stages: vec![stage], created_ms: now };
            s.versions.push((v.clone(), value));
            // The oldest versions without a stage go first, past what's kept.
            while s.versions.len() > VERSIONS_MAX {
                match s.versions.iter().position(|(v, _)| v.stages.is_empty()) {
                    Some(i) => {
                        s.versions.remove(i);
                    }
                    None => break,
                }
            }
            Ok(v)
        })();
        Box::pin(async move { out })
    }

    fn get<'a>(&'a self, name: &'a str, select: Select) -> BoxFuture<'a, Result<(Version, SecretValue), Refusal>> {
        let now = self.clock.unix_ms();
        let out = (|| {
            let mut st = self.inner.lock();
            let s = Self::live(&mut st, name, now)?;
            let found = match &select {
                Select::Stage(stage) => s.with(*stage),
                Select::Version(id) => s.versions.iter().find(|(v, _)| v.id == *id),
            };
            found.cloned().ok_or_else(|| Refusal::NoSuchVersion { name: name.to_owned() })
        })();
        Box::pin(async move { out })
    }

    fn promote<'a>(&'a self, name: &'a str, id: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        let now = self.clock.unix_ms();
        let out = (|| {
            let mut st = self.inner.lock();
            let s = Self::live(&mut st, name, now)?;
            if !s.versions.iter().any(|(v, _)| v.id == id) {
                return Err(Refusal::NoSuchVersion { name: name.to_owned() });
            }
            if s.with(Stage::Current).is_some_and(|(v, _)| v.id == id) {
                return Ok(());
            }
            s.drop_stage(Stage::Previous);
            for (v, _) in &mut s.versions {
                if v.id == id {
                    v.stages.retain(|x| *x != Stage::Pending);
                    v.stages.push(Stage::Current);
                } else if let Some(c) = v.stages.iter_mut().find(|x| **x == Stage::Current) {
                    *c = Stage::Previous;
                }
            }
            Ok(())
        })();
        Box::pin(async move { out })
    }

    fn describe<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Described, Refusal>> {
        let now = self.clock.unix_ms();
        let out = (|| {
            let mut st = self.inner.lock();
            if st.secrets.get(name).and_then(|s| s.purge_at_ms).is_some_and(|p| now >= p) {
                st.secrets.remove(name);
            }
            let s = st.secrets.get(name).ok_or_else(|| Refusal::NotFound(name.to_owned()))?;
            Ok(Described { name: name.to_owned(), versions: s.versions.iter().map(|(v, _)| v.clone()).collect(), purge_at_ms: s.purge_at_ms })
        })();
        Box::pin(async move { out })
    }

    fn list<'a>(&'a self, prefix: &'a str, after: Option<&'a str>, limit: usize) -> BoxFuture<'a, Result<Vec<String>, Refusal>> {
        let now = self.clock.unix_ms();
        let out = (|| {
            if limit == 0 {
                return Err(Refusal::Invalid { what: "a listing's limit", why: "it's 0".into() });
            }
            let st = self.inner.lock();
            let start = match after {
                Some(a) if a >= prefix => std::ops::Bound::Excluded(a.to_owned()),
                _ => std::ops::Bound::Included(prefix.to_owned()),
            };
            Ok(st
                .secrets
                .range((start, std::ops::Bound::Unbounded))
                .take_while(|(k, _)| k.starts_with(prefix))
                .filter(|(_, s)| s.purge_at_ms.is_none_or(|p| now < p))
                .take(limit.min(LIST_MAX))
                .map(|(k, _)| k.clone())
                .collect())
        })();
        Box::pin(async move { out })
    }

    fn delete<'a>(&'a self, name: &'a str, window: Duration) -> BoxFuture<'a, Result<i64, Refusal>> {
        let now = self.clock.unix_ms();
        let out = (|| {
            if window < WINDOW_MIN || window > WINDOW_MAX {
                return Err(Refusal::Invalid { what: "a delete's window", why: format!("{window:?} (7 to 30 days)") });
            }
            let mut st = self.inner.lock();
            let s = Self::live(&mut st, name, now)?;
            let purge_at_ms = now.saturating_add(ms(window));
            s.purge_at_ms = Some(purge_at_ms);
            Ok(purge_at_ms)
        })();
        Box::pin(async move { out })
    }

    fn restore<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        let now = self.clock.unix_ms();
        let out = (|| {
            let mut st = self.inner.lock();
            if st.secrets.get(name).and_then(|s| s.purge_at_ms).is_some_and(|p| now >= p) {
                st.secrets.remove(name);
            }
            let s = st.secrets.get_mut(name).ok_or_else(|| Refusal::NotFound(name.to_owned()))?;
            s.purge_at_ms = None;
            Ok(())
        })();
        Box::pin(async move { out })
    }

    fn lease<'a>(&'a self, name: &'a str, ttl: Duration) -> BoxFuture<'a, Result<Lease, Refusal>> {
        let now = self.clock.unix_ms();
        let out = (|| {
            if ttl.is_zero() || ttl > LEASE_MAX {
                return Err(Refusal::Invalid { what: "a lease's time", why: format!("{ttl:?} (more than 0, at most a day)") });
            }
            let mut st = self.inner.lock();
            st.leases.retain(|_, l| now < l.expires_ms);
            if st.leases.len() >= SIM_SECRETS {
                return Err(Refusal::Full { what: "leases", limit: SIM_SECRETS });
            }
            Self::live(&mut st, name, now)?;
            let lease = Lease { id: Self::next_id(&mut st, "lease"), name: name.to_owned(), expires_ms: now.saturating_add(ms(ttl)) };
            st.leases.insert(lease.id.clone(), lease.clone());
            Ok(lease)
        })();
        Box::pin(async move { out })
    }

    fn read_leased<'a>(&'a self, lease: &'a str) -> BoxFuture<'a, Result<SecretValue, Refusal>> {
        let now = self.clock.unix_ms();
        let out = (|| {
            let mut st = self.inner.lock();
            let l = Self::lease_of(&st, lease, now)?;
            let s = Self::live(&mut st, &l.name, now)?;
            s.with(Stage::Current).map(|(_, v)| v.clone()).ok_or(Refusal::NoSuchVersion { name: l.name })
        })();
        Box::pin(async move { out })
    }

    fn renew<'a>(&'a self, lease: &'a str, ttl: Duration) -> BoxFuture<'a, Result<Lease, Refusal>> {
        let now = self.clock.unix_ms();
        let out = (|| {
            if ttl.is_zero() || ttl > LEASE_MAX {
                return Err(Refusal::Invalid { what: "a lease's time", why: format!("{ttl:?} (more than 0, at most a day)") });
            }
            let mut st = self.inner.lock();
            Self::lease_of(&st, lease, now)?;
            let l = st.leases.get_mut(lease).ok_or(Refusal::Expired)?;
            l.expires_ms = now.saturating_add(ms(ttl));
            Ok(l.clone())
        })();
        Box::pin(async move { out })
    }

    fn revoke<'a>(&'a self, lease: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        self.inner.lock().leases.remove(lease);
        Box::pin(async { Ok(()) })
    }
}

// -------------------------------------------------------------------------------- conformance

/// What a `Secrets` conformance suite runs against: an empty store per case, and its clock.
pub trait SecretsFixture: Send + Sync {
    fn secrets(&self) -> &dyn Secrets;
    /// Lets `d` pass by the store's clock.
    fn pass(&self, d: Duration) -> BoxFuture<'_, ()>;
}

/// The fixture for a [`SimSecrets`].
pub struct SimSecretsFixture {
    pub secrets: SimSecrets,
    pub clock: SimClock,
}

impl Default for SimSecretsFixture {
    fn default() -> Self {
        let clock = SimClock::new();
        Self { secrets: SimSecrets::new(Arc::new(clock.clone())), clock }
    }
}

impl SecretsFixture for SimSecretsFixture {
    fn secrets(&self) -> &dyn Secrets {
        &self.secrets
    }
    fn pass(&self, d: Duration) -> BoxFuture<'_, ()> {
        self.clock.advance(d);
        Box::pin(async {})
    }
}

fn v(s: &str) -> SecretValue {
    SecretValue::new(s.as_bytes().to_vec())
}

async fn read(s: &dyn Secrets, name: &str, select: Select) -> Result<String, Failure> {
    let (_, value) = fine("get", s.get(name, select).await)?;
    Ok(String::from_utf8_lossy(value.expose()).into_owned())
}

const N: &str = "conformance/db";
const DAY: Duration = Duration::from_secs(24 * 3600);

fn c1<F: SecretsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.secrets();
        let first = fine("create", s.create(N, v("hunter2")).await)?;
        same("the first version's stages", first.stages, vec![Stage::Current])?;
        same("the current value", read(s, N, Select::Stage(Stage::Current)).await?, "hunter2".to_owned())
    })
}

fn c2<F: SecretsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.secrets();
        fine("create", s.create(N, v("one")).await)?;
        fine("put", s.put(N, v("two"), Stage::Current).await)?;
        same("current", read(s, N, Select::Stage(Stage::Current)).await?, "two".to_owned())?;
        same("previous", read(s, N, Select::Stage(Stage::Previous)).await?, "one".to_owned())?;
        fine("put", s.put(N, v("three"), Stage::Current).await)?;
        same("previous after another", read(s, N, Select::Stage(Stage::Previous)).await?, "two".to_owned())
    })
}

fn c3<F: SecretsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.secrets();
        fine("create", s.create(N, v("old")).await)?;
        let pending = fine("stage", s.put(N, v("new"), Stage::Pending).await)?;
        same("pending", read(s, N, Select::Stage(Stage::Pending)).await?, "new".to_owned())?;
        same("current while it's pending", read(s, N, Select::Stage(Stage::Current)).await?, "old".to_owned())?;
        fine("promote", s.promote(N, &pending.id).await)?;
        same("current once promoted", read(s, N, Select::Stage(Stage::Current)).await?, "new".to_owned())?;
        same("previous once promoted", read(s, N, Select::Stage(Stage::Previous)).await?, "old".to_owned())?;
        ensure(matches!(s.get(N, Select::Stage(Stage::Pending)).await, Err(Refusal::NoSuchVersion { .. })), || "the pending stage stayed after promotion".into())
    })
}

fn c4<F: SecretsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.secrets();
        let first = fine("create", s.create(N, v("one")).await)?;
        for x in ["two", "three", "four"] {
            fine("put", s.put(N, v(x), Stage::Current).await)?;
        }
        same("the first version by its id", read(s, N, Select::Version(first.id)).await?, "one".to_owned())?;
        ensure(matches!(s.get(N, Select::Version("no-such-version".into())).await, Err(Refusal::NoSuchVersion { .. })), || "an unknown version wasn't refused NoSuchVersion".into())
    })
}

fn c5<F: SecretsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.secrets();
        fine("create", s.create(N, v("never-shown-anywhere")).await)?;
        let (_, value) = fine("get", s.get(N, Select::Stage(Stage::Current)).await)?;
        let said = format!("{value:?} {value} {:?} {:?}", fine("describe", s.describe(N).await)?, fine("list", s.list("conformance/", None, 10).await)?);
        ensure(!said.contains("never-shown-anywhere"), || "a value showed in Debug, Display, a describe or a listing".into())
    })
}

fn c6<F: SecretsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.secrets();
        fine("create", s.create(N, v("x")).await)?;
        same("a second create", s.create(N, v("y")).await.map(|v| v.id), Err(Refusal::Exists(N.into())))?;
        same("a get of nothing", s.get("conformance/none", Select::Stage(Stage::Current)).await.map(|x| x.0.id), Err(Refusal::NotFound("conformance/none".into())))?;
        ensure(matches!(s.create("bad name!", v("x")).await, Err(Refusal::Invalid { .. })), || "a bad name wasn't refused Invalid".into())?;
        ensure(matches!(s.create("conformance/big", SecretValue::new(vec![0; VALUE_MAX.saturating_add(1)])).await, Err(Refusal::TooLarge { .. })), || "a value over 64 KiB wasn't refused TooLarge".into())
    })
}

fn c7<F: SecretsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.secrets();
        fine("create", s.create(N, v("x")).await)?;
        let purge = fine("delete", s.delete(N, DAY.saturating_mul(7)).await)?;
        same("a read while it's scheduled", s.get(N, Select::Stage(Stage::Current)).await.map(|x| x.0.id), Err(Refusal::Deleted { name: N.into(), purge_at_ms: purge }))?;
        fine("restore", s.restore(N).await)?;
        same("a read once restored", read(s, N, Select::Stage(Stage::Current)).await?, "x".to_owned())?;
        fine("delete", s.delete(N, DAY.saturating_mul(7)).await)?;
        f.pass(DAY.saturating_mul(8)).await;
        ensure(matches!(s.get(N, Select::Stage(Stage::Current)).await, Err(Refusal::NotFound(_))), || "a secret past its window wasn't gone".into())?;
        ensure(matches!(s.delete("conformance/other", DAY).await, Err(Refusal::Invalid { .. } | Refusal::NotFound(_))), || "a window under 7 days was taken".into())
    })
}

fn c8<F: SecretsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.secrets();
        fine("create", s.create(N, v("leased")).await)?;
        let lease = fine("lease", s.lease(N, Duration::from_secs(60)).await)?;
        same("a read through the lease", fine("read", s.read_leased(&lease.id).await)?.expose().to_vec(), b"leased".to_vec())?;
        f.pass(Duration::from_secs(30)).await;
        fine("renew", s.renew(&lease.id, Duration::from_secs(60)).await)?;
        f.pass(Duration::from_secs(45)).await;
        ensure(s.read_leased(&lease.id).await.is_ok(), || "a renewed lease expired at its first time".into())?;
        f.pass(Duration::from_secs(16)).await;
        same("a read once it expired", s.read_leased(&lease.id).await.map(|x| x.len()), Err(Refusal::Expired))?;
        let other = fine("lease", s.lease(N, Duration::from_secs(60)).await)?;
        fine("revoke", s.revoke(&other.id).await)?;
        same("a read once revoked", s.read_leased(&other.id).await.map(|x| x.len()), Err(Refusal::Expired))
    })
}

fn c9<F: SecretsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.secrets();
        for n in ["conformance/b", "conformance/a", "other/x", "conformance/c"] {
            fine("create", s.create(n, v("x")).await)?;
        }
        same("the first page", fine("list", s.list("conformance/", None, 2).await)?, vec!["conformance/a".to_owned(), "conformance/b".to_owned()])?;
        same("the next", fine("list", s.list("conformance/", Some("conformance/b"), 2).await)?, vec!["conformance/c".to_owned()])
    })
}

/// The `Secrets` contract, clause by clause (docs/seams.md, "Secrets").
pub fn cases<F: SecretsFixture>() -> Vec<Case<F>> {
    vec![
        Case { id: "SECRETS-1", says: "a secret created has one version, current, read back as created", check: c1::<F> },
        Case { id: "SECRETS-2", says: "a version put current becomes AWSCURRENT, and the one it replaced AWSPREVIOUS", check: c2::<F> },
        Case { id: "SECRETS-3", says: "a version put pending is read as AWSPENDING and isn't current until promoted; promoted, the old current is previous and the pending stage goes", check: c3::<F> },
        Case { id: "SECRETS-4", says: "a version is read by its id with the value it was made with, however many came after; an unknown one is refused NoSuchVersion", check: c4::<F> },
        Case { id: "SECRETS-5", says: "a value never shows: not in Debug or Display, a describe or a listing", check: c5::<F> },
        Case { id: "SECRETS-6", says: "creating a name that exists is refused Exists, reading one that doesn't NotFound; bad names and values over 64 KiB are refused", check: c6::<F> },
        Case { id: "SECRETS-7", says: "a delete is scheduled 7 to 30 days out: reads are refused Deleted until it's restored, and once the window passes it's gone", check: c7::<F> },
        Case { id: "SECRETS-8", says: "a lease reads the current value until its time passes by the store's clock, then is refused Expired; renewing extends it, revoking ends it", check: c8::<F> },
        Case { id: "SECRETS-9", says: "a listing gives names with a prefix in name order, after its cursor, at most its limit", check: c9::<F> },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::run;

    /// CP-M2, AC-M2.2: the simulator and its suite.
    #[test]
    fn the_simulated_store_passes_the_secrets_suite() -> Result<(), String> {
        futures::executor::block_on(run("Secrets", &cases::<SimSecretsFixture>(), || async { SimSecretsFixture::default() })).into_result().map(|_| ())
    }

    /// CP-M2, AC-M2.2: a secret's value never shows.
    #[test]
    fn only_a_value_s_length_shows() {
        let value = SecretValue::new(b"abc".to_vec());
        assert_eq!(format!("{value:?}"), "SecretValue(<3 bytes, redacted>)");
        assert_eq!(value.to_string(), "<redacted>");
        assert!(!value.is_empty() && value.expose() == b"abc");
    }
}
