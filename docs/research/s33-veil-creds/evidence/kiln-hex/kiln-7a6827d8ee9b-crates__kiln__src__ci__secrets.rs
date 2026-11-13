//! Secrets and variables on the server (docs/ci.md W09).
//!
//! - **Encryption (W09-R03).** Every value is sealed with AES-256-GCM, with a fresh nonce, under
//!   a data key. The data key is wrapped by the master key file (`<data>/secrets/master.key`,
//!   32 random bytes, mode 0600, made at first use). A ciphertext is bound to its scope and name,
//!   so one can't be moved to another row. The database holds no plaintext, and nothing here
//!   returns a value except to a leg.
//! - **Resolution (W09-R01).** A name resolves to its most specific scope: environment, then
//!   workflow, then repository, then organization, then server. The `[secrets]` of the server's
//!   config count as server scope, below stored ones.
//! - **Uses (W09-R04).** Every delivery to a leg is recorded.
//! - **Variables (W09-R09).** The same scopes, in plain text.
//! - **Rotation.** [`rotate`] re-seals every value under a new data key.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use kiln_ci::secrets::{self as rules, ScopeKind, Used};
use rusqlite::{OptionalExtension, params};

use crate::hub::Hub;
use crate::store::Repo;

/// A scope, as stored: its kind and id (`""`, an owner, a repository id, `<repo id>:<path>` or
/// `<repo id>:<environment>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub kind: ScopeKind,
    pub id: String,
}

impl Scope {
    fn aad(&self, name: &str) -> Vec<u8> {
        format!("kiln secret\0{}\0{}\0{name}", self.kind.as_str(), self.id).into_bytes()
    }
}

/// A scope as people write it: `server`, `org:<owner>`, `repo:<owner/name or id>`,
/// `workflow:<repo>:<path>`, `env:<repo>:<environment>`.
pub async fn parse_scope(hub: &Hub, s: &str) -> Result<Scope, String> {
    let s = s.trim();
    if s == "server" || s.is_empty() {
        return Ok(Scope { kind: ScopeKind::Server, id: String::new() });
    }
    let (kind, rest) = s.split_once(':').ok_or_else(|| format!("`{s}` is not a scope: server, org:<owner>, repo:<owner/name>, workflow:<repo>:<path> or env:<repo>:<name>"))?;
    let repo = |name: &str| {
        let name = name.to_owned();
        async move {
            let repos = hub.store.repos().await.map_err(|e| format!("{e:#}"))?;
            repos.into_iter().find(|r| r.id == name || r.name.eq_ignore_ascii_case(&name)).ok_or_else(|| format!("no repository `{name}`"))
        }
    };
    match ScopeKind::parse(kind) {
        Some(ScopeKind::Organization) if !rest.is_empty() => Ok(Scope { kind: ScopeKind::Organization, id: rest.to_ascii_lowercase() }),
        Some(ScopeKind::Repository) => Ok(Scope { kind: ScopeKind::Repository, id: repo(rest).await?.id }),
        Some(k @ (ScopeKind::Workflow | ScopeKind::Environment)) => {
            let (r, tail) = rest.split_once(':').ok_or_else(|| format!("`{s}`: say {kind}:<repo>:<{}>", if k == ScopeKind::Workflow { "path" } else { "environment" }))?;
            if tail.is_empty() {
                return Err(format!("`{s}` names no {}", if k == ScopeKind::Workflow { "workflow" } else { "environment" }));
            }
            Ok(Scope { kind: k, id: format!("{}:{tail}", repo(r).await?.id) })
        }
        _ => Err(format!("`{s}` is not a scope: server, org:<owner>, repo:<owner/name>, workflow:<repo>:<path> or env:<repo>:<name>")),
    }
}

/// The scopes a leg of `repo`'s `workflow` reads, bound to `environment`, from the most
/// specific.
fn scopes_for(repo: &Repo, workflow: &str, environment: Option<&str>) -> Vec<Scope> {
    let owner = repo.name.split_once('/').map_or(repo.name.as_str(), |(o, _)| o).to_ascii_lowercase();
    let mut v = Vec::new();
    if let Some(e) = environment.filter(|e| !e.is_empty()) {
        v.push(Scope { kind: ScopeKind::Environment, id: format!("{}:{e}", repo.id) });
    }
    v.push(Scope { kind: ScopeKind::Workflow, id: format!("{}:{workflow}", repo.id) });
    v.push(Scope { kind: ScopeKind::Repository, id: repo.id.clone() });
    v.push(Scope { kind: ScopeKind::Organization, id: owner });
    v.push(Scope { kind: ScopeKind::Server, id: String::new() });
    v
}

// ------------------------------------------------------------------ keys

fn master_path(hub: &Hub) -> PathBuf {
    hub.cfg.data("secrets").join("master.key")
}

fn random<const N: usize>() -> anyhow::Result<[u8; N]> {
    let mut b = [0u8; N];
    SystemRandom::new().fill(&mut b).map_err(|_| anyhow::anyhow!("the system's random source failed"))?;
    Ok(b)
}

/// The master key, made at first use.
fn master(hub: &Hub) -> anyhow::Result<[u8; 32]> {
    master_at(&master_path(hub))
}

fn master_at(p: &std::path::Path) -> anyhow::Result<[u8; 32]> {
    match std::fs::read(p) {
        Ok(b) if b.len() == 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b);
            Ok(k)
        }
        Ok(_) => anyhow::bail!("{} is not a 32-byte key", p.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let k: [u8; 32] = random()?;
            crate::util::write_secret(p, &k, 0o600)?;
            Ok(k)
        }
        Err(e) => Err(e.into()),
    }
}

/// A key of its own for `label`, from the master key: one kiln checks with and never stores (an
/// inbound webhook's secret, W03-R02).
pub(crate) fn derived(hub: &Hub, label: &str) -> anyhow::Result<String> {
    Ok(crate::util::hmac_hex(&master(hub)?, label.as_bytes()))
}

fn key(bytes: &[u8; 32]) -> anyhow::Result<LessSafeKey> {
    Ok(LessSafeKey::new(UnboundKey::new(&AES_256_GCM, bytes).map_err(|_| anyhow::anyhow!("bad key"))?))
}

/// Seals `plain` under `k`, bound to `aad`: `(ciphertext with tag, nonce)`.
fn seal(k: &[u8; 32], plain: &[u8], aad: &[u8]) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let nonce: [u8; NONCE_LEN] = random()?;
    let mut buf = plain.to_vec();
    key(k)?.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(aad), &mut buf).map_err(|_| anyhow::anyhow!("sealing failed"))?;
    Ok((buf, nonce.to_vec()))
}

fn open(k: &[u8; 32], sealed: &[u8], nonce: &[u8], aad: &[u8]) -> anyhow::Result<Vec<u8>> {
    let n: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| anyhow::anyhow!("bad nonce"))?;
    let mut buf = sealed.to_vec();
    let plain = key(k)?.open_in_place(Nonce::assume_unique_for_key(n), Aad::from(aad), &mut buf).map_err(|_| anyhow::anyhow!("a secret doesn't decrypt: the master key or the database changed"))?;
    Ok(plain.to_vec())
}

/// Seals `plain` under the master key, bound to `aad` (keys of other subsystems: OIDC, W10).
pub(crate) fn seal_master(hub: &Hub, plain: &[u8], aad: &[u8]) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    seal(&master(hub)?, plain, aad)
}

pub(crate) fn open_master(hub: &Hub, sealed: &[u8], nonce: &[u8], aad: &[u8]) -> anyhow::Result<Vec<u8>> {
    open(&master(hub)?, sealed, nonce, aad)
}

/// Seals `plain` under the master key at `master` (`<data>/secrets/master.key`), bound to `aad`:
/// for subsystems without the hub (observe's cloud credentials).
pub(crate) fn seal_with(master: &std::path::Path, plain: &[u8], aad: &[u8]) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    seal(&master_at(master)?, plain, aad)
}

pub(crate) fn open_with(master: &std::path::Path, sealed: &[u8], nonce: &[u8], aad: &[u8]) -> anyhow::Result<Vec<u8>> {
    open(&master_at(master)?, sealed, nonce, aad)
}

/// The current data key: `(id, key)`, made (and wrapped) when there is none.
async fn current_key(hub: &Hub) -> anyhow::Result<(String, [u8; 32])> {
    let m = master(hub)?;
    hub.store
        .call(move |c| {
            let row: Option<(String, Vec<u8>, Vec<u8>)> = c.query_row("SELECT id, wrapped, nonce FROM ci_keys WHERE retired_at IS NULL ORDER BY created_at DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
            if let Some((id, wrapped, nonce)) = row {
                return Ok((id.clone(), unwrap_key(&m, &wrapped, &nonce, &id)?));
            }
            let (id, fresh) = new_key(c, &m)?;
            Ok((id, fresh))
        })
        .await
}

fn new_key(c: &rusqlite::Connection, m: &[u8; 32]) -> anyhow::Result<(String, [u8; 32])> {
    let id = crate::util::new_id();
    let fresh: [u8; 32] = random()?;
    let (wrapped, nonce) = seal(m, &fresh, format!("kiln data key\0{id}").as_bytes())?;
    c.execute("INSERT INTO ci_keys (id, wrapped, nonce, created_at) VALUES (?1, ?2, ?3, ?4)", params![id, wrapped, nonce, crate::util::now_unix()])?;
    Ok((id, fresh))
}

fn unwrap_key(m: &[u8; 32], wrapped: &[u8], nonce: &[u8], id: &str) -> anyhow::Result<[u8; 32]> {
    let raw = open(m, wrapped, nonce, format!("kiln data key\0{id}").as_bytes())?;
    raw.try_into().map_err(|_| anyhow::anyhow!("a data key is not 32 bytes"))
}

/// Every data key, unwrapped, by id.
fn all_keys(c: &rusqlite::Connection, m: &[u8; 32]) -> anyhow::Result<HashMap<String, [u8; 32]>> {
    let mut q = c.prepare("SELECT id, wrapped, nonce FROM ci_keys")?;
    let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, Vec<u8>>(2)?)))?;
    let mut out = HashMap::new();
    for row in rows {
        let (id, w, n) = row?;
        out.insert(id.clone(), unwrap_key(m, &w, &n, &id)?);
    }
    Ok(out)
}

// ------------------------------------------------------------------ secrets

/// A secret as reads describe it: never its value.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Meta {
    pub name: String,
    pub size: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_used_at: Option<i64>,
    /// A brokered secret's source (W09-R08); its value is made at leg start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// Sets a secret (W09-R02, W09-R03).
pub async fn set(hub: &Hub, scope: &Scope, name: &str, value: &str, by: &str) -> Result<(), String> {
    store_row(hub, scope, name, value, None, by).await
}

/// Stores a secret's row: a value sealed, or a brokered one's source (its value empty).
async fn store_row(hub: &Hub, scope: &Scope, name: &str, value: &str, from: Option<String>, by: &str) -> Result<(), String> {
    let name = rules::check_name(name)?;
    if value.len() > rules::MAX_SECRET {
        return Err(format!("a secret is at most {} KiB", rules::MAX_SECRET >> 10));
    }
    let (key_id, k) = current_key(hub).await.map_err(|e| format!("{e:#}"))?;
    let (sealed, nonce) = seal(&k, value.as_bytes(), &scope.aad(&name)).map_err(|e| format!("{e:#}"))?;
    let (s, by, size) = (scope.clone(), by.to_owned(), i64::try_from(value.len()).unwrap_or(0));
    hub.store
        .call(move |c| {
            let n: i64 = c.query_row("SELECT COUNT(*) FROM ci_secrets WHERE scope_kind = ?1 AND scope_id = ?2 AND name != ?3", params![s.kind.as_str(), s.id, name], |r| r.get(0))?;
            if usize::try_from(n).unwrap_or(usize::MAX) >= rules::MAX_PER_SCOPE {
                anyhow::bail!("a scope holds at most {} secrets", rules::MAX_PER_SCOPE);
            }
            let now = crate::util::now_unix();
            c.execute(
                "INSERT INTO ci_secrets (id, scope_kind, scope_id, name, ciphertext, nonce, key_id, size, created_by, created_at, updated_at, broker) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?11)
                 ON CONFLICT (scope_kind, scope_id, name) DO UPDATE SET ciphertext = excluded.ciphertext, nonce = excluded.nonce, key_id = excluded.key_id, size = excluded.size, updated_at = excluded.updated_at, broker = excluded.broker",
                params![crate::util::new_id(), s.kind.as_str(), s.id, name, sealed, nonce, key_id, size, by, now, from],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Defines a secret as brokered (W09-R08): `from` says where its value comes from at leg start.
pub async fn set_brokered(hub: &Hub, scope: &Scope, name: &str, from: &str, by: &str) -> Result<(), String> {
    super::broker::parse(from)?;
    store_row(hub, scope, name, "", Some(from.trim().to_owned()), by).await
}

pub async fn delete(hub: &Hub, scope: &Scope, name: &str) -> anyhow::Result<bool> {
    let (s, name) = (scope.clone(), name.to_ascii_uppercase());
    hub.store.call(move |c| Ok(c.execute("DELETE FROM ci_secrets WHERE scope_kind = ?1 AND scope_id = ?2 AND name = ?3", params![s.kind.as_str(), s.id, name])? > 0)).await
}

pub async fn list(hub: &Hub, scope: &Scope) -> anyhow::Result<Vec<Meta>> {
    let s = scope.clone();
    hub.store
        .call(move |c| {
            let mut q = c.prepare("SELECT name, size, created_at, updated_at, last_used_at, broker FROM ci_secrets WHERE scope_kind = ?1 AND scope_id = ?2 ORDER BY name")?;
            let rows = q.query_map(params![s.kind.as_str(), s.id], |r| Ok(Meta { name: r.get(0)?, size: r.get(1)?, created_at: r.get(2)?, updated_at: r.get(3)?, last_used_at: r.get(4)?, from: r.get(5)? }))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

/// One secret's value, decrypted, for the server's own use: an integration's credential (W11),
/// which lives in [`ScopeKind::Integration`] and is never resolved for a leg.
pub(crate) async fn reveal(hub: &Hub, scope: &Scope, name: &str) -> anyhow::Result<Option<String>> {
    let (s, name) = (scope.clone(), name.to_ascii_uppercase());
    let m = master(hub)?;
    hub.store
        .call(move |c| {
            let row: Option<(Vec<u8>, Vec<u8>, String)> = c
                .query_row("SELECT ciphertext, nonce, key_id FROM ci_secrets WHERE scope_kind = ?1 AND scope_id = ?2 AND name = ?3", params![s.kind.as_str(), s.id, name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?;
            let Some((sealed, nonce, key_id)) = row else { return Ok(None) };
            let keys = all_keys(c, &m)?;
            let k = keys.get(&key_id).ok_or_else(|| anyhow::anyhow!("secret {name}: its data key is gone"))?;
            let plain = open(k, &sealed, &nonce, &s.aad(&name))?;
            Ok(Some(String::from_utf8_lossy(&plain).into_owned()))
        })
        .await
}

/// A resolved secret: its value, and its row (`None` for the config's).
#[derive(Debug, Clone)]
pub struct Resolved {
    pub value: String,
    pub id: Option<String>,
    pub scope: ScopeKind,
}

/// The secrets a leg of `repo`'s `workflow` (bound to `environment`) may draw on, resolved by
/// scope and decrypted: the names in `wanted`, or all of them (W09-R01, W09-R04). Brokered ones
/// (W09-R08) are made now, for the leg acting as `w`; those that couldn't be, with why, come
/// second.
pub async fn for_leg(hub: &Hub, repo: &Repo, workflow: &str, environment: Option<&str>, wanted: &Used, w: &kiln_ci::oidc::Workload<'_>) -> anyhow::Result<(BTreeMap<String, Resolved>, Vec<(String, String)>)> {
    let scopes = scopes_for(repo, workflow, environment);
    let wanted = wanted.clone();
    let m = master(hub)?;
    let (all, names) = (wanted.all, wanted.names.clone());
    let (mut stored, brokered): (BTreeMap<String, Resolved>, Vec<(String, String, String, ScopeKind)>) = hub
        .store
        .call(move |c| {
            let keys = all_keys(c, &m)?;
            let mut out: BTreeMap<String, Resolved> = BTreeMap::new();
            let mut brokered: Vec<(String, String, String, ScopeKind)> = Vec::new();
            // From the least specific up, so the most specific wins.
            for s in scopes.iter().rev() {
                let mut q = c.prepare("SELECT id, name, ciphertext, nonce, key_id, broker FROM ci_secrets WHERE scope_kind = ?1 AND scope_id = ?2")?;
                let rows = q.query_map(params![s.kind.as_str(), s.id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Vec<u8>>(2)?, r.get::<_, Vec<u8>>(3)?, r.get::<_, String>(4)?, r.get::<_, Option<String>>(5)?)))?;
                for row in rows {
                    let (id, name, sealed, nonce, key_id, broker) = row?;
                    if !all && !names.contains(&name) {
                        continue;
                    }
                    if let Some(from) = broker {
                        // A more specific scope's wins here too.
                        out.remove(&name);
                        brokered.retain(|b| b.0 != name);
                        brokered.push((name, from, id, s.kind));
                        continue;
                    }
                    brokered.retain(|b| b.0 != name);
                    let k = keys.get(&key_id).ok_or_else(|| anyhow::anyhow!("secret {name}: its data key is gone"))?;
                    let plain = open(k, &sealed, &nonce, &s.aad(&name))?;
                    out.insert(name, Resolved { value: String::from_utf8_lossy(&plain).into_owned(), id: Some(id), scope: s.kind });
                }
            }
            Ok((out, brokered))
        })
        .await?;
    let mut failed = Vec::new();
    for (name, from, id, scope) in brokered {
        let made = match super::broker::parse(&from) {
            Ok(src) => super::broker::value(hub, repo, w, &src).await.map_err(|e| format!("{e:#}")),
            Err(e) => Err(e),
        };
        match made {
            Ok(value) => {
                stored.insert(name, Resolved { value, id: Some(id), scope });
            }
            Err(e) => failed.push((name, format!("from {from}: {e}"))),
        }
    }
    // The config's `[secrets]`: server scope, beneath stored ones.
    let mut out: BTreeMap<String, Resolved> = BTreeMap::new();
    for (k, v) in hub.ci_secrets() {
        let name = k.to_ascii_uppercase();
        if wanted.all || wanted.names.contains(&name) {
            out.insert(name, Resolved { value: v, id: None, scope: ScopeKind::Server });
        }
    }
    for (name, r) in stored {
        let keep = out.get(&name).is_none_or(|have| r.scope >= have.scope);
        if keep {
            out.insert(name, r);
        }
    }
    Ok((out, failed))
}

/// The names a leg of `repo`'s `workflow` bound to `environment` could resolve, without
/// decrypting anything (W09-R06's check).
pub async fn names_for(hub: &Hub, repo: &Repo, workflow: &str, environment: Option<&str>) -> anyhow::Result<std::collections::BTreeSet<String>> {
    let scopes = scopes_for(repo, workflow, environment);
    let mut names: std::collections::BTreeSet<String> = hub
        .store
        .call(move |c| {
            let mut out = std::collections::BTreeSet::new();
            for s in &scopes {
                let mut q = c.prepare("SELECT name FROM ci_secrets WHERE scope_kind = ?1 AND scope_id = ?2")?;
                let rows = q.query_map(params![s.kind.as_str(), s.id], |r| r.get::<_, String>(0))?;
                for r in rows {
                    out.insert(r?);
                }
            }
            Ok(out)
        })
        .await?;
    names.extend(hub.ci_secrets().into_iter().map(|(k, _)| k.to_ascii_uppercase()));
    Ok(names)
}

/// Where each secret and variable `repo`'s `workflow` may read is set, by name: its narrowest
/// scope's word (`workflow`, `repository`, `organization`, `server`), as Ship's Pipelines says it
/// (docs/ci.md W29-R04). A secret of the server's own config is the server's. Names only.
pub async fn set_in(hub: &Hub, repo: &Repo, workflow: &str) -> anyhow::Result<HashMap<String, String>> {
    let scopes = scopes_for(repo, workflow, None);
    let mut out: HashMap<String, String> = hub
        .store
        .call(move |c| {
            let mut out = HashMap::new();
            for s in &scopes {
                for q in ["SELECT name FROM ci_secrets WHERE scope_kind = ?1 AND scope_id = ?2", "SELECT name FROM ci_variables WHERE scope_kind = ?1 AND scope_id = ?2"] {
                    let mut st = c.prepare(q)?;
                    for name in st.query_map(params![s.kind.as_str(), s.id], |r| r.get::<_, String>(0))? {
                        out.entry(name?).or_insert_with(|| s.kind.as_str().to_owned());
                    }
                }
            }
            Ok(out)
        })
        .await?;
    for (k, _) in hub.ci_secrets() {
        out.entry(k.to_ascii_uppercase()).or_insert_with(|| ScopeKind::Server.as_str().to_owned());
    }
    Ok(out)
}

/// The scopes searched for a leg, as people read them.
pub fn searched(repo: &Repo, workflow: &str, environment: Option<&str>) -> String {
    let owner = repo.name.split_once('/').map_or(repo.name.as_str(), |(o, _)| o);
    let mut v = Vec::new();
    if let Some(e) = environment {
        v.push(format!("environment `{e}`"));
    }
    v.push(format!("workflow `{workflow}`"));
    v.push(format!("repository {}", repo.name));
    v.push(format!("organization {owner}"));
    v.push("server".into());
    v.join(", ")
}

/// Records that `leg` was given these secrets (W09-R04).
pub async fn record_uses(hub: &Hub, leg: &str, ids: Vec<String>) -> anyhow::Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let leg = leg.to_owned();
    let l = leg.clone();
    let names: Vec<String> = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let now = crate::util::now_unix();
            let mut names = Vec::new();
            for id in &ids {
                tx.execute("INSERT OR REPLACE INTO ci_secret_uses (secret_id, leg_id, ts) VALUES (?1, ?2, ?3)", params![id, l, now])?;
                tx.execute("UPDATE ci_secrets SET last_used_at = ?2 WHERE id = ?1", params![id, now])?;
                if let Some(n) = tx.query_row("SELECT name FROM ci_secrets WHERE id = ?1", [id], |r| r.get::<_, String>(0)).optional()? {
                    names.push(n);
                }
            }
            tx.commit()?;
            Ok(names)
        })
        .await?;
    // A secret's use is audited (W19-R05): which leg was given which, by name.
    hub.store.audit(format!("job:{leg}"), "ci.secrets.use".into(), names.join(", ")).await;
    Ok(())
}

/// Re-seals every secret under a new data key and retires the old ones: how many moved.
pub async fn rotate(hub: &Hub) -> anyhow::Result<usize> {
    let m = master(hub)?;
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let keys = all_keys(&tx, &m)?;
            let (new_id, fresh) = new_key(&tx, &m)?;
            let rows: Vec<(String, String, String, String, Vec<u8>, Vec<u8>, String)> = {
                let mut q = tx.prepare("SELECT id, scope_kind, scope_id, name, ciphertext, nonce, key_id FROM ci_secrets")?;
                let r = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?;
                r.collect::<Result<Vec<_>, _>>()?
            };
            for (id, kind, scope_id, name, sealed, nonce, key_id) in &rows {
                let s = Scope { kind: ScopeKind::parse(kind).unwrap_or(ScopeKind::Server), id: scope_id.clone() };
                let k = keys.get(key_id).ok_or_else(|| anyhow::anyhow!("secret {name}: its data key is gone"))?;
                let plain = open(k, sealed, nonce, &s.aad(name))?;
                let (resealed, n) = seal(&fresh, &plain, &s.aad(name))?;
                tx.execute("UPDATE ci_secrets SET ciphertext = ?2, nonce = ?3, key_id = ?4 WHERE id = ?1", params![id, resealed, n, new_id])?;
            }
            tx.execute("UPDATE ci_keys SET retired_at = ?2 WHERE id != ?1 AND retired_at IS NULL", params![new_id, crate::util::now_unix()])?;
            tx.execute("DELETE FROM ci_keys WHERE retired_at IS NOT NULL AND id NOT IN (SELECT key_id FROM ci_secrets)", [])?;
            tx.commit()?;
            Ok(rows.len())
        })
        .await
}

// ------------------------------------------------------------------ variables

#[derive(Debug, Clone, serde::Serialize)]
pub struct Variable {
    pub name: String,
    pub value: String,
    pub updated_at: i64,
}

pub async fn set_var(hub: &Hub, scope: &Scope, name: &str, value: &str, by: &str) -> Result<(), String> {
    let name = rules::check_name(name)?;
    if value.len() > rules::MAX_VARIABLE {
        return Err(format!("a variable is at most {} KiB", rules::MAX_VARIABLE >> 10));
    }
    let (s, value, by) = (scope.clone(), value.to_owned(), by.to_owned());
    hub.store
        .call(move |c| {
            let n: i64 = c.query_row("SELECT COUNT(*) FROM ci_variables WHERE scope_kind = ?1 AND scope_id = ?2 AND name != ?3", params![s.kind.as_str(), s.id, name], |r| r.get(0))?;
            if usize::try_from(n).unwrap_or(usize::MAX) >= rules::MAX_PER_SCOPE {
                anyhow::bail!("a scope holds at most {} variables", rules::MAX_PER_SCOPE);
            }
            c.execute(
                "INSERT INTO ci_variables (scope_kind, scope_id, name, value, updated_by, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (scope_kind, scope_id, name) DO UPDATE SET value = excluded.value, updated_by = excluded.updated_by, updated_at = excluded.updated_at",
                params![s.kind.as_str(), s.id, name, value, by, crate::util::now_unix()],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| format!("{e:#}"))
}

pub async fn delete_var(hub: &Hub, scope: &Scope, name: &str) -> anyhow::Result<bool> {
    let (s, name) = (scope.clone(), name.to_ascii_uppercase());
    hub.store.call(move |c| Ok(c.execute("DELETE FROM ci_variables WHERE scope_kind = ?1 AND scope_id = ?2 AND name = ?3", params![s.kind.as_str(), s.id, name])? > 0)).await
}

pub async fn list_vars(hub: &Hub, scope: &Scope) -> anyhow::Result<Vec<Variable>> {
    let s = scope.clone();
    hub.store
        .call(move |c| {
            let mut q = c.prepare("SELECT name, value, updated_at FROM ci_variables WHERE scope_kind = ?1 AND scope_id = ?2 ORDER BY name")?;
            let rows = q.query_map(params![s.kind.as_str(), s.id], |r| Ok(Variable { name: r.get(0)?, value: r.get(1)?, updated_at: r.get(2)? }))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

/// The `vars` context of `repo`'s `workflow` bound to `environment`: every scope's, the most
/// specific winning (W09-R09).
pub async fn vars_for(hub: &Hub, repo: &Repo, workflow: &str, environment: Option<&str>) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let scopes = scopes_for(repo, workflow, environment);
    hub.store
        .call(move |c| {
            let mut out = serde_json::Map::new();
            for s in scopes.iter().rev() {
                let mut q = c.prepare("SELECT name, value FROM ci_variables WHERE scope_kind = ?1 AND scope_id = ?2")?;
                let rows = q.query_map(params![s.kind.as_str(), s.id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
                for row in rows {
                    let (n, v) = row?;
                    out.insert(n, serde_json::Value::String(v));
                }
            }
            Ok(out)
        })
        .await
}

// ------------------------------------------------------------------ HTTP (W09-R11)

#[derive(serde::Deserialize)]
struct Value {
    value: String,
}

/// A secret as set: its `value`, or where it comes `from` (W09-R08).
#[derive(serde::Deserialize)]
struct SecretBody {
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    from: Option<String>,
}

fn fail(status: http::StatusCode, m: impl Into<String>) -> super::Fail {
    (status, m.into())
}

async fn scope_of(hub: &Hub, q: &std::collections::HashMap<String, String>) -> Result<(Scope, String), super::Fail> {
    let text = q.get("scope").cloned().unwrap_or_else(|| "server".into());
    let scope = parse_scope(hub, &text).await.map_err(|e| fail(http::StatusCode::BAD_REQUEST, e))?;
    Ok((scope, text))
}

/// `/api/ci/secrets…` and `/api/ci/variables…`, behind the console's auth. Reads never carry a
/// secret's value.
pub async fn route(hub: &Hub, method: &http::Method, rest: &[&str], q: &std::collections::HashMap<String, String>, body: kiln_http::Incoming) -> Result<crate::http::server::Response, super::Fail> {
    use crate::http::server as srv;
    use http::{Method, StatusCode};
    let internal = |e: anyhow::Error| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    match (method, rest) {
        (&Method::GET, ["secrets"]) => {
            let (scope, text) = scope_of(hub, q).await?;
            let list = list(hub, &scope).await.map_err(internal)?;
            Ok(srv::json(StatusCode::OK, &serde_json::json!({ "scope": text, "secrets": list })))
        }
        (&Method::POST, ["secrets", "rotate"]) => {
            let n = rotate(hub).await.map_err(internal)?;
            hub.store.audit("admin".into(), "ci.secrets.rotate".into(), format!("re-sealed {n} secrets under a new data key")).await;
            Ok(srv::json(StatusCode::OK, &serde_json::json!({ "resealed": n })))
        }
        (&Method::PUT, ["secrets", name]) => {
            let (scope, text) = scope_of(hub, q).await?;
            let v: SecretBody = srv::body_json(body, rules::MAX_SECRET * 2 + 1024).await.map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            let detail = match (&v.value, &v.from) {
                (Some(value), None) => {
                    set(hub, &scope, name, value, "admin").await.map_err(|e| fail(StatusCode::UNPROCESSABLE_ENTITY, e))?;
                    format!("{} in {text}", name.to_ascii_uppercase())
                }
                // W09-R08: brokered, its value made at leg start.
                (None, Some(from)) => {
                    set_brokered(hub, &scope, name, from, "admin").await.map_err(|e| fail(StatusCode::UNPROCESSABLE_ENTITY, e))?;
                    format!("{} in {text}, from {}", name.to_ascii_uppercase(), from.trim())
                }
                _ => return Err(fail(StatusCode::BAD_REQUEST, "a secret has a `value`, or says where it comes `from`")),
            };
            hub.store.audit("admin".into(), "ci.secret.set".into(), detail).await;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::DELETE, ["secrets", name]) => {
            let (scope, text) = scope_of(hub, q).await?;
            if !delete(hub, &scope, name).await.map_err(internal)? {
                return Err(fail(StatusCode::NOT_FOUND, format!("no secret {} in {text}", name.to_ascii_uppercase())));
            }
            hub.store.audit("admin".into(), "ci.secret.delete".into(), format!("{} in {text}", name.to_ascii_uppercase())).await;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::GET, ["variables"]) => {
            let (scope, text) = scope_of(hub, q).await?;
            let list = list_vars(hub, &scope).await.map_err(internal)?;
            Ok(srv::json(StatusCode::OK, &serde_json::json!({ "scope": text, "variables": list })))
        }
        (&Method::PUT, ["variables", name]) => {
            let (scope, text) = scope_of(hub, q).await?;
            let v: Value = srv::body_json(body, rules::MAX_VARIABLE * 2 + 1024).await.map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            set_var(hub, &scope, name, &v.value, "admin").await.map_err(|e| fail(StatusCode::UNPROCESSABLE_ENTITY, e))?;
            hub.store.audit("admin".into(), "ci.variable.set".into(), format!("{} in {text}", name.to_ascii_uppercase())).await;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::DELETE, ["variables", name]) => {
            let (scope, text) = scope_of(hub, q).await?;
            if !delete_var(hub, &scope, name).await.map_err(internal)? {
                return Err(fail(StatusCode::NOT_FOUND, format!("no variable {} in {text}", name.to_ascii_uppercase())));
            }
            hub.store.audit("admin".into(), "ci.variable.delete".into(), format!("{} in {text}", name.to_ascii_uppercase())).await;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        _ => Err(fail(StatusCode::NOT_FOUND, "no such route")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w09_r03_sealed_values_open_only_where_they_were_sealed() -> anyhow::Result<()> {
        let k: [u8; 32] = random()?;
        let s = Scope { kind: ScopeKind::Repository, id: "r1".into() };
        let (sealed, nonce) = seal(&k, b"hunter2", &s.aad("PASS"))?;
        assert!(!sealed.windows(7).any(|w| w == b"hunter2"));
        assert_eq!(open(&k, &sealed, &nonce, &s.aad("PASS"))?, b"hunter2");
        // Another name, another scope, another key, or a flipped bit: refused.
        assert!(open(&k, &sealed, &nonce, &s.aad("OTHER")).is_err());
        assert!(open(&k, &sealed, &nonce, &Scope { kind: ScopeKind::Server, id: String::new() }.aad("PASS")).is_err());
        let other: [u8; 32] = random()?;
        assert!(open(&other, &sealed, &nonce, &s.aad("PASS")).is_err());
        let mut bad = sealed.clone();
        if let Some(b) = bad.first_mut() {
            *b ^= 1;
        }
        assert!(open(&k, &bad, &nonce, &s.aad("PASS")).is_err());
        // Fresh nonces: the same value seals differently each time.
        let (again, n2) = seal(&k, b"hunter2", &s.aad("PASS"))?;
        assert!(again != sealed && n2 != nonce);
        Ok(())
    }
}
