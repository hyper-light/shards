//! A repository's `.git`, as `ADD --keep-git-dir` leaves one: the fetched pack and its
//! index (gitformat-pack.adoc, "Version 2 pack-*.idx files"), the index of the checkout
//! (gitformat-index.adoc, version 2), and the few files git needs to read them.

use sha1_checked::Digest as _;

use crate::Oid;
use crate::pack::Pack;

/// The `.idx` of `pack`: its objects by name, each one's CRC32 and offset, then the
/// pack's checksum and the index's own.
pub fn pack_index(pack: &Pack) -> Result<Vec<u8>, String> {
    let mut spans = pack.spans();
    spans.sort_unstable_by_key(|(oid, _, _)| *oid);
    let mut out = b"\xfftOc".to_vec();
    out.extend_from_slice(&2u32.to_be_bytes());
    let mut fanout = [0u32; 256];
    for (oid, _, _) in &spans {
        let first = usize::from(oid.0[0]);
        for slot in fanout.iter_mut().skip(first) {
            *slot += 1;
        }
    }
    for n in fanout {
        out.extend_from_slice(&n.to_be_bytes());
    }
    for (oid, _, _) in &spans {
        out.extend_from_slice(&oid.0);
    }
    let bytes = pack.bytes();
    for (_, start, end) in &spans {
        let raw = bytes.get(*start..*end).ok_or("a pack's object past its end")?;
        let mut crc = flate2::Crc::new();
        crc.update(raw);
        out.extend_from_slice(&crc.sum().to_be_bytes());
    }
    let mut large = Vec::new();
    for (_, start, _) in &spans {
        let at = u64::try_from(*start).map_err(|_| "a pack too large")?;
        match u32::try_from(at) {
            Ok(small) if small < 0x8000_0000 => out.extend_from_slice(&small.to_be_bytes()),
            _ => {
                let i = u32::try_from(large.len()).map_err(|_| "a pack too large")?;
                out.extend_from_slice(&(0x8000_0000 | i).to_be_bytes());
                large.push(at);
            }
        }
    }
    for at in large {
        out.extend_from_slice(&at.to_be_bytes());
    }
    out.extend_from_slice(bytes.get(bytes.len().saturating_sub(20)..).unwrap_or_default());
    let sum = sha1_checked::Sha1::digest(&out);
    out.extend_from_slice(&sum);
    Ok(out)
}

/// One path of a checkout, as the index records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracked {
    pub path: Vec<u8>,
    /// Git's mode: 0o100644, 0o100755, 0o120000 or 0o160000.
    pub mode: u32,
    pub oid: Oid,
    /// Its size as checked out: a file's bytes, a symlink's target's.
    pub size: u32,
    /// Its time as checked out, seconds since the epoch: the index's ctime and mtime.
    pub time: u32,
}

/// The index (`DIRC`, version 2) of `entries`, sorted as git sorts them: by path's bytes.
/// Its device and inode numbers are 0, which git, finding them unlike the files', checks
/// the files' contents against before it trusts.
pub fn index(entries: &[Tracked]) -> Result<Vec<u8>, String> {
    let mut sorted: Vec<&Tracked> = entries.iter().collect();
    sorted.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    let mut out = b"DIRC".to_vec();
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(
        &u32::try_from(sorted.len())
            .map_err(|_| "too many paths")?
            .to_be_bytes(),
    );
    for e in sorted {
        let start = out.len();
        for field in [e.time, 0, e.time, 0, 0, 0, e.mode, 0, 0, e.size] {
            out.extend_from_slice(&field.to_be_bytes());
        }
        out.extend_from_slice(&e.oid.0);
        // Its name's length, up to 0xfff, which says "longer"; no stage, no flags.
        let flags = u16::try_from(e.path.len().min(0xfff)).unwrap_or(0xfff);
        out.extend_from_slice(&flags.to_be_bytes());
        out.extend_from_slice(&e.path);
        // One to eight NULs, to a multiple of eight from the entry's start.
        let len = out.len() - start;
        out.extend(std::iter::repeat_n(0u8, 8 - len % 8));
    }
    let sum = sha1_checked::Sha1::digest(&out);
    out.extend_from_slice(&sum);
    Ok(out)
}

/// A ref the checkout keeps: a branch's (`refs/heads/NAME`) or a tag's (`refs/tags/NAME`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptRef {
    pub name: Vec<u8>,
    pub oid: Oid,
}

/// The files of a `.git` holding `pack` with HEAD detached at `head`, shallow there,
/// `origin` at `url`, the ref asked for, `index`, each submodule's `(name, url)` in its
/// config, and, for a submodule's own repository, the path back to its work tree: each
/// path relative to the `.git` and its bytes. Objects are read-only, as git writes them.
pub fn git_dir(
    pack: &Pack,
    head: &Oid,
    url: &str,
    kept: Option<&KeptRef>,
    index: Vec<u8>,
    submodules: &[(Vec<u8>, Vec<u8>)],
    worktree: Option<&str>,
) -> Result<Vec<(String, Vec<u8>, u32)>, String> {
    let pack_name = Oid(pack
        .bytes()
        .get(pack.bytes().len().saturating_sub(20)..)
        .and_then(|t| t.try_into().ok())
        .ok_or("a pack without a trailer")?)
    .hex();
    let mut config = String::from(
        "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = true\n",
    );
    if let Some(w) = worktree {
        config.push_str(&format!("\tworktree = {}\n", quote(w.as_bytes())));
    }
    config.push_str(&format!(
        "[remote \"origin\"]\n\turl = {}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
        quote(url.as_bytes())
    ));
    for (name, sub_url) in submodules {
        config.push_str(&format!(
            "[submodule \"{}\"]\n\tactive = true\n\turl = {}\n",
            String::from_utf8_lossy(name)
                .replace('\\', "\\\\")
                .replace('"', "\\\""),
            quote(sub_url)
        ));
    }
    let mut files = vec![
        (
            "HEAD".to_string(),
            format!("{}\n", head.hex()).into_bytes(),
            0o644,
        ),
        ("config".to_string(), config.into_bytes(), 0o644),
        (
            "shallow".to_string(),
            format!("{}\n", head.hex()).into_bytes(),
            0o644,
        ),
        ("index".to_string(), index, 0o644),
        (
            format!("objects/pack/pack-{pack_name}.pack"),
            pack.bytes().to_vec(),
            0o444,
        ),
        (
            format!("objects/pack/pack-{pack_name}.idx"),
            pack_index(pack)?,
            0o444,
        ),
    ];
    if let Some(r) = kept {
        files.push((
            String::from_utf8_lossy(&r.name).into_owned(),
            format!("{}\n", r.oid.hex()).into_bytes(),
            0o644,
        ));
    }
    Ok(files)
}

/// A config value written so that git reads it back as it is: quoted where it has what a
/// bare value loses (leading or trailing space, `#`, `;`), its `\` and `"` escaped.
fn quote(v: &[u8]) -> String {
    let s = String::from_utf8_lossy(v);
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    if s.starts_with(' ') || s.ends_with(' ') || s.contains(['#', ';']) {
        format!("\"{escaped}\"")
    } else {
        escaped
    }
}
