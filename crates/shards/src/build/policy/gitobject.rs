//! Git commit and tag objects as BuildKit's gitobject package reads them
//! (util/gitutil/gitobject): headers, message, the signature and what it signs, the
//! object's checksum, and its commit or tag with their actors.

use std::collections::BTreeMap;

/// GitObject: its headers and message as text, its signature and what it signs as the
/// bytes they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitObject {
    pub tag: bool,
    pub headers: BTreeMap<String, Vec<String>>,
    pub message: String,
    pub signature: Vec<u8>,
    pub signed_data: Vec<u8>,
    pub raw: Vec<u8>,
}

/// Actor: a name, an email, and when, with the offset it was written with.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Actor {
    pub name: String,
    pub email: String,
    /// RFC 3339 as Go's JSON writes a time.Time in its zone.
    pub when: Option<String>,
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// gitobject.Parse.
pub fn parse(raw: &[u8]) -> Result<GitObject, String> {
    let tag = raw.starts_with(b"object ");
    let mut obj = GitObject {
        tag,
        headers: BTreeMap::new(),
        message: String::new(),
        signature: Vec::new(),
        signed_data: Vec::new(),
        raw: raw.to_vec(),
    };
    let (mut headers_done, mut message_done, mut in_sig) = (false, false, false);
    let mut sig_lines: Vec<&[u8]> = Vec::new();
    let mut message_lines: Vec<&[u8]> = Vec::new();
    let mut signed_lines: Vec<&[u8]> = Vec::new();
    for l in raw.split(|&b| b == b'\n') {
        if !headers_done {
            if l.is_empty() {
                headers_done = true;
                signed_lines.push(l);
                continue;
            }
            if !tag
                && let Some(v) = l
                    .strip_prefix(b"gpgsig ")
                    .or_else(|| l.strip_prefix(b"gpgsig-sha256 "))
            {
                in_sig = true;
                sig_lines.push(v);
                continue;
            }
            if in_sig {
                if let Some(v) = l.strip_prefix(b" ") {
                    sig_lines.push(v);
                    continue;
                }
                in_sig = false;
            }
            signed_lines.push(l);
            if let Some(i) = l.iter().position(|&b| b == b' ') {
                let (k, v) = (l.get(..i).unwrap_or_default(), l.get(i + 1..).unwrap_or_default());
                obj.headers.entry(text(k)).or_default().push(text(v));
            }
            continue;
        }
        if tag && (l == b"-----BEGIN PGP SIGNATURE-----" || l == b"-----BEGIN SSH SIGNATURE-----") {
            message_done = true;
        }
        if message_done {
            sig_lines.push(l);
        } else {
            message_lines.push(l);
            signed_lines.push(l);
        }
    }
    let message = message_lines.join(&b'\n');
    obj.message = text(message.strip_suffix(b"\n").unwrap_or(&message));
    obj.signature = sig_lines.join(&b'\n');
    obj.signed_data = signed_lines.join(&b'\n');
    if tag {
        obj.signed_data.push(b'\n');
    }
    let (kind, required): (&str, &[&str]) = if tag {
        ("tag", &["object", "type", "tag", "tagger"])
    } else {
        ("commit", &["tree", "author", "committer"])
    };
    for h in required {
        if !obj.headers.contains_key(*h) {
            return Err(format!("invalid {kind} object: missing {h} header"));
        }
    }
    Ok(obj)
}

impl GitObject {
    /// VerifyChecksum: the object hashed with its header, by SHA-1 or SHA-256 as the
    /// checksum's length says.
    pub fn verify_checksum(&self, sha: &str) -> Result<(), String> {
        let algorithm = match sha.len() {
            40 => &aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY,
            64 => &aws_lc_rs::digest::SHA256,
            n => return Err(format!("unsupported sha length {n}")),
        };
        let header = format!("{} {}\0", if self.tag { "tag" } else { "commit" }, self.raw.len());
        let mut ctx = aws_lc_rs::digest::Context::new(algorithm);
        ctx.update(header.as_bytes());
        ctx.update(&self.raw);
        let got: String = ctx.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect();
        if got != sha {
            return Err(format!("checksum mismatch: expected {sha}, got {got}"));
        }
        Ok(())
    }

    fn first(&self, k: &str) -> String {
        self.headers
            .get(k)
            .and_then(|v| v.first())
            .cloned()
            .unwrap_or_default()
    }
}

/// A commit (gitobject.Commit).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Commit {
    pub tree: String,
    pub parents: Vec<String>,
    pub author: Actor,
    pub committer: Actor,
    pub message: String,
}

/// A tag (gitobject.Tag).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tag {
    pub object: String,
    pub kind: String,
    pub tag: String,
    pub tagger: Actor,
    pub message: String,
}

impl GitObject {
    pub fn to_commit(&self) -> Result<Commit, String> {
        if self.tag {
            return Err("not a commit object".into());
        }
        Ok(Commit {
            tree: self.first("tree"),
            parents: self.headers.get("parent").cloned().unwrap_or_default(),
            author: actor(&self.first("author")),
            committer: actor(&self.first("committer")),
            message: self.message.clone(),
        })
    }

    pub fn to_tag(&self) -> Result<Tag, String> {
        if !self.tag {
            return Err("not a tag object".into());
        }
        Ok(Tag {
            object: self.first("object"),
            kind: self.first("type"),
            tag: self.first("tag"),
            tagger: actor(&self.first("tagger")),
            message: self.message.clone(),
        })
    }
}

/// parseActor: `Name <email> UNIX ±HHMM`, the name before the last `<`; malformed, the
/// whole a name.
fn actor(s: &str) -> Actor {
    let s = s.trim();
    let mut a = Actor::default();
    let (Some(start), Some(end)) = (s.rfind('<'), s.rfind('>')) else {
        a.name = s.to_string();
        return a;
    };
    if end < start {
        a.name = s.to_string();
        return a;
    }
    a.name = s.get(..start).unwrap_or_default().trim().to_string();
    a.email = s.get(start + 1..end).unwrap_or_default().trim().to_string();
    let rest = s.get(end + 1..).unwrap_or_default().trim();
    let parts: Vec<&str> = rest.split_whitespace().collect();
    let (Some(unix), Some(tz)) = (parts.first(), parts.get(1)) else {
        return a;
    };
    let Ok(unix) = unix.parse::<i64>() else {
        return a;
    };
    let b = tz.as_bytes();
    if b.len() != 5 || !matches!(b.first(), Some(b'+' | b'-')) {
        return a;
    }
    let sign = if b.first() == Some(&b'-') { -1 } else { 1 };
    // strconv.Atoi's errors ignored, as gitobject ignores them: 0.
    let num = |r: std::ops::Range<usize>| tz.get(r).and_then(|x| x.parse::<i64>().ok()).unwrap_or(0);
    let offset = sign * (num(1..3) * 3600 + num(3..5) * 60);
    a.when = rfc3339(unix, offset);
    a
}

/// `time.Unix(unix, 0).In(time.FixedZone("", offset))`, as encoding/json writes it.
fn rfc3339(unix: i64, offset: i64) -> Option<String> {
    let local = unix.checked_add(offset)?;
    let mut t = shards_dockerfile::go::Time::from_unix(local);
    t.offset = i32::try_from(offset).ok()?;
    t.rfc3339_nano().ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    // The measured case (buildx v0.37.1, octocat/Hello-World's master).
    #[test]
    fn actors_read_as_gitobject_reads_them() {
        let a = actor("The Octocat <octocat@nowhere.com> 1331075210 -0800");
        assert_eq!(
            (a.name.as_str(), a.email.as_str(), a.when.as_deref()),
            (
                "The Octocat",
                "octocat@nowhere.com",
                Some("2012-03-06T15:06:50-08:00")
            )
        );
        assert_eq!(
            actor("x <y> 0 +0000").when.as_deref(),
            Some("1970-01-01T00:00:00Z")
        );
        assert_eq!(actor("no email").name, "no email");
        assert_eq!(actor("n <e> soon +0000").when, None);
    }
}
