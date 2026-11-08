//! The trusted metadata set as go-tuf v2 keeps it (trustedmetadata): a root, then a
//! timestamp, a snapshot and targets, each accepted only as the specification's client
//! workflow allows, and each delegation verified (Metadata.VerifyDelegate).

use crate::Error;
use crate::keys::{self, Hash};
use crate::metadata::{Body, Key, Metadata, ROOT, SNAPSHOT, TARGETS, TIMESTAMP};

static EMPTY: crate::metadata::Named<Key> = Vec::new();

/// VerifyDelegate of `delegated`, as role `name`, by `delegator`: at least the role's
/// threshold of its keys, counted once per distinct key, sign `delegated`'s canonical
/// form. A key that cannot be read fails it outright, as go-tuf fails.
pub fn verify_delegate(delegator: &Metadata, name: &str, delegated: &Metadata) -> Result<(), Error> {
    let none = || Error::Value(format!("no delegation found for {name}"));
    let (keys, keyids, threshold): (&crate::metadata::Named<Key>, Vec<String>, i64) =
        match &delegator.signed.body {
            Body::Root { keys, roles, .. } => {
                let role = roles
                    .as_ref()
                    .and_then(|r| r.iter().rev().find(|(k, _)| k == name))
                    .ok_or_else(none)?;
                let (ids, t) = role.1.as_ref().map_or((Vec::new(), 0), |r| {
                    (r.keyids.clone().unwrap_or_default(), r.threshold)
                });
                (keys.as_ref().unwrap_or(&EMPTY), ids, t)
            }
            Body::Targets { delegations, .. } => {
                let d = delegations
                    .as_ref()
                    .ok_or_else(|| Error::Value("no delegations found".into()))?;
                let (ids, t) = if let Some(roles) = &d.roles {
                    let r = roles.iter().find(|r| r.name == name).ok_or_else(none)?;
                    (r.keyids.clone().unwrap_or_default(), r.threshold)
                } else if let Some(s) = &d.succinct {
                    (s.keyids.clone().unwrap_or_default(), s.threshold)
                } else {
                    (Vec::new(), 0)
                };
                (d.keys.as_ref().unwrap_or(&EMPTY), ids, t)
            }
            Body::Meta(_) => {
                return Err(Error::Type(
                    "call is valid only on delegator metadata (should be either root or targets)".into(),
                ));
            }
        };
    if keyids.is_empty() {
        return Err(none());
    }
    if threshold < 1 {
        return Err(Error::Value(format!(
            "insufficient threshold ({threshold}) configured for {name}"
        )));
    }
    // What the keys sign, made once, where go-tuf makes it: after each key is read.
    let mut payload: Option<Vec<u8>> = None;
    let mut signing: Vec<Vec<u8>> = Vec::new();
    for id in &keyids {
        let key = keys
            .iter()
            .rev()
            .find(|(k, _)| k == id)
            .ok_or_else(|| Error::Value(format!("key with ID {id} not found in {name} keyids")))?;
        // A null key: go-tuf would dereference it and panic; here, no key.
        let key = key
            .1
            .as_ref()
            .ok_or_else(|| Error::Key(format!("key with ID {id} is null")))?;
        let public = keys::to_public_key(key)?;
        let hash = match key.scheme.as_str() {
            "ecdsa-sha2-nistp384" if key.keytype != "ed25519" => Hash::Sha384,
            _ => Hash::Sha256,
        };
        let payload = match &mut payload {
            Some(p) => p,
            None => payload.insert(delegated.signed.canonical()?),
        };
        // The signature by this key ID: the last, as go-tuf's loop leaves it.
        let sig = delegated
            .signatures
            .iter()
            .rev()
            .find(|s| s.keyid == *id)
            .map(|s| s.sig.as_slice())
            .unwrap_or_default();
        if public.verify(hash, payload, sig) {
            let fp = public.fingerprint();
            if !signing.contains(&fp) {
                signing.push(fp);
            }
        }
    }
    if (signing.len() as i64) < threshold {
        return Err(Error::Unsigned(format!(
            "Verifying {name} failed, not enough signatures, got {}, want {threshold}",
            signing.len()
        )));
    }
    Ok(())
}

/// The trusted set: what has been accepted so far, and the time it is judged at.
#[derive(Debug, Clone)]
pub struct Trusted {
    pub root: Metadata,
    pub timestamp: Option<Metadata>,
    pub snapshot: Option<Metadata>,
    /// Targets roles by name, in the order loaded.
    pub targets: Vec<(String, Metadata)>,
    /// RefTime, as seconds and nanoseconds since 1970.
    pub now: (i64, u32),
}

fn check_kind(m: &Metadata, kind: &str) -> Result<(), Error> {
    if m.signed.kind != kind {
        return Err(Error::Repository(format!(
            "expected {kind}, got {}",
            m.signed.kind
        )));
    }
    Ok(())
}

impl Trusted {
    /// New: a root, trusted as given once it signs itself.
    pub fn new(root: &[u8], now: (i64, u32)) -> Result<Trusted, Error> {
        let root = Metadata::from_bytes(ROOT, root)?;
        check_kind(&root, ROOT)?;
        verify_delegate(&root, ROOT, &root)?;
        Ok(Trusted {
            root,
            timestamp: None,
            snapshot: None,
            targets: Vec::new(),
            now,
        })
    }

    pub fn targets_of(&self, name: &str) -> Option<&Metadata> {
        self.targets.iter().find(|(n, _)| n == name).map(|(_, m)| m)
    }

    /// UpdateRoot: the next version, signed by the current root's keys and its own.
    pub fn update_root(&mut self, data: &[u8]) -> Result<(), Error> {
        if self.timestamp.is_some() {
            return Err(Error::Runtime("cannot update root after timestamp".into()));
        }
        let new = Metadata::from_bytes(ROOT, data)?;
        check_kind(&new, ROOT)?;
        verify_delegate(&self.root, ROOT, &new)?;
        if new.signed.version != self.root.signed.version + 1 {
            return Err(Error::BadVersion(format!(
                "bad version number, expected {}, got {}",
                self.root.signed.version + 1,
                new.signed.version
            )));
        }
        verify_delegate(&new, ROOT, &new)?;
        self.root = new;
        Ok(())
    }

    /// UpdateTimestamp.
    pub fn update_timestamp(&mut self, data: &[u8]) -> Result<(), Error> {
        if self.snapshot.is_some() {
            return Err(Error::Runtime("cannot update timestamp after snapshot".into()));
        }
        if self.root.signed.is_expired(self.now) {
            return Err(Error::Expired("final root.json is expired".into()));
        }
        let new = Metadata::from_bytes(TIMESTAMP, data)?;
        check_kind(&new, TIMESTAMP)?;
        verify_delegate(&self.root, TIMESTAMP, &new)?;
        if let Some(old) = &self.timestamp {
            if new.signed.version < old.signed.version {
                return Err(Error::BadVersion(format!(
                    "new timestamp version {} must be >= {}",
                    new.signed.version, old.signed.version
                )));
            }
            if new.signed.version == old.signed.version {
                return Err(Error::EqualVersion(format!(
                    "new timestamp version {} equals the old one {}",
                    new.signed.version, old.signed.version
                )));
            }
            let v = |m: &Metadata| m.signed.meta("snapshot.json").map_or(0, |f| f.version);
            if v(&new) < v(old) {
                return Err(Error::BadVersion(format!(
                    "new snapshot version {} must be >= {}",
                    v(&new),
                    v(old)
                )));
            }
        }
        let expired = new.signed.is_expired(self.now);
        self.timestamp = Some(new);
        if expired {
            return Err(Error::Expired("timestamp.json is expired".into()));
        }
        Ok(())
    }

    fn check_final_timestamp(&self) -> Result<(), Error> {
        if self
            .timestamp
            .as_ref()
            .is_some_and(|t| t.signed.is_expired(self.now))
        {
            return Err(Error::Expired("timestamp.json is expired".into()));
        }
        Ok(())
    }

    /// UpdateSnapshot: `trusted` for the copy already on disk, whose length and hashes
    /// were checked when it was stored.
    pub fn update_snapshot(&mut self, data: &[u8], trusted: bool) -> Result<(), Error> {
        let Some(timestamp) = &self.timestamp else {
            return Err(Error::Runtime("cannot update snapshot before timestamp".into()));
        };
        if self.targets_of(TARGETS).is_some() {
            return Err(Error::Runtime("cannot update snapshot after targets".into()));
        }
        self.check_final_timestamp()?;
        let meta = timestamp.signed.meta("snapshot.json").cloned();
        if !trusted && let Some(meta) = &meta {
            meta.verify(data)?;
        }
        let new = Metadata::from_bytes(SNAPSHOT, data)?;
        check_kind(&new, SNAPSHOT)?;
        verify_delegate(&self.root, SNAPSHOT, &new)?;
        if let Some(old) = &self.snapshot
            && let (Body::Meta(Some(old_meta)), Body::Meta(new_meta)) = (&old.signed.body, &new.signed.body)
        {
            for (name, info) in old_meta {
                let new_info = new_meta
                    .as_ref()
                    .and_then(|m| m.iter().rev().find(|(n, _)| n == name))
                    .ok_or_else(|| Error::Repository(format!("new snapshot is missing info for {name}")))?;
                let (ov, nv) = (
                    info.as_ref().map_or(0, |i| i.version),
                    new_info.1.as_ref().map_or(0, |i| i.version),
                );
                if nv < ov {
                    return Err(Error::BadVersion(format!(
                        "expected {name} version {nv}, got {ov}"
                    )));
                }
            }
        }
        self.snapshot = Some(new);
        self.check_final_snapshot()
    }

    fn check_final_snapshot(&self) -> Result<(), Error> {
        let Some(snapshot) = &self.snapshot else {
            return Ok(());
        };
        if snapshot.signed.is_expired(self.now) {
            return Err(Error::Expired("snapshot.json is expired".into()));
        }
        let want = self
            .timestamp
            .as_ref()
            .and_then(|t| t.signed.meta("snapshot.json"))
            .map_or(0, |m| m.version);
        if snapshot.signed.version != want {
            return Err(Error::BadVersion(format!(
                "expected {want}, got {}",
                snapshot.signed.version
            )));
        }
        Ok(())
    }

    /// UpdateDelegatedTargets.
    pub fn update_targets(&mut self, data: &[u8], name: &str, delegator: &str) -> Result<(), Error> {
        let Some(snapshot) = &self.snapshot else {
            return Err(Error::Runtime("cannot load targets before snapshot".into()));
        };
        self.check_final_snapshot()?;
        if delegator != ROOT && self.targets_of(delegator).is_none() {
            return Err(Error::Runtime("cannot load targets before delegator".into()));
        }
        let meta = snapshot
            .signed
            .meta(&format!("{name}.json"))
            .cloned()
            .ok_or_else(|| Error::Repository(format!("snapshot does not contain information for {name}")))?;
        meta.verify(data)?;
        let new = Metadata::from_bytes(TARGETS, data)?;
        check_kind(&new, TARGETS)?;
        match delegator {
            ROOT => verify_delegate(&self.root, name, &new)?,
            d => {
                let parent = self
                    .targets_of(d)
                    .ok_or_else(|| Error::Runtime("cannot load targets before delegator".into()))?;
                verify_delegate(parent, name, &new)?;
            }
        }
        if new.signed.version != meta.version {
            return Err(Error::BadVersion(format!(
                "expected {name} version {}, got {}",
                meta.version, new.signed.version
            )));
        }
        if new.signed.is_expired(self.now) {
            return Err(Error::Expired(format!("new {name} is expired")));
        }
        self.targets.retain(|(n, _)| n != name);
        self.targets.push((name.to_string(), new));
        Ok(())
    }
}

/// filepath.ErrBadPattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadPattern;

/// path/filepath.Match of one path element (on Windows, `\` is no escape, as there).
pub fn path_match(pattern: &str, name: &str) -> Result<bool, BadPattern> {
    let escape = !cfg!(windows);
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    fn class(p: &[char], mut i: usize, c: char, escape: bool) -> Result<(bool, usize), BadPattern> {
        // p[i] is just past '['.
        let negated = p.get(i) == Some(&'^');
        if negated {
            i += 1;
        }
        let mut matched = false;
        let mut first = true;
        loop {
            match p.get(i) {
                None => return Err(BadPattern),
                Some(']') if !first => {
                    i += 1;
                    break;
                }
                _ => {}
            }
            first = false;
            let lo = take_char(p, &mut i, escape)?;
            let hi = if p.get(i) == Some(&'-') {
                i += 1;
                take_char(p, &mut i, escape)?
            } else {
                lo
            };
            if lo <= c && c <= hi {
                matched = true;
            }
        }
        Ok((matched != negated, i))
    }
    fn take_char(p: &[char], i: &mut usize, escape: bool) -> Result<char, BadPattern> {
        match p.get(*i) {
            None | Some('-' | ']') => Err(BadPattern),
            Some('\\') if escape => {
                *i += 1;
                let c = *p.get(*i).ok_or(BadPattern)?;
                *i += 1;
                Ok(c)
            }
            Some(&c) => {
                *i += 1;
                Ok(c)
            }
        }
    }
    // Backtracking over `*`, which never matches the separator.
    fn go(p: &[char], pi: usize, n: &[char], ni: usize, escape: bool) -> Result<bool, BadPattern> {
        let Some(&c) = p.get(pi) else {
            return Ok(ni == n.len());
        };
        match c {
            '*' => {
                let mut k = ni;
                loop {
                    if go(p, pi + 1, n, k, escape)? {
                        return Ok(true);
                    }
                    match n.get(k) {
                        Some('/') | None => return check_rest(p, pi + 1, escape).map(|()| false),
                        Some(_) => k += 1,
                    }
                }
            }
            '?' => match n.get(ni) {
                Some(&x) if x != '/' => go(p, pi + 1, n, ni + 1, escape),
                _ => check_rest(p, pi + 1, escape).map(|()| false),
            },
            '[' => {
                let Some(&x) = n.get(ni) else {
                    class(p, pi + 1, '\0', escape)?;
                    return Ok(false);
                };
                let (ok, next) = class(p, pi + 1, x, escape)?;
                if ok && x != '/' {
                    go(p, next, n, ni + 1, escape)
                } else {
                    check_rest(p, next, escape).map(|()| false)
                }
            }
            '\\' if escape => {
                let Some(&lit) = p.get(pi + 1) else {
                    return Err(BadPattern);
                };
                if n.get(ni) == Some(&lit) {
                    go(p, pi + 2, n, ni + 1, escape)
                } else {
                    check_rest(p, pi + 2, escape).map(|()| false)
                }
            }
            lit => {
                if n.get(ni) == Some(&lit) {
                    go(p, pi + 1, n, ni + 1, escape)
                } else {
                    check_rest(p, pi + 1, escape).map(|()| false)
                }
            }
        }
    }
    // filepath.Match reports ErrBadPattern for a malformed rest of the pattern even when
    // the name has failed to match.
    fn check_rest(p: &[char], mut i: usize, escape: bool) -> Result<(), BadPattern> {
        while let Some(&c) = p.get(i) {
            match c {
                '[' => i = class(p, i + 1, '\0', escape)?.1,
                '\\' if escape => {
                    if p.get(i + 1).is_none() {
                        return Err(BadPattern);
                    }
                    i += 2;
                }
                _ => i += 1,
            }
        }
        Ok(())
    }
    go(&p, 0, &n, 0, escape)
}

/// isTargetInPathPattern: as many elements as the pattern, each matching its own.
pub fn target_in_pattern(target: &str, pattern: &str) -> bool {
    let t: Vec<&str> = target.split('/').collect();
    let p: Vec<&str> = pattern.split('/').collect();
    t.len() == p.len() && t.iter().zip(&p).all(|(t, p)| path_match(p, t).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    // Go's own filepath.Match cases (path/filepath/match_test.go), those without a
    // separator in the name.
    #[test]
    fn globs_match_as_filepath_match() {
        for (pattern, name, want) in [
            ("abc", "abc", Ok(true)),
            ("*", "abc", Ok(true)),
            ("*c", "abc", Ok(true)),
            ("a*", "a", Ok(true)),
            ("a*", "abc", Ok(true)),
            ("a*/b", "abc/b", Ok(true)),
            ("a*b*c*d*e*/f", "axbxcxdxe/f", Ok(true)),
            ("a*b?c*x", "abxbbxdbxebxczzx", Ok(true)),
            ("a*b?c*x", "abxbbxdbxebxczzy", Ok(false)),
            ("ab[c]", "abc", Ok(true)),
            ("ab[b-d]", "abc", Ok(true)),
            ("ab[e-g]", "abc", Ok(false)),
            ("ab[^c]", "abc", Ok(false)),
            ("ab[^b-d]", "abc", Ok(false)),
            ("ab[^e-g]", "abc", Ok(true)),
            ("a\\*b", "a*b", Ok(true)),
            ("a\\*b", "ab", Ok(false)),
            ("a?b", "a☺b", Ok(true)),
            ("a[^a]b", "a☺b", Ok(true)),
            ("a???b", "a☺b", Ok(false)),
            ("a[^a][^a][^a]b", "a☺b", Ok(false)),
            ("[a-ζ]*", "α", Ok(true)),
            ("*[a-ζ]", "A", Ok(false)),
            ("a?b", "a/b", Ok(false)),
            ("a*b", "a/b", Ok(false)),
            ("[\\]a]", "]", Ok(true)),
            ("[\\-]", "-", Ok(true)),
            ("[x\\-]", "x", Ok(true)),
            ("[x\\-]", "-", Ok(true)),
            ("[x\\-]", "z", Ok(false)),
            ("[\\-x]", "x", Ok(true)),
            ("[\\-x]", "-", Ok(true)),
            ("[\\-x]", "a", Ok(false)),
            ("[]a]", "]", Err(BadPattern)),
            ("[-]", "-", Err(BadPattern)),
            ("[x-]", "x", Err(BadPattern)),
            ("[x-]", "-", Err(BadPattern)),
            ("[x-]", "z", Err(BadPattern)),
            ("[-x]", "x", Err(BadPattern)),
            ("[-x]", "-", Err(BadPattern)),
            ("[-x]", "a", Err(BadPattern)),
            ("\\", "a", Err(BadPattern)),
            ("[a-b-c]", "a", Err(BadPattern)),
            ("[", "a", Err(BadPattern)),
            ("[^", "a", Err(BadPattern)),
            ("[^bc", "a", Err(BadPattern)),
            ("a[", "a", Err(BadPattern)),
            ("a[", "ab", Err(BadPattern)),
            ("a[", "x", Err(BadPattern)),
            ("a/b[", "x", Err(BadPattern)),
            ("*x", "xxx", Ok(true)),
        ] {
            if cfg!(windows) && pattern.contains('\\') {
                continue;
            }
            assert_eq!(path_match(pattern, name), want, "{pattern} {name}");
        }
    }
}
