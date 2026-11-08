//! Protocol v2 (gitprotocol-v2.adoc) over smart HTTP (gitprotocol-http.adoc): the
//! capability advertisement `GET $URL/info/refs?service=git-upload-pack` answers, and
//! the bodies of `ls-refs` and `fetch`, which are `POST`ed to `$URL/git-upload-pack`.

use std::io::Read;

use crate::Oid;
use crate::pktline::{self, Packet};

/// The `Git-Protocol` header that asks a server for version 2.
pub const VERSION_2: (&str, &str) = ("Git-Protocol", "version=2");
pub const ADVERTISEMENT_TYPE: &str = "application/x-git-upload-pack-advertisement";
pub const REQUEST_TYPE: &str = "application/x-git-upload-pack-request";
pub const RESULT_TYPE: &str = "application/x-git-upload-pack-result";

/// What a server says it can do.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// Each capability's name and value (`fetch=shallow wait-for-done` and such).
    pub list: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Capabilities {
    pub fn value(&self, name: &[u8]) -> Option<&[u8]> {
        self.list
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_slice())
    }

    /// Whether `command` takes `feature` (`fetch=shallow filter` takes `shallow`).
    pub fn takes(&self, command: &[u8], feature: &[u8]) -> bool {
        self.value(command)
            .is_some_and(|v| v.split(|&b| b == b' ').any(|f| f == feature))
    }
}

/// The advertisement `body` holds: version 2's capabilities, after the `# service=` line
/// a smart HTTP server may send first; `None` where the server speaks an older version.
pub fn advertisement(mut body: impl Read) -> Result<Option<Capabilities>, String> {
    let mut caps = Capabilities::default();
    let mut first = true;
    loop {
        match pktline::read(&mut body).map_err(|e| format!("the server's advertisement: {e}"))? {
            Some(Packet::Data(line)) => {
                let line = pktline::text(&line);
                if first && line.starts_with(b"# service=") {
                    // Its flush follows; the advertisement proper comes after it.
                    match pktline::read(&mut body).map_err(|e| e.to_string())? {
                        Some(Packet::Flush) => continue,
                        _ => return Err("a bad smart HTTP advertisement".into()),
                    }
                }
                if first {
                    if line != b"version 2" {
                        return Ok(None);
                    }
                    first = false;
                    continue;
                }
                let (name, value) = match line.iter().position(|&b| b == b'=') {
                    Some(i) => (
                        line.get(..i).unwrap_or_default(),
                        line.get(i + 1..).unwrap_or_default(),
                    ),
                    None => (line, &b""[..]),
                };
                caps.list.push((name.to_vec(), value.to_vec()));
            }
            Some(Packet::Flush) | None if !first => return Ok(Some(caps)),
            Some(Packet::Flush) | None => return Ok(None),
            Some(Packet::Delimiter | Packet::ResponseEnd) => return Err("a bad advertisement".into()),
        }
    }
}

/// A command's request: `command=NAME`, the agent, then its arguments.
fn request(command: &str, agent: &str, args: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    let e = |e: std::io::Error| e.to_string();
    let mut out = pktline::data(format!("command={command}\n").as_bytes()).map_err(e)?;
    out.extend(pktline::data(format!("agent={agent}\n").as_bytes()).map_err(e)?);
    out.extend_from_slice(pktline::DELIMITER);
    for arg in args {
        out.extend(pktline::data(arg).map_err(e)?);
    }
    out.extend_from_slice(pktline::FLUSH);
    Ok(out)
}

/// A ref, as `ls-refs` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    pub oid: Oid,
    pub name: Vec<u8>,
    /// What a symbolic ref (HEAD) points at.
    pub target: Option<Vec<u8>>,
    /// What an annotated tag names, peeled.
    pub peeled: Option<Oid>,
}

/// `ls-refs` of the refs under `prefixes`, with symbolic refs' targets and tags peeled.
pub fn ls_refs(agent: &str, prefixes: &[&[u8]]) -> Result<Vec<u8>, String> {
    let mut args = vec![b"symrefs\n".to_vec(), b"peel\n".to_vec()];
    for p in prefixes {
        args.push([b"ref-prefix ".as_slice(), p, b"\n"].concat());
    }
    request("ls-refs", agent, &args)
}

/// The refs an `ls-refs` answer lists.
pub fn refs(mut body: impl Read) -> Result<Vec<Ref>, String> {
    let mut out = Vec::new();
    loop {
        match pktline::read(&mut body).map_err(|e| format!("the server's refs: {e}"))? {
            Some(Packet::Data(line)) => {
                let line = pktline::text(&line);
                if let Some(e) = line.strip_prefix(b"ERR ") {
                    return Err(format!("remote error: {}", String::from_utf8_lossy(e)));
                }
                let mut fields = line.split(|&b| b == b' ');
                let oid = fields.next().and_then(Oid::parse);
                let name = fields.next();
                let (Some(oid), Some(name)) = (oid, name) else {
                    // An unborn HEAD (`unborn HEAD symref-target:...`) names no object.
                    continue;
                };
                let mut r = Ref {
                    oid,
                    name: name.to_vec(),
                    target: None,
                    peeled: None,
                };
                for attr in fields {
                    if let Some(t) = attr.strip_prefix(b"symref-target:") {
                        r.target = Some(t.to_vec());
                    } else if let Some(p) = attr.strip_prefix(b"peeled:") {
                        r.peeled = Oid::parse(p);
                    }
                }
                out.push(r);
            }
            Some(Packet::Flush) | None => return Ok(out),
            Some(_) => return Err("a bad ls-refs answer".into()),
        }
    }
}

/// `fetch` of `wants`, `depth` commits deep (0: all of them), the pack's objects deltified
/// against offsets, without the server's progress.
pub fn fetch(agent: &str, wants: &[Oid], depth: u32, caps: &Capabilities) -> Result<Vec<u8>, String> {
    let mut args = Vec::new();
    for w in wants {
        args.push(format!("want {}\n", w.hex()).into_bytes());
    }
    if depth > 0 {
        if !caps.takes(b"fetch", b"shallow") {
            return Err("the server cannot fetch a shallow history".into());
        }
        args.push(format!("deepen {depth}\n").into_bytes());
    }
    args.push(b"ofs-delta\n".to_vec());
    args.push(b"no-progress\n".to_vec());
    args.push(b"done\n".to_vec());
    request("fetch", agent, &args)
}

/// What a `fetch` answer holds: the commits it made shallow, and the pack.
#[derive(Debug, Default)]
pub struct Fetched {
    pub shallow: Vec<Oid>,
    pub pack: Vec<u8>,
}

/// A `fetch` answer's sections (gitprotocol-v2.adoc, "fetch" output): `shallow-info`,
/// `wanted-refs` and `packfile-uris` are read past to the `packfile`, whose packets are
/// side-band-64k's: 1 the pack, 2 progress, 3 an error, which ends the fetch. No more
/// than `largest` bytes of pack are taken.
pub fn fetched(mut body: impl Read, largest: usize) -> Result<Fetched, String> {
    let mut out = Fetched::default();
    let mut section: Vec<u8> = Vec::new();
    loop {
        let packet = pktline::read(&mut body).map_err(|e| format!("the server's pack: {e}"))?;
        match packet {
            Some(Packet::Data(line)) if section.is_empty() || section == b"\n" => {
                let line = pktline::text(&line);
                if let Some(e) = line.strip_prefix(b"ERR ") {
                    return Err(format!("remote error: {}", String::from_utf8_lossy(e)));
                }
                section = line.to_vec();
            }
            Some(Packet::Data(line)) if section == b"packfile" => {
                let (band, data) = line.split_first().ok_or("an empty side-band packet")?;
                match band {
                    1 => {
                        if out.pack.len().saturating_add(data.len()) > largest {
                            return Err(format!("a pack larger than {largest} bytes"));
                        }
                        out.pack.extend_from_slice(data);
                    }
                    2 => {}
                    3 => {
                        return Err(format!(
                            "remote error: {}",
                            String::from_utf8_lossy(pktline::text(data))
                        ));
                    }
                    b => return Err(format!("a side-band packet on band {b}")),
                }
            }
            Some(Packet::Data(line)) => {
                if section == b"shallow-info"
                    && let Some(hex) = pktline::text(&line).strip_prefix(b"shallow ")
                {
                    out.shallow.push(Oid::parse(hex).ok_or("a bad shallow line")?);
                }
            }
            Some(Packet::Delimiter) => section.clear(),
            Some(Packet::Flush | Packet::ResponseEnd) | None if section == b"packfile" => return Ok(out),
            Some(Packet::Flush | Packet::ResponseEnd) | None => {
                return Err("the server sent no pack".into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(lines: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for l in lines {
            match *l {
                b"0000" | b"0001" | b"0002" => out.extend_from_slice(l),
                _ => out.extend(pktline::data(l).unwrap()),
            }
        }
        out
    }

    #[test]
    fn version_2_is_spoken_as_gitprotocol_v2_says() {
        let adv = wire(&[
            b"# service=git-upload-pack\n",
            b"0000",
            b"version 2\n",
            b"agent=git/2.51.0\n",
            b"ls-refs=unborn\n",
            b"fetch=shallow wait-for-done\n",
            b"0000",
        ]);
        let caps = advertisement(&adv[..]).unwrap().unwrap();
        assert!(caps.takes(b"fetch", b"shallow"));
        assert!(!caps.takes(b"fetch", b"filter"));
        assert_eq!(caps.value(b"agent"), Some(&b"git/2.51.0"[..]));
        let v0 = wire(&[
            b"# service=git-upload-pack\n",
            b"0000",
            b"0123 HEAD\0caps\n",
            b"0000",
        ]);
        assert_eq!(advertisement(&v0[..]).unwrap(), None);

        assert_eq!(
            ls_refs("shards", &[b"HEAD"]).unwrap(),
            b"0014command=ls-refs\n0011agent=shards\n0001000csymrefs\n0009peel\n0014ref-prefix HEAD\n0000"
        );
        let a = "ce013625030ba8dba906f756967f9e9ca394464a";
        let b = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
        let answer = wire(&[
            format!("{a} HEAD symref-target:refs/heads/main\n").as_bytes(),
            format!("{a} refs/heads/main\n").as_bytes(),
            format!("{b} refs/tags/v1 peeled:{a}\n").as_bytes(),
            b"unborn refs/heads/x\n",
            b"0000",
        ]);
        let listed = refs(&answer[..]).unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0].target.as_deref(), Some(&b"refs/heads/main"[..]));
        assert_eq!(listed[2].peeled.unwrap().hex(), a);
        assert!(
            refs(&wire(&[b"ERR denied\n"])[..])
                .unwrap_err()
                .contains("denied")
        );

        let want = Oid::parse(a.as_bytes()).unwrap();
        let body = fetch("shards", &[want], 1, &caps).unwrap();
        assert!(String::from_utf8_lossy(&body).contains(&format!("want {a}\n")));
        assert!(String::from_utf8_lossy(&body).contains("deepen 1\n"));
        assert!(fetch("shards", &[want], 1, &Capabilities::default()).is_err());

        let answer = wire(&[
            b"shallow-info\n",
            format!("shallow {a}\n").as_bytes(),
            b"0001",
            b"packfile\n",
            b"\x02counting\n",
            b"\x01PACK",
            b"\x01rest",
            b"0000",
        ]);
        let got = fetched(&answer[..], 1 << 20).unwrap();
        assert_eq!(got.pack, b"PACKrest");
        assert_eq!(got.shallow, [want]);
        assert!(fetched(&answer[..], 4).is_err());
        let failed = wire(&[b"packfile\n", b"\x03upload-pack: not our ref\n", b"0000"]);
        assert_eq!(
            fetched(&failed[..], 99).unwrap_err(),
            "remote error: upload-pack: not our ref"
        );
    }
}
