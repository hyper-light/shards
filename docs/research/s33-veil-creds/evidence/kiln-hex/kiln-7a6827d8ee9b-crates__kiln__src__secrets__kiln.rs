//! kiln's own secrets backend: versions in kiln's store, each value sealed with AES-256-GCM under
//! a data key of this backend's own, the data keys wrapped by the server's master key (the one
//! the secret store keeps, docs/ci.md W09). Each ciphertext is bound to its scope, name and
//! version, so none can be moved to another row. The last [`KEPT_VERSIONS`] versions of a secret
//! keep their values; older ones keep only their record. Each read for a delivery is recorded.

use std::collections::HashMap;
use std::sync::Arc;

use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use rusqlite::{Connection, OptionalExtension, params};

use super::{Backend, Bundle, Fut, KEPT_VERSIONS, Meta, Read, Scope, Use, Value, Version};
use crate::hub::Hub;

/// How many reads of a scope are kept.
const KEPT_READS: i64 = 1000;

pub struct KilnBackend {
    hub: Arc<Hub>,
}

impl KilnBackend {
    pub fn new(hub: Arc<Hub>) -> KilnBackend {
        KilnBackend { hub }
    }
}

fn random<const N: usize>() -> anyhow::Result<[u8; N]> {
    let mut b = [0u8; N];
    SystemRandom::new().fill(&mut b).map_err(|_| anyhow::anyhow!("the system's random source failed"))?;
    Ok(b)
}

fn aead(k: &[u8; 32]) -> anyhow::Result<LessSafeKey> {
    Ok(LessSafeKey::new(UnboundKey::new(&AES_256_GCM, k).map_err(|_| anyhow::anyhow!("a bad data key"))?))
}

/// What binds a value's ciphertext to its row.
fn aad(scope: &str, name: &str, version: u32) -> Vec<u8> {
    format!("kiln secret\0{scope}\0{name}\0{version}").into_bytes()
}

fn key_aad(id: &str) -> Vec<u8> {
    format!("kiln secret key\0{id}").into_bytes()
}

fn seal(k: &[u8; 32], plain: &[u8], aad: &[u8]) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let nonce: [u8; NONCE_LEN] = random()?;
    let mut buf = plain.to_vec();
    aead(k)?.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(aad), &mut buf).map_err(|_| anyhow::anyhow!("sealing failed"))?;
    Ok((buf, nonce.to_vec()))
}

fn open(k: &[u8; 32], sealed: &[u8], nonce: &[u8], aad: &[u8]) -> anyhow::Result<Value> {
    let n: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| anyhow::anyhow!("a bad nonce"))?;
    let mut buf = sealed.to_vec();
    let len = aead(k)?.open_in_place(Nonce::assume_unique_for_key(n), Aad::from(aad), &mut buf).map_err(|_| anyhow::anyhow!("a secret doesn't decrypt: the master key or the store changed"))?.len();
    buf.truncate(len);
    Ok(Value::new(buf))
}

/// Every data key, unwrapped, by id.
fn keys(hub: &Hub, c: &Connection) -> anyhow::Result<HashMap<String, [u8; 32]>> {
    let mut q = c.prepare("SELECT id, wrapped, nonce FROM secret_keys")?;
    let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, Vec<u8>>(2)?)))?.collect::<Result<Vec<_>, _>>()?;
    let mut out = HashMap::new();
    for (id, wrapped, nonce) in rows {
        let plain = crate::ci::secrets::open_master(hub, &wrapped, &nonce, &key_aad(&id))?;
        let k: [u8; 32] = plain.as_slice().try_into().map_err(|_| anyhow::anyhow!("data key {id} isn't 32 bytes"))?;
        out.insert(id, k);
    }
    Ok(out)
}

/// A new data key, kept wrapped.
fn new_key(hub: &Hub, c: &Connection) -> anyhow::Result<(String, [u8; 32])> {
    let (id, k) = (crate::util::new_id(), random::<32>()?);
    let (wrapped, nonce) = crate::ci::secrets::seal_master(hub, &k, &key_aad(&id))?;
    c.execute("INSERT INTO secret_keys (id, wrapped, nonce, created_at) VALUES (?1, ?2, ?3, ?4)", params![id, wrapped, nonce, crate::util::now_unix()])?;
    Ok((id, k))
}

/// The data key new values are sealed under: the newest not retired, made at first use.
fn current_key(hub: &Hub, c: &Connection) -> anyhow::Result<(String, [u8; 32])> {
    let row: Option<(String, Vec<u8>, Vec<u8>)> =
        c.query_row("SELECT id, wrapped, nonce FROM secret_keys WHERE retired_at IS NULL ORDER BY created_at DESC, id DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
    match row {
        Some((id, wrapped, nonce)) => {
            let plain = crate::ci::secrets::open_master(hub, &wrapped, &nonce, &key_aad(&id))?;
            let k: [u8; 32] = plain.as_slice().try_into().map_err(|_| anyhow::anyhow!("data key {id} isn't 32 bytes"))?;
            Ok((id, k))
        }
        None => new_key(hub, c),
    }
}

/// Each secret's newest version in a scope: (name, version, size, set_by, set_at, deleted).
fn newest(c: &Connection, scope: &str) -> anyhow::Result<Vec<(String, u32, usize, String, i64, bool)>> {
    let mut q = c.prepare(
        "SELECT name, version, size, set_by, set_at, deleted FROM secret_versions v
         WHERE scope = ?1 AND version = (SELECT MAX(version) FROM secret_versions w WHERE w.scope = v.scope AND w.name = v.name) ORDER BY name",
    )?;
    let rows = q
        .query_map([scope], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, r.get::<_, i64>(4)?, r.get::<_, bool>(5)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows.into_iter().map(|(n, v, size, by, at, del)| (n, v, usize::try_from(size).unwrap_or(0), by, at, del)).collect())
}

impl Backend for KilnBackend {
    fn name(&self) -> &'static str {
        "kiln"
    }

    fn list<'a>(&'a self, at: &'a Scope) -> Fut<'a, Vec<Meta>> {
        let scope = at.key();
        Box::pin(async move {
            let rows = self.hub.store.call(move |c| newest(c, &scope)).await?;
            Ok(rows.into_iter().filter(|r| !r.5).map(|(name, version, size, set_by, set_at, _)| Meta { name, version, size, set_by, set_at }).collect())
        })
    }

    fn versions<'a>(&'a self, at: &'a Scope, name: &'a str) -> Fut<'a, Vec<Version>> {
        let (scope, name) = (at.key(), name.to_owned());
        Box::pin(async move {
            self.hub
                .store
                .call(move |c| {
                    let mut q = c.prepare("SELECT version, set_by, set_at, deleted, ciphertext IS NULL FROM secret_versions WHERE scope = ?1 AND name = ?2 ORDER BY version DESC")?;
                    let rows = q
                        .query_map(params![scope, name], |r| Ok(Version { version: r.get(0)?, set_by: r.get(1)?, set_at: r.get(2)?, deleted: r.get(3)?, destroyed: r.get(4)? }))?
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(rows)
                })
                .await
        })
    }

    fn put<'a>(&'a self, at: &'a Scope, name: &'a str, value: Value, by: &'a str) -> Fut<'a, Meta> {
        let (scope, name, by, hub) = (at.key(), name.to_owned(), by.to_owned(), Arc::clone(&self.hub));
        Box::pin(async move {
            self.hub
                .store
                .call(move |c| {
                    let tx = crate::store::tx(c)?;
                    let last: Option<u32> = tx.query_row("SELECT MAX(version) FROM secret_versions WHERE scope = ?1 AND name = ?2", params![scope, name], |r| r.get(0))?;
                    let version = last.unwrap_or(0) + 1;
                    let (key_id, k) = current_key(&hub, &tx)?;
                    let (sealed, nonce) = seal(&k, value.bytes(), &aad(&scope, &name, version))?;
                    let (size, now) = (value.len(), crate::util::now_unix());
                    tx.execute(
                        "INSERT INTO secret_versions (scope, name, version, ciphertext, nonce, key_id, size, set_by, set_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![scope, name, version, sealed, nonce, key_id, i64::try_from(size).unwrap_or(0), by, now],
                    )?;
                    // Older versions than those kept keep their record, not their value.
                    tx.execute(
                        "UPDATE secret_versions SET ciphertext = NULL, nonce = NULL, key_id = NULL WHERE scope = ?1 AND name = ?2 AND version <= ?3 AND ciphertext IS NOT NULL",
                        params![scope, name, version.saturating_sub(KEPT_VERSIONS)],
                    )?;
                    tx.commit()?;
                    Ok(Meta { name, version, size, set_by: by, set_at: now })
                })
                .await
        })
    }

    fn delete<'a>(&'a self, at: &'a Scope, name: &'a str, by: &'a str) -> Fut<'a, bool> {
        let (scope, name, by) = (at.key(), name.to_owned(), by.to_owned());
        Box::pin(async move {
            self.hub
                .store
                .call(move |c| {
                    let tx = crate::store::tx(c)?;
                    let last: Option<(u32, bool)> = tx
                        .query_row("SELECT version, deleted FROM secret_versions WHERE scope = ?1 AND name = ?2 ORDER BY version DESC LIMIT 1", params![scope, name], |r| Ok((r.get(0)?, r.get(1)?)))
                        .optional()?;
                    let Some((version, false)) = last else { return Ok(false) };
                    tx.execute(
                        "INSERT INTO secret_versions (scope, name, version, size, set_by, set_at, deleted) VALUES (?1, ?2, ?3, 0, ?4, ?5, 1)",
                        params![scope, name, version + 1, by, crate::util::now_unix()],
                    )?;
                    tx.commit()?;
                    Ok(true)
                })
                .await
        })
    }

    fn resolve<'a>(&'a self, at: &'a Scope, want: &'a [(String, u32)], why: &'a Use) -> Fut<'a, Bundle> {
        let (scope, hub, by, purpose, want) = (at.key(), Arc::clone(&self.hub), why.by.clone(), why.purpose.clone(), want.to_vec());
        Box::pin(async move {
            self.hub
                .store
                .call(move |c| {
                    let keys = keys(&hub, c)?;
                    let mut items = Vec::with_capacity(want.len());
                    for (name, version) in want {
                        let row: Option<(Option<Vec<u8>>, Option<Vec<u8>>, Option<String>)> = c
                            .query_row("SELECT ciphertext, nonce, key_id FROM secret_versions WHERE scope = ?1 AND name = ?2 AND version = ?3", params![scope, name, version], |r| {
                                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                            })
                            .optional()?;
                        let Some(row) = row else { anyhow::bail!("secret {name} has no version {version}") };
                        let (Some(sealed), Some(nonce), Some(key_id)) = row else {
                            anyhow::bail!("secret {name} v{version}'s value is no longer kept (only the last {KEPT_VERSIONS} are): release again, to deliver its current version")
                        };
                        let k = keys.get(&key_id).ok_or_else(|| anyhow::anyhow!("secret {name}: its data key is gone"))?;
                        items.push((name.clone(), version, open(k, &sealed, &nonce, &aad(&scope, &name, version))?));
                    }
                    let bundle = Bundle { items, expires_at: None };
                    c.execute("INSERT INTO secret_reads (scope, by_whom, purpose, versions, at) VALUES (?1, ?2, ?3, ?4, ?5)", params![scope, by, purpose, bundle.versions(), crate::util::now_unix()])?;
                    c.execute(
                        "DELETE FROM secret_reads WHERE scope = ?1 AND id <= (SELECT id FROM secret_reads WHERE scope = ?1 ORDER BY id DESC LIMIT 1 OFFSET ?2)",
                        params![scope, KEPT_READS],
                    )?;
                    Ok(bundle)
                })
                .await
        })
    }

    fn reads<'a>(&'a self, at: &'a Scope, limit: usize) -> Fut<'a, Vec<Read>> {
        let (scope, limit) = (at.key(), i64::try_from(limit).unwrap_or(100));
        Box::pin(async move {
            self.hub
                .store
                .call(move |c| {
                    let mut q = c.prepare("SELECT by_whom, purpose, versions, at FROM secret_reads WHERE scope = ?1 ORDER BY id DESC LIMIT +?2")?;
                    let rows = q.query_map(params![scope, limit], |r| Ok(Read { by: r.get(0)?, purpose: r.get(1)?, versions: r.get(2)?, at: r.get(3)? }))?.collect::<Result<Vec<_>, _>>()?;
                    Ok(rows)
                })
                .await
        })
    }

    fn missing<'a>(&'a self, at: &'a Scope, want: &'a [(String, u32)]) -> Fut<'a, Vec<(String, u32)>> {
        let (scope, want) = (at.key(), want.to_vec());
        Box::pin(async move {
            self.hub
                .store
                .call(move |c| {
                    let mut out = Vec::new();
                    for (name, version) in want {
                        let kept: Option<bool> = c
                            .query_row("SELECT ciphertext IS NOT NULL FROM secret_versions WHERE scope = ?1 AND name = ?2 AND version = ?3", params![scope, name, version], |r| r.get(0))
                            .optional()?;
                        if kept != Some(true) {
                            out.push((name, version));
                        }
                    }
                    Ok(out)
                })
                .await
        })
    }

    fn rotate(&self) -> Fut<'_, usize> {
        let hub = Arc::clone(&self.hub);
        Box::pin(async move {
            self.hub
                .store
                .call(move |c| {
                    let tx = crate::store::tx(c)?;
                    let old = keys(&hub, &tx)?;
                    let (new_id, fresh) = new_key(&hub, &tx)?;
                    let rows: Vec<(String, String, u32, Vec<u8>, Vec<u8>, String)> = {
                        let mut q = tx.prepare("SELECT scope, name, version, ciphertext, nonce, key_id FROM secret_versions WHERE ciphertext IS NOT NULL")?;
                        let r = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?;
                        r.collect::<Result<Vec<_>, _>>()?
                    };
                    for (scope, name, version, sealed, nonce, key_id) in &rows {
                        let k = old.get(key_id).ok_or_else(|| anyhow::anyhow!("secret {name}: its data key is gone"))?;
                        let ad = aad(scope, name, *version);
                        let plain = open(k, sealed, nonce, &ad)?;
                        let (resealed, n) = seal(&fresh, plain.bytes(), &ad)?;
                        tx.execute("UPDATE secret_versions SET ciphertext = ?4, nonce = ?5, key_id = ?6 WHERE scope = ?1 AND name = ?2 AND version = ?3", params![scope, name, version, resealed, n, new_id])?;
                    }
                    tx.execute("UPDATE secret_keys SET retired_at = ?2 WHERE id != ?1 AND retired_at IS NULL", params![new_id, crate::util::now_unix()])?;
                    tx.execute("DELETE FROM secret_keys WHERE retired_at IS NOT NULL AND id NOT IN (SELECT key_id FROM secret_versions WHERE key_id IS NOT NULL)", [])?;
                    tx.commit()?;
                    Ok(rows.len())
                })
                .await
        })
    }

    fn forget<'a>(&'a self, at: &'a Scope, by: &'a str) -> Fut<'a, usize> {
        let (scope, by) = (at.key(), by.to_owned());
        Box::pin(async move {
            self.hub
                .store
                .call(move |c| {
                    let tx = crate::store::tx(c)?;
                    let live: Vec<(String, u32)> = newest(&tx, &scope)?.into_iter().filter(|r| !r.5).map(|r| (r.0, r.1)).collect();
                    let now = crate::util::now_unix();
                    for (name, version) in &live {
                        tx.execute(
                            "INSERT INTO secret_versions (scope, name, version, size, set_by, set_at, deleted) VALUES (?1, ?2, ?3, 0, ?4, ?5, 1)",
                            params![scope, name, version + 1, by, now],
                        )?;
                    }
                    tx.execute("UPDATE secret_versions SET ciphertext = NULL, nonce = NULL, key_id = NULL WHERE scope = ?1 AND ciphertext IS NOT NULL", params![scope])?;
                    tx.commit()?;
                    Ok(live.len())
                })
                .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::{self, Caller, Refused};

    fn ada() -> Caller {
        Caller { name: "ada".into(), agent: false }
    }

    fn value(s: &str) -> Value {
        Value::new(s.as_bytes().to_vec())
    }

    /// The values a delivery gets, as text.
    fn texts(b: &Bundle) -> Vec<(String, u32, String)> {
        b.items.iter().map(|(n, v, x)| (n.clone(), *v, String::from_utf8_lossy(x.bytes()).into_owned())).collect()
    }

    #[test]
    fn versions_sealed_and_kept_ten_deliveries_recorded_rotated_and_forgotten() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let s = secrets::of(&hub);
            let (web, prod) = (Scope::of("web", "staging"), Scope::of("web", "production"));
            for n in 1..=12u32 {
                let m = s.put(&web, "DB_PASSWORD", value(&format!("pw-{n}")), &ada()).await?;
                anyhow::ensure!(m.version == n && m.set_by == "ada", "{m:?}");
            }
            s.put(&web, "API_KEY", value("api-key-one-7f3c"), &ada()).await?;
            let list = s.list(&web, &ada()).await?;
            let names: Vec<(&str, u32, usize)> = list.iter().map(|m| (m.name.as_str(), m.version, m.size)).collect();
            anyhow::ensure!(names == [("API_KEY", 1, 16), ("DB_PASSWORD", 12, 5)], "{names:?}");
            anyhow::ensure!(s.list(&prod, &ada()).await?.is_empty(), "a scope holds only its own");

            // The last ten keep their values; older ones only their record.
            let versions = s.versions(&web, "DB_PASSWORD", &ada()).await?;
            let destroyed: Vec<u32> = versions.iter().filter(|v| v.destroyed).map(|v| v.version).collect();
            anyhow::ensure!(versions.first().map(|v| v.version) == Some(12) && versions.len() == 12 && destroyed == [2, 1], "{versions:?}");

            // Sealed: no value in the store as it is, and none opens in another row.
            let rows: Vec<Vec<u8>> = hub
                .store
                .call(|c| {
                    let mut q = c.prepare("SELECT ciphertext FROM secret_versions WHERE ciphertext IS NOT NULL")?;
                    let r = q.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
                    Ok(r)
                })
                .await?;
            // Each value is long enough that no ciphertext holds it by chance.
            anyhow::ensure!(rows.len() == 11 && rows.iter().all(|c| !c.windows(5).any(|w| w == b"pw-12") && !c.windows(16).any(|w| w == b"api-key-one-7f3c")), "values are sealed");
            hub.store
                .call(|c| {
                    c.execute(
                        "INSERT INTO secret_versions (scope, name, version, ciphertext, nonce, key_id, size, set_by, set_at)
                         SELECT 'app/web/production', name, version, ciphertext, nonce, key_id, size, set_by, set_at FROM secret_versions WHERE scope = 'app/web/staging' AND name = 'API_KEY'",
                        [],
                    )?;
                    Ok(())
                })
                .await?;
            let why = Use { by: "kilncd".into(), purpose: "render web-production".into() };
            anyhow::ensure!(s.resolve(&prod, &[("API_KEY".to_owned(), 1)], &why).await.is_err(), "a value moved to another scope doesn't open");
            hub.store.call(|c| Ok(c.execute("DELETE FROM secret_versions WHERE scope = 'app/web/production'", [])?)).await?;

            // A delivery gets the values of the versions it pins, and the read is recorded.
            let why = Use { by: "kilncd".into(), purpose: "render web-staging".into() };
            let now = s.current(&web).await?;
            anyhow::ensure!(now == [("API_KEY".to_owned(), 1), ("DB_PASSWORD".to_owned(), 12)], "{now:?}");
            let b = s.resolve(&web, &now, &why).await?;
            anyhow::ensure!(texts(&b) == [("API_KEY".into(), 1, "api-key-one-7f3c".into()), ("DB_PASSWORD".into(), 12, "pw-12".into())], "{:?}", b.versions());
            let reads = s.reads(&web, 10, &ada()).await?;
            anyhow::ensure!(reads.len() == 1 && reads.first().is_some_and(|r| r.by == "kilncd" && r.versions == "API_KEY@1,DB_PASSWORD@12"), "{reads:?}");

            // Rotation re-seals every value kept under a new data key; the old one goes.
            anyhow::ensure!(s.rotate(&ada()).await? == 11, "the eleven values kept");
            let keys: i64 = hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM secret_keys", [], |r| r.get(0))?)).await?;
            anyhow::ensure!(keys == 1, "{keys} data keys after a rotation");
            anyhow::ensure!(texts(&s.resolve(&web, &now, &why).await?) == texts(&b), "the same values after a rotation");
            // An older version kept opens; one no longer kept, or never set, is refused.
            anyhow::ensure!(texts(&s.resolve(&web, &[("DB_PASSWORD".to_owned(), 3)], &why).await?) == [("DB_PASSWORD".into(), 3, "pw-3".into())]);
            anyhow::ensure!(s.resolve(&web, &[("DB_PASSWORD".to_owned(), 2)], &why).await.is_err_and(|e| e.to_string().contains("no longer kept")));
            anyhow::ensure!(s.resolve(&web, &[("DB_PASSWORD".to_owned(), 99)], &why).await.is_err());
            // A preflight asks which have no value, reading none.
            let reads_before = s.reads(&web, 1000, &ada()).await?.len();
            let pins = [("DB_PASSWORD".to_owned(), 3), ("DB_PASSWORD".to_owned(), 2), ("API_KEY".to_owned(), 1), ("NEVER".to_owned(), 1)];
            anyhow::ensure!(s.missing(&web, &pins).await? == [("DB_PASSWORD".to_owned(), 2), ("NEVER".to_owned(), 1)]);
            anyhow::ensure!(s.reads(&web, 1000, &ada()).await?.len() == reads_before, "nothing read");

            // Agents look; people change. Names are environment variables'; values aren't empty.
            let agent = Caller { name: "claude".into(), agent: true };
            anyhow::ensure!(matches!(s.put(&web, "X", value("v"), &agent).await, Err(Refused::Forbidden(_))));
            anyhow::ensure!(matches!(s.delete(&web, "API_KEY", &agent).await, Err(Refused::Forbidden(_))));
            anyhow::ensure!(matches!(s.rotate(&agent).await, Err(Refused::Forbidden(_))));
            anyhow::ensure!(s.list(&web, &agent).await?.len() == 2, "agents list");
            anyhow::ensure!(matches!(s.put(&web, "db-password", value("v"), &ada()).await, Err(Refused::Bad(_))));
            anyhow::ensure!(matches!(s.put(&web, "EMPTY", value(""), &ada()).await, Err(Refused::Bad(_))));
            anyhow::ensure!(matches!(s.put(&web, "BIG", Value::new(vec![b'x'; secrets::MAX_VALUE + 1]), &ada()).await, Err(Refused::Bad(_))));
            // A scope's values together fit one Kubernetes Secret.
            let full = Scope::of("big", "staging");
            for n in 0..secrets::MAX_SCOPE_BYTES / secrets::MAX_VALUE {
                s.put(&full, &format!("PART_{n}"), Value::new(vec![b'x'; secrets::MAX_VALUE]), &ada()).await?;
            }
            anyhow::ensure!(matches!(s.put(&full, "ONE_MORE", value("x"), &ada()).await, Err(Refused::Bad(m)) if m.contains("1 MiB")));
            anyhow::ensure!(s.put(&full, "PART_0", value("smaller"), &ada()).await.is_ok(), "a secret replaced counts once");

            // A deletion is a version of its own; set again, the secret goes on from it.
            s.delete(&web, "DB_PASSWORD", &ada()).await?;
            anyhow::ensure!(matches!(s.delete(&web, "DB_PASSWORD", &ada()).await, Err(Refused::Missing(_))));
            anyhow::ensure!(s.current(&web).await? == [("API_KEY".to_owned(), 1)], "a deleted secret isn't pinned");
            let v = s.versions(&web, "DB_PASSWORD", &ada()).await?;
            anyhow::ensure!(v.first().is_some_and(|v| v.version == 13 && v.deleted && v.destroyed), "{v:?}");
            anyhow::ensure!(s.put(&web, "DB_PASSWORD", value("pw-again"), &ada()).await?.version == 14);
            anyhow::ensure!(matches!(s.versions(&web, "NEVER_SET", &ada()).await, Err(Refused::Missing(_))));

            // Forgotten (its app deleted): nothing is listed or delivered, and no value is kept.
            anyhow::ensure!(s.forget(&web, "ada").await? == 2);
            anyhow::ensure!(s.list(&web, &ada()).await?.is_empty() && s.resolve(&web, &[("API_KEY".to_owned(), 1)], &why).await.is_err());
            let kept: i64 = hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM secret_versions WHERE scope = 'app/web/staging' AND ciphertext IS NOT NULL", [], |r| r.get(0))?)).await?;
            anyhow::ensure!(kept == 0, "{kept} values kept after forgetting");
            Ok(())
        })
    }
}
