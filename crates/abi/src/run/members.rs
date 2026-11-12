//! What an orchestrator stages into a run's members, and hears of their ends (D231): setup
//! entries that name a member by its declared index (agents first, then harnesses, in the
//! order the image's Agentfile declares them: the n-th runs as `DOMAIN_FIRST_ID + n`), and
//! the frame that says a member's population has ended.
//!
//! Staging is native-sign-in delegation (managed-runtime-plan.md G18): the member reads
//! every byte it is given.

use alloc::vec::Vec;

/// `member-env=<index>\0KEY=VALUE`: a variable of the member's environment, over its
/// image's.
pub const ENV_KEY: &[u8] = b"member-env=";
/// `member-file=<index>\0<mode in octal>\0<path>\0<bytes>`: a regular file under the
/// member's home, its own scratch tmpfs.
pub const FILE_KEY: &[u8] = b"member-file=";
/// `member-ends`, alone: the run's owner hears each member's end
/// ([`kind::MEMBER_ENDED`](super::kind::MEMBER_ENDED)), which a daemon's run does not ask.
pub const ENDS: &[u8] = b"member-ends";
/// The variable that names a member's domain to its processes: init's to set
/// (init domains.rs), never a stager's.
pub const DOMAIN_VARIABLE: &[u8] = b"SHARDS_DOMAIN";
/// A staged file's mode: the permission bits alone (inode(7), `S_IRWXU | S_IRWXG |
/// S_IRWXO`), so no set-ID or sticky bit.
pub const MODE_BITS: u32 = 0o777;
/// The longest path a staged file may have, its NUL counted: Linux's `PATH_MAX`
/// (include/uapi/linux/limits.h).
pub const PATH_MOST: usize = 4096;
/// The longest component of that path: Linux's `NAME_MAX` (include/uapi/linux/limits.h).
pub const NAME_MOST: usize = 255;

/// Why a staging entry is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Its member index is not a decimal u32.
    Index,
    /// Its variable is not `KEY=VALUE` with a nonempty key, or holds a NUL.
    Variable,
    /// Its variable is init's own ([`DOMAIN_VARIABLE`]).
    Reserved,
    /// Its mode is not octal, or has bits past [`MODE_BITS`].
    Mode,
    /// Its path is empty, absolute, holds an empty, `.` or `..` component or a NUL, or is
    /// past Linux's bounds.
    Path,
    /// The entry lacks one of its fields.
    Shape,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Error::Index => "a member index that is not a decimal u32",
            Error::Variable => "a variable that is not KEY=VALUE without NUL",
            Error::Reserved => "SHARDS_DOMAIN, which init sets",
            Error::Mode => "a mode past the permission bits (0777)",
            Error::Path => "a path that is not relative, plain and within PATH_MAX",
            Error::Shape => "an entry missing a field",
        })
    }
}

/// A staged variable, borrowed from its entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Env<'a> {
    pub member: u32,
    /// `KEY=VALUE`.
    pub variable: &'a [u8],
}

impl<'a> Env<'a> {
    /// Its key, before the first `=`.
    pub fn key(&self) -> &'a [u8] {
        let variable: &'a [u8] = self.variable;
        variable.split(|&b| b == b'=').next().unwrap_or_default()
    }
}

/// A staged file, borrowed from its entry.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct File<'a> {
    pub member: u32,
    pub mode: u32,
    /// Relative to the member's home.
    pub path: &'a [u8],
    pub bytes: &'a [u8],
}

/// Its bytes are a stager's, a credential as often as not: never printed.
impl core::fmt::Debug for File<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("File")
            .field("member", &self.member)
            .field("mode", &format_args!("{:o}", self.mode))
            .field("path", &self.path)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// Whether `path` names a file beneath a directory, and nothing else: relative, its
/// components nonempty and neither `.` nor `..`, no NUL, within Linux's bounds.
pub fn path_ok(path: &[u8]) -> bool {
    !path.is_empty()
        && path.len() < PATH_MOST
        && !path.contains(&0)
        && path.first() != Some(&b'/')
        && path
            .split(|&b| b == b'/')
            .all(|c| !c.is_empty() && c != b"." && c != b".." && c.len() <= NAME_MOST)
}

/// `variable` as a staged variable may be: `KEY=VALUE`, its key nonempty, no NUL, and not
/// init's own.
pub fn variable_ok(variable: &[u8]) -> Result<(), Error> {
    let (key, _) = split_once(variable, b'=').ok_or(Error::Variable)?;
    if key.is_empty() || variable.contains(&0) {
        return Err(Error::Variable);
    }
    if key == DOMAIN_VARIABLE {
        return Err(Error::Reserved);
    }
    Ok(())
}

/// The setup entry staging `variable` into member `member`, refused as [`variable_ok`]
/// refuses it.
pub fn env(member: u32, variable: &[u8]) -> Result<Vec<u8>, Error> {
    variable_ok(variable)?;
    let mut entry = Vec::new();
    entry.extend_from_slice(ENV_KEY);
    push_decimal(&mut entry, member);
    entry.push(0);
    entry.extend_from_slice(variable);
    Ok(entry)
}

/// The setup entry staging `bytes` at `path` under member `member`'s home with `mode`,
/// refused where the path or mode is not one a staged file may have.
pub fn file(member: u32, mode: u32, path: &[u8], bytes: &[u8]) -> Result<Vec<u8>, Error> {
    if mode & !MODE_BITS != 0 {
        return Err(Error::Mode);
    }
    if !path_ok(path) {
        return Err(Error::Path);
    }
    let mut entry = Vec::new();
    entry.extend_from_slice(FILE_KEY);
    push_decimal(&mut entry, member);
    entry.push(0);
    push_octal(&mut entry, mode);
    entry.push(0);
    entry.extend_from_slice(path);
    entry.push(0);
    entry.extend_from_slice(bytes);
    Ok(entry)
}

/// A `member-env=` entry's value, checked.
pub fn parse_env(value: &[u8]) -> Result<Env<'_>, Error> {
    let (index, variable) = split_once(value, 0).ok_or(Error::Shape)?;
    variable_ok(variable)?;
    Ok(Env {
        member: decimal(index)?,
        variable,
    })
}

/// A `member-file=` entry's value, checked: its bytes are everything after its path's NUL.
pub fn parse_file(value: &[u8]) -> Result<File<'_>, Error> {
    let (index, rest) = split_once(value, 0).ok_or(Error::Shape)?;
    let (mode, rest) = split_once(rest, 0).ok_or(Error::Shape)?;
    let (path, bytes) = split_once(rest, 0).ok_or(Error::Shape)?;
    let mode = octal(mode)?;
    if !path_ok(path) {
        return Err(Error::Path);
    }
    Ok(File {
        member: decimal(index)?,
        mode,
        path,
        bytes,
    })
}

/// Every `member-env=` entry of `setup` for `member`, in order; the first refused ends
/// it with its error.
pub fn envs_of(setup: &[Vec<u8>], member: u32) -> Result<Vec<Env<'_>>, Error> {
    let mut out = Vec::new();
    for value in setup.iter().filter_map(|e| e.strip_prefix(ENV_KEY)) {
        let env = parse_env(value)?;
        if env.member == member {
            out.push(env);
        }
    }
    Ok(out)
}

/// Every `member-file=` entry of `setup` for `member`, in order; the first refused ends
/// it with its error.
pub fn files_of(setup: &[Vec<u8>], member: u32) -> Result<Vec<File<'_>>, Error> {
    let mut out = Vec::new();
    for value in setup.iter().filter_map(|e| e.strip_prefix(FILE_KEY)) {
        let file = parse_file(value)?;
        if file.member == member {
            out.push(file);
        }
    }
    Ok(out)
}

/// The members `setup` stages anything into, each once, ascending: for a guest to refuse
/// an entry naming no member it starts.
pub fn staged_members(setup: &[Vec<u8>]) -> Result<Vec<u32>, Error> {
    let mut members = Vec::new();
    for entry in setup {
        let member = if let Some(value) = entry.strip_prefix(ENV_KEY) {
            parse_env(value)?.member
        } else if let Some(value) = entry.strip_prefix(FILE_KEY) {
            parse_file(value)?.member
        } else {
            continue;
        };
        if let Err(at) = members.binary_search(&member) {
            members.insert(at, member);
        }
    }
    Ok(members)
}

/// `[member, status]`, each a big-endian u32: the payload of
/// [`kind::MEMBER_ENDED`](super::kind::MEMBER_ENDED), its status as `docker run` reports one
/// (its code, or 128 plus the number of the signal that ended it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ended {
    pub member: u32,
    pub status: u32,
}

impl Ended {
    pub const LEN: usize = 2 * size_of::<u32>();

    pub fn encode(self) -> [u8; Self::LEN] {
        let [a, b, c, d] = self.member.to_be_bytes();
        let [e, f, g, h] = self.status.to_be_bytes();
        [a, b, c, d, e, f, g, h]
    }

    pub fn decode(bytes: &[u8]) -> Option<Ended> {
        let [a, b, c, d, e, f, g, h] = <[u8; Self::LEN]>::try_from(bytes).ok()?;
        Some(Ended {
            member: u32::from_be_bytes([a, b, c, d]),
            status: u32::from_be_bytes([e, f, g, h]),
        })
    }
}

fn split_once(bytes: &[u8], at: u8) -> Option<(&[u8], &[u8])> {
    let i = bytes.iter().position(|&b| b == at)?;
    Some((bytes.get(..i)?, bytes.get(i + 1..)?))
}

fn decimal(text: &[u8]) -> Result<u32, Error> {
    // No sign, no leading zero but "0" itself, digits alone: one spelling for each index.
    let canonical = match text {
        [] => false,
        [b'0'] => true,
        [b'0', ..] => false,
        _ => text.iter().all(u8::is_ascii_digit),
    };
    if !canonical {
        return Err(Error::Index);
    }
    core::str::from_utf8(text)
        .ok()
        .and_then(|t| t.parse::<u32>().ok())
        .ok_or(Error::Index)
}

fn octal(text: &[u8]) -> Result<u32, Error> {
    if text.is_empty() || text.len() > 4 || !text.iter().all(|b| (b'0'..=b'7').contains(b)) {
        return Err(Error::Mode);
    }
    let mode = text
        .iter()
        .try_fold(0u32, |m, &d| m.checked_mul(8)?.checked_add(u32::from(d - b'0')))
        .ok_or(Error::Mode)?;
    if mode & !MODE_BITS != 0 {
        return Err(Error::Mode);
    }
    Ok(mode)
}

fn push_decimal(out: &mut Vec<u8>, n: u32) {
    let mut digits = [0u8; 10];
    let mut at = digits.len();
    let mut n = n;
    loop {
        at -= 1;
        if let Some(d) = digits.get_mut(at) {
            *d = b'0' + (n % 10) as u8;
        }
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(digits.get(at..).unwrap_or_default());
}

fn push_octal(out: &mut Vec<u8>, mode: u32) {
    // At most three digits: `file` refused any bit past 0777.
    for shift in [6, 3, 0] {
        out.push(b'0' + ((mode >> shift) & 7) as u8);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use alloc::vec;

    /// An entry made is the entry read back, for every field at its bounds: index 0 and
    /// u32::MAX, mode 0 and 0777, bytes holding NULs, an empty file.
    #[test]
    fn entries_round_trip() {
        for member in [0, 7, u32::MAX] {
            let e = env(member, b"OPENAI_API_KEY=sk-a=b").unwrap();
            let got = parse_env(e.strip_prefix(ENV_KEY).unwrap()).unwrap();
            assert_eq!(got.member, member);
            assert_eq!(got.variable, b"OPENAI_API_KEY=sk-a=b");
            assert_eq!(got.key(), b"OPENAI_API_KEY");
            for (mode, bytes) in [(0, &b""[..]), (0o600, b"{\"t\":\0\"x\"}"), (0o777, b"\0\0")] {
                let f = file(member, mode, b".codex/auth.json", bytes).unwrap();
                let got = parse_file(f.strip_prefix(FILE_KEY).unwrap()).unwrap();
                assert_eq!(
                    (got.member, got.mode, got.path, got.bytes),
                    (member, mode, &b".codex/auth.json"[..], bytes)
                );
            }
        }
    }

    /// Paths beneath the home alone: absolute, `..`, `.`, empty components, NUL and past
    /// Linux's bounds are refused, before an entry is made and as one is read.
    #[test]
    fn paths_beneath_the_home_alone() {
        for bad in [
            &b""[..],
            b"/etc/passwd",
            b"../x",
            b"a/../../x",
            b"a/./b",
            b".",
            b"a//b",
            b"a/",
            b"a\0b",
        ] {
            assert!(!path_ok(bad), "{bad:?}");
            assert_eq!(file(0, 0o600, bad, b"x"), Err(Error::Path), "{bad:?}");
        }
        let long_name = vec![b'n'; NAME_MOST + 1];
        assert!(!path_ok(&long_name));
        assert!(path_ok(&long_name[..NAME_MOST]));
        let mut long_path = b"a/".repeat(PATH_MOST / 2);
        long_path.push(b'a');
        assert!(!path_ok(&long_path));
        // Read back: a crafted entry with an escaping path is refused too.
        assert_eq!(parse_file(b"0\x00600\x00../x\x00bytes"), Err(Error::Path));
        assert!(path_ok(b".claude/.credentials.json"));
    }

    /// No set-ID, no sticky bit, no mode past the permission bits, nor any spelled
    /// otherwise than octal.
    #[test]
    fn modes_are_permission_bits_alone() {
        for mode in [0o4755, 0o2755, 0o1777, 0o10000, u32::MAX] {
            assert_eq!(file(0, mode, b"x", b""), Err(Error::Mode), "{mode:o}");
        }
        for bad in [&b"4755"[..], b"", b"8", b"0x1", b"77777", b"-1"] {
            let entry = [&b"0\0"[..], bad, b"\0x\0"].concat();
            assert_eq!(parse_file(&entry), Err(Error::Mode), "{bad:?}");
        }
    }

    /// A variable is KEY=VALUE with a key, no NUL, never init's SHARDS_DOMAIN.
    #[test]
    fn variables_are_checked() {
        assert_eq!(env(0, b"NOEQUALS"), Err(Error::Variable));
        assert_eq!(env(0, b"=value"), Err(Error::Variable));
        assert_eq!(env(0, b"A=b\0c"), Err(Error::Variable));
        assert_eq!(env(0, b"SHARDS_DOMAIN=agent x"), Err(Error::Reserved));
        assert_eq!(parse_env(b"0\0SHARDS_DOMAIN=agent x"), Err(Error::Reserved));
        assert!(env(0, b"SHARDS_DOMAINS=fine").is_ok());
        assert!(env(0, b"EMPTY=").is_ok());
    }

    /// An index has one spelling: no sign, no leading zero, no overflow, no other byte.
    #[test]
    fn indexes_have_one_spelling() {
        for bad in [&b""[..], b"01", b"+1", b"-1", b"4294967296", b"1a", b" 1"] {
            let entry = [bad, b"\0A=b"].concat();
            assert_eq!(parse_env(&entry), Err(Error::Index), "{bad:?}");
        }
        assert_eq!(parse_env(b"0\0A=b").unwrap().member, 0);
        assert_eq!(parse_env(b"4294967295\0A=b").unwrap().member, u32::MAX);
    }

    /// Entries missing a field are refused by shape.
    #[test]
    fn short_entries_are_refused() {
        assert_eq!(parse_env(b"0"), Err(Error::Shape));
        assert_eq!(parse_file(b"0\x00600"), Err(Error::Shape));
        assert_eq!(parse_file(b"0\x00600\x00path"), Err(Error::Shape));
    }

    /// A member's entries are its own, in order; the members staged into are listed once
    /// each, ascending, and a bad entry anywhere refuses the whole.
    #[test]
    fn a_members_entries_are_its_own() {
        let setup = vec![
            env(2, b"A=1").unwrap(),
            b"readonly".to_vec(),
            file(0, 0o600, b"f", b"zero").unwrap(),
            env(2, b"B=2").unwrap(),
            env(0, b"C=3").unwrap(),
        ];
        let two: Vec<&[u8]> = envs_of(&setup, 2).unwrap().iter().map(|e| e.variable).collect();
        assert_eq!(two, [&b"A=1"[..], b"B=2"]);
        assert_eq!(files_of(&setup, 0).unwrap().len(), 1);
        assert!(files_of(&setup, 2).unwrap().is_empty());
        assert_eq!(staged_members(&setup).unwrap(), [0, 2]);
        let mut bad = setup.clone();
        bad.push([ENV_KEY, b"1\0SHARDS_DOMAIN=x"].concat());
        assert_eq!(staged_members(&bad), Err(Error::Reserved));
        assert_eq!(envs_of(&bad, 2), Err(Error::Reserved));
    }

    /// A staged file's bytes never reach Debug.
    #[test]
    fn a_files_bytes_are_not_printed() {
        let f = file(1, 0o600, b"auth.json", b"sk-secret-token").unwrap();
        let parsed = parse_file(f.strip_prefix(FILE_KEY).unwrap()).unwrap();
        let shown = alloc::format!("{parsed:?}");
        assert!(!shown.contains("secret"), "{shown}");
        assert!(shown.contains("bytes: 15"), "{shown}");
    }

    /// A member's end round-trips, and any other length is no end.
    #[test]
    fn ends_round_trip() {
        for ended in [
            Ended { member: 0, status: 0 },
            Ended {
                member: 3,
                status: 137,
            },
            Ended {
                member: u32::MAX,
                status: u32::MAX,
            },
        ] {
            assert_eq!(Ended::decode(&ended.encode()), Some(ended));
        }
        assert_eq!(Ended::decode(&[0; 7]), None);
        assert_eq!(Ended::decode(&[0; 9]), None);
    }
}
