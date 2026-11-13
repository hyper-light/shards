//! The secrets service (docs/deploy-api.md §3.5): an app's secrets for an environment, their
//! versions, who set each, and every read of their values for a delivery.
//!
//! Callers (`/api/deploy`, `kiln`, delivery through KilnCD) talk to [`Secrets`]: the policy is
//! here, the same whatever keeps the values (names, sizes, limits, who may, agents never setting
//! or reading values, the audit). What keeps them is a [`Backend`]: [`kiln`] today (versions
//! sealed in kiln's own store); just-in-time secrets later, swapped in here without the callers
//! changing. Every backend is a module of kiln's one binary: no other process or store.

pub mod kiln;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Serialize;

use crate::hub::Hub;

/// A backend's answer.
pub type Fut<'a, T> = Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send + 'a>>;

/// The most a value holds.
pub const MAX_VALUE: usize = 64 << 10;
/// The most secrets a scope holds.
pub const MAX_PER_SCOPE: usize = 200;
/// The most a scope's values hold together: they're delivered as one Kubernetes Secret, which
/// holds at most 1 MiB, its keys included.
pub const MAX_SCOPE_BYTES: usize = 960 << 10;
/// How many versions keep their values; older ones keep only their record.
pub const KEPT_VERSIONS: u32 = 10;

/// What secrets belong to: an app in an environment, today. The service will scope by more (a
/// project, a team, a path) without its callers changing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Scope {
    pub app: String,
    pub environment: String,
}

impl Scope {
    pub fn of(app: &str, environment: &str) -> Scope {
        Scope { app: app.to_owned(), environment: environment.to_owned() }
    }

    /// Its key in a backend: `app/<app>/<environment>`.
    pub fn key(&self) -> String {
        format!("app/{}/{}", self.app, self.environment)
    }

    /// The scope a key names.
    pub fn from_key(key: &str) -> Option<Scope> {
        let (app, environment) = key.strip_prefix("app/")?.split_once('/')?;
        (!app.is_empty() && !environment.is_empty() && !environment.contains('/')).then(|| Scope::of(app, environment))
    }
}

/// Versions as a release pins them and a render key holds them: `API_KEY@1,DB_PASSWORD@4`,
/// by name.
pub fn versions_text<'a>(items: impl IntoIterator<Item = (&'a str, u32)>) -> String {
    let mut v: Vec<(&str, u32)> = items.into_iter().collect();
    v.sort_unstable();
    v.iter().map(|(n, x)| format!("{n}@{x}")).collect::<Vec<_>>().join(",")
}

/// Reads [`versions_text`] back.
pub fn parse_versions(text: &str) -> anyhow::Result<Vec<(String, u32)>> {
    text.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(|item| {
            let (name, v) = item.rsplit_once('@').ok_or_else(|| anyhow::anyhow!("`{item}` isn't NAME@version"))?;
            check_name(name).map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((name.to_owned(), v.parse().map_err(|_| anyhow::anyhow!("`{item}`: its version isn't a number"))?))
        })
        .collect()
}

/// A secret's value: its bytes, never printed, wiped when dropped.
pub struct Value(Vec<u8>);

impl Value {
    pub fn new(bytes: Vec<u8>) -> Value {
        Value(bytes)
    }

    pub fn bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Value(<{} bytes>)", self.0.len())
    }
}

impl Drop for Value {
    fn drop(&mut self) {
        self.0.fill(0);
        // Zeros nothing reads again are dead stores to an optimizer: this read keeps them.
        std::hint::black_box(&self.0);
    }
}

/// A secret as listed: never its value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Meta {
    pub name: String,
    pub version: u32,
    pub size: usize,
    pub set_by: String,
    pub set_at: i64,
}

/// One version of a secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Version {
    pub version: u32,
    pub set_by: String,
    pub set_at: i64,
    /// A deletion: the secret was gone from this version on.
    pub deleted: bool,
    /// Its value is no longer kept (older than the versions kept), or it never had one.
    pub destroyed: bool,
}

/// The values a delivery takes: each secret's name, version and value, and until when they hold
/// (a lease, from a backend that issues them; never, from kiln's own).
#[derive(Debug, Default)]
pub struct Bundle {
    pub items: Vec<(String, u32, Value)>,
    pub expires_at: Option<i64>,
}

impl Bundle {
    /// What the values are, without them: `NAME@3,OTHER@1`. What a render key holds of them.
    pub fn versions(&self) -> String {
        versions_text(self.items.iter().map(|(n, v, _)| (n.as_str(), *v)))
    }
}

/// Why values are read: recorded with each read.
#[derive(Debug, Clone)]
pub struct Use {
    /// Who reads them (`kilncd`).
    pub by: String,
    /// For what (`render shop/web-production`).
    pub purpose: String,
}

/// A read recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Read {
    pub by: String,
    pub purpose: String,
    pub versions: String,
    pub at: i64,
}

/// What keeps secrets. Each call is about one scope; a backend checks nothing the service does.
pub trait Backend: Send + Sync {
    /// Its name, as the API says it (`kiln`).
    fn name(&self) -> &'static str;
    /// Each secret of the scope, at its current version (deleted ones left out).
    fn list<'a>(&'a self, at: &'a Scope) -> Fut<'a, Vec<Meta>>;
    /// A secret's versions, newest first.
    fn versions<'a>(&'a self, at: &'a Scope, name: &'a str) -> Fut<'a, Vec<Version>>;
    /// Keeps `value` as the secret's next version.
    fn put<'a>(&'a self, at: &'a Scope, name: &'a str, value: Value, by: &'a str) -> Fut<'a, Meta>;
    /// Deletes the secret (its history stays): whether there was one.
    fn delete<'a>(&'a self, at: &'a Scope, name: &'a str, by: &'a str) -> Fut<'a, bool>;
    /// The values of exactly these versions, for a delivery, the read recorded. A version whose
    /// value isn't kept is an error that says so.
    fn resolve<'a>(&'a self, at: &'a Scope, want: &'a [(String, u32)], why: &'a Use) -> Fut<'a, Bundle>;
    /// The reads recorded, newest first.
    fn reads<'a>(&'a self, at: &'a Scope, limit: usize) -> Fut<'a, Vec<Read>>;
    /// Which of these versions have no value kept (older than those kept, deleted, never set):
    /// what a delivery of them would fail on, asked without reading a value.
    fn missing<'a>(&'a self, at: &'a Scope, want: &'a [(String, u32)]) -> Fut<'a, Vec<(String, u32)>>;
    /// Re-seals what it keeps under new keys: how many values. Nothing to do for some.
    fn rotate(&self) -> Fut<'_, usize>;
    /// Deletes every secret of the scope and destroys their values (their records stay): what
    /// the scope going away means. How many secrets it had.
    fn forget<'a>(&'a self, at: &'a Scope, by: &'a str) -> Fut<'a, usize>;
}

/// Why a call didn't happen, as the API says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The request is wrong (a name, a size).
    Bad(String),
    /// The caller may not.
    Forbidden(String),
    Missing(String),
    /// The backend failed.
    Failed(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Bad(m) | Refused::Forbidden(m) | Refused::Missing(m) | Refused::Failed(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Refused {}

impl From<anyhow::Error> for Refused {
    fn from(e: anyhow::Error) -> Refused {
        Refused::Failed(format!("{e:#}"))
    }
}

/// Who asks: a person or an agent, by the name the audit says.
#[derive(Debug, Clone)]
pub struct Caller {
    pub name: String,
    pub agent: bool,
}

/// What a caller does with a scope's secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Their names and versions, and the reads of them.
    List,
    /// A new version.
    Put,
    Delete,
    /// Every value re-sealed under new keys.
    Rotate,
}

/// The service: the policy, over a backend.
pub struct Secrets {
    hub: Arc<Hub>,
    backend: Arc<dyn Backend>,
}

/// The service over the backend this server keeps secrets in: kiln's own, until just-in-time
/// secrets (another module of this binary, picked here).
pub fn of(hub: &Arc<Hub>) -> Secrets {
    Secrets { hub: Arc::clone(hub), backend: Arc::new(kiln::KilnBackend::new(Arc::clone(hub))) }
}

/// Whether `name` may name a secret: an environment variable's name, upper case.
pub fn check_name(name: &str) -> Result<(), Refused> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name.bytes().next().is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
        && name.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
    ok.then_some(()).ok_or_else(|| Refused::Bad(format!("`{name}`: a secret's name is an environment variable's: A-Z, 0-9 and _, not starting with a digit")))
}

impl Secrets {
    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Whether `who` may `act` on a scope: the service's policy, in one place (the route table has
    /// said who may call at all). Today: agents look, people change. Vault-like policies (by
    /// path, by capability) come here.
    fn may(&self, who: &Caller, act: Action) -> Result<(), Refused> {
        match act {
            Action::Put | Action::Delete | Action::Rotate if who.agent => Err(Refused::Forbidden("agents don't set, delete or rotate secrets: a person does".into())),
            _ => Ok(()),
        }
    }

    pub async fn list(&self, at: &Scope, who: &Caller) -> Result<Vec<Meta>, Refused> {
        self.may(who, Action::List)?;
        Ok(self.backend.list(at).await?)
    }

    pub async fn versions(&self, at: &Scope, name: &str, who: &Caller) -> Result<Vec<Version>, Refused> {
        self.may(who, Action::List)?;
        check_name(name)?;
        let v = self.backend.versions(at, name).await?;
        if v.is_empty() {
            return Err(Refused::Missing(format!("no secret {name} for {} in {}", at.app, at.environment)));
        }
        Ok(v)
    }

    /// Sets a secret's next version.
    pub async fn put(&self, at: &Scope, name: &str, value: Value, who: &Caller) -> Result<Meta, Refused> {
        self.may(who, Action::Put)?;
        check_name(name)?;
        if value.is_empty() {
            return Err(Refused::Bad("a secret's value is empty: delete it instead".into()));
        }
        if value.len() > MAX_VALUE {
            return Err(Refused::Bad(format!("a secret's value is at most {} KiB", MAX_VALUE >> 10)));
        }
        let now = self.backend.list(at).await?;
        if now.len() >= MAX_PER_SCOPE && !now.iter().any(|m| m.name == name) {
            return Err(Refused::Bad(format!("{} in {} holds {MAX_PER_SCOPE} secrets already", at.app, at.environment)));
        }
        let others: usize = now.iter().filter(|m| m.name != name).map(|m| m.size).sum();
        if others + value.len() > MAX_SCOPE_BYTES {
            return Err(Refused::Bad(format!(
                "{} in {} would hold more than {} KiB of secrets: they're delivered as one Kubernetes Secret, which holds at most 1 MiB",
                at.app,
                at.environment,
                MAX_SCOPE_BYTES >> 10
            )));
        }
        let meta = self.backend.put(at, name, value, &who.name).await?;
        self.hub.store.audit(who.name.clone(), "secrets.put".into(), format!("{} {name} v{}", at.key(), meta.version)).await;
        Ok(meta)
    }

    /// Deletes a secret (its history stays).
    pub async fn delete(&self, at: &Scope, name: &str, who: &Caller) -> Result<(), Refused> {
        self.may(who, Action::Delete)?;
        check_name(name)?;
        if !self.backend.delete(at, name, &who.name).await? {
            return Err(Refused::Missing(format!("no secret {name} for {} in {}", at.app, at.environment)));
        }
        self.hub.store.audit(who.name.clone(), "secrets.delete".into(), format!("{} {name}", at.key())).await;
        Ok(())
    }

    /// The reads of a scope's values for deliveries, newest first.
    pub async fn reads(&self, at: &Scope, limit: usize, who: &Caller) -> Result<Vec<Read>, Refused> {
        self.may(who, Action::List)?;
        Ok(self.backend.reads(at, limit.clamp(1, 1000)).await?)
    }

    /// Each secret's current version, for a release to pin: `[(name, version)]`, by name.
    pub async fn current(&self, at: &Scope) -> anyhow::Result<Vec<(String, u32)>> {
        Ok(self.backend.list(at).await?.into_iter().map(|m| (m.name, m.version)).collect())
    }

    /// Which of the versions a release pins have no value kept: a release's preflight (§8.1).
    pub async fn missing(&self, at: &Scope, want: &[(String, u32)]) -> anyhow::Result<Vec<(String, u32)>> {
        self.backend.missing(at, want).await
    }

    /// The values of the versions a release pinned, for a delivery: never for a person or an
    /// agent.
    pub async fn resolve(&self, at: &Scope, want: &[(String, u32)], why: &Use) -> anyhow::Result<Bundle> {
        self.backend.resolve(at, want, why).await
    }

    /// Deletes a scope's secrets and destroys their values, as what they belong to goes away (an
    /// app, an environment). `by` deleted it.
    pub async fn forget(&self, at: &Scope, by: &str) -> anyhow::Result<usize> {
        let n = self.backend.forget(at, by).await?;
        if n > 0 {
            self.hub.store.audit(by.to_owned(), "secrets.forget".into(), format!("{} ({n} secrets)", at.key())).await;
        }
        Ok(n)
    }

    /// Re-seals every value the backend keeps under new keys: how many.
    pub async fn rotate(&self, who: &Caller) -> Result<usize, Refused> {
        self.may(who, Action::Rotate)?;
        let n = self.backend.rotate().await?;
        self.hub.store.audit(who.name.clone(), "secrets.rotate".into(), format!("{} re-sealed {n} values under a new data key", self.backend.name())).await;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_environment_variables_and_values_never_print() {
        for good in ["DB_PASSWORD", "_TOKEN", "API_KEY_2"] {
            assert!(check_name(good).is_ok(), "{good}");
        }
        let long = "X".repeat(129);
        for wrong in ["", "db_password", "2FA", "A-B", "A B", long.as_str()] {
            assert!(check_name(wrong).is_err(), "{wrong}");
        }
        let v = Value::new(b"hunter2".to_vec());
        assert_eq!(format!("{v:?}"), "Value(<7 bytes>)");
        let b = Bundle { items: vec![("A".into(), 3, Value::new(b"hunter2".to_vec())), ("B".into(), 1, Value::new(b"swordfish".to_vec()))], expires_at: None };
        assert_eq!(b.versions(), "A@3,B@1");
        let shown = format!("{b:?}");
        assert!(!shown.contains("hunter2") && !shown.contains("swordfish"), "a bundle prints no value: {shown}");
        assert_eq!(versions_text([("B", 1), ("A", 3)]), "A@3,B@1");
        assert_eq!(parse_versions("A@3, B@1").ok(), Some(vec![("A".to_owned(), 3), ("B".to_owned(), 1)]));
        assert_eq!(parse_versions("").ok(), Some(Vec::new()));
        assert!(parse_versions("A").is_err() && parse_versions("a@1").is_err() && parse_versions("A@x").is_err());
        assert_eq!(Scope::from_key("app/web/staging"), Some(Scope::of("web", "staging")));
        assert_eq!(Scope::from_key("app/web"), None);
    }
}
