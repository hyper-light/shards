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
/// `member-workdir=<index>\0<path>`: the member's working directory, absolute and plain
/// ([`workdir_ok`]): the project path its run names, over its image's (its config's
/// `run.workdir`, in its directory). One for a member at most.
pub const WORKDIR_KEY: &[u8] = b"member-workdir=";
/// `image-env=KEY=VALUE`: one of the image's own variables (its config's `Env`, OCI
/// image-spec config.md), which every member starts with: over init's defaults, under its
/// own config's `run.env` and what is staged for it (init domains.rs `harness_env`).
pub const IMAGE_ENV_KEY: &[u8] = b"image-env=";
/// `member-ends`, alone: the run's owner hears each member's end
/// ([`kind::MEMBER_ENDED`](super::kind::MEMBER_ENDED)), which a daemon's run does not ask.
pub const ENDS: &[u8] = b"member-ends";
/// `member-terminal=<index>`: the member's stdio is a terminal of its own (plan step 6,
/// D244), whose master init holds: what the terminal shows is its stdout channel, its
/// stdin goes to it, and [`builtin::RESIZE`](super::builtin::RESIZE) sizes it. One for a
/// member at most.
pub const TERMINAL_KEY: &[u8] = b"member-terminal=";
/// `member-arg=<index>\0<argument>`: one of the member's run-time arguments (D256), on its
/// command line after its config's `run.command` (its startup descriptor's entrypoint and
/// options), in the order staged: what its run adds, as a harness's own flags. Any bytes but
/// a NUL; an empty one is an argument too.
pub const ARG_KEY: &[u8] = b"member-arg=";
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
    /// Its working directory is relative, holds an empty, `.` or `..` component or a NUL,
    /// or is past Linux's bounds.
    Workdir,
    /// A second working directory for one member.
    Twice,
    /// The entry lacks one of its fields.
    Shape,
    /// A second member for one exec ([`EXEC_KEY`]).
    SecondMember,
    /// A second terminal for one member ([`TERMINAL_KEY`]).
    SecondTerminal,
    /// A terminal's size that is not its rows and columns, each a decimal u16.
    Size,
    /// An argument holding a NUL, which no command line exec'd holds (execve(2)'s strings
    /// end at one).
    Argument,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Error::Index => "a member index that is not a decimal u32",
            Error::Variable => "a variable that is not KEY=VALUE without NUL",
            Error::Reserved => "SHARDS_DOMAIN, which init sets",
            Error::Mode => "a mode past the permission bits (0777)",
            Error::Path => "a path that is not relative, plain and within PATH_MAX",
            Error::Workdir => "a working directory that is not absolute, plain and within PATH_MAX",
            Error::Twice => "a second working directory for one member",
            Error::Shape => "an entry missing a field",
            Error::SecondMember => "a second member for one exec",
            Error::SecondTerminal => "a second terminal for one member",
            Error::Size => "a terminal size that is not its rows and columns in decimal u16s",
            Error::Argument => "an argument holding a NUL",
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

/// Whether `path` names a directory a member may work in, and nothing else: absolute, each
/// component past `/` nonempty and neither `.` nor `..` (so nothing it names leaves where
/// it is written to be), no NUL, within Linux's bounds. `/` alone is one.
pub fn workdir_ok(path: &[u8]) -> bool {
    match path.strip_prefix(b"/") {
        Some(b"") => true,
        Some(rest) => path.len() < PATH_MOST && path_ok(rest),
        None => false,
    }
}

/// The setup entry giving member `member` the working directory `path`, refused where it is
/// not one a member may work in ([`workdir_ok`]).
pub fn workdir(member: u32, path: &[u8]) -> Result<Vec<u8>, Error> {
    if !workdir_ok(path) {
        return Err(Error::Workdir);
    }
    let mut entry = Vec::new();
    entry.extend_from_slice(WORKDIR_KEY);
    push_decimal(&mut entry, member);
    entry.push(0);
    entry.extend_from_slice(path);
    Ok(entry)
}

/// A `member-workdir=` entry's value, checked: its member and its path.
pub fn parse_workdir(value: &[u8]) -> Result<(u32, &[u8]), Error> {
    let (index, path) = split_once(value, 0).ok_or(Error::Shape)?;
    if !workdir_ok(path) {
        return Err(Error::Workdir);
    }
    Ok((decimal(index)?, path))
}

/// Member `member`'s working directory, where `setup` gives it one; every entry is checked,
/// and a second for one member refuses the whole.
pub fn workdir_of(setup: &[Vec<u8>], member: u32) -> Result<Option<&[u8]>, Error> {
    let mut seen = alloc::collections::BTreeSet::new();
    let mut found = None;
    for value in setup.iter().filter_map(|e| e.strip_prefix(WORKDIR_KEY)) {
        let (m, path) = parse_workdir(value)?;
        if !seen.insert(m) {
            return Err(Error::Twice);
        }
        if m == member {
            found = Some(path);
        }
    }
    Ok(found)
}

/// `arg` as a run-time argument may be (D256): any bytes but a NUL.
pub fn arg_ok(arg: &[u8]) -> Result<(), Error> {
    if arg.contains(&0) {
        return Err(Error::Argument);
    }
    Ok(())
}

/// The setup entry giving member `member` the run-time argument `arg` (D256), refused as
/// [`arg_ok`] refuses it.
pub fn arg(member: u32, arg: &[u8]) -> Result<Vec<u8>, Error> {
    arg_ok(arg)?;
    let mut entry = Vec::new();
    entry.extend_from_slice(ARG_KEY);
    push_decimal(&mut entry, member);
    entry.push(0);
    entry.extend_from_slice(arg);
    Ok(entry)
}

/// A `member-arg=` entry's value, checked: its member and its argument.
pub fn parse_arg(value: &[u8]) -> Result<(u32, &[u8]), Error> {
    let (index, arg) = split_once(value, 0).ok_or(Error::Shape)?;
    arg_ok(arg)?;
    Ok((decimal(index)?, arg))
}

/// Member `member`'s run-time arguments, in the order `setup` stages them; every entry is
/// checked, and the first refused ends it with its error.
pub fn args_of(setup: &[Vec<u8>], member: u32) -> Result<Vec<&[u8]>, Error> {
    let mut out = Vec::new();
    for value in setup.iter().filter_map(|e| e.strip_prefix(ARG_KEY)) {
        let (m, arg) = parse_arg(value)?;
        if m == member {
            out.push(arg);
        }
    }
    Ok(out)
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

/// The setup entry giving every member the image's `variable`, refused as [`variable_ok`]
/// refuses it: an image's `ENV` may not name a member's domain.
pub fn image_env(variable: &[u8]) -> Result<Vec<u8>, Error> {
    variable_ok(variable)?;
    Ok([IMAGE_ENV_KEY, variable].concat())
}

/// Every `image-env=` entry of `setup`, in order, each checked; the first refused ends it
/// with its error.
pub fn image_envs(setup: &[Vec<u8>]) -> Result<Vec<&[u8]>, Error> {
    let mut out = Vec::new();
    for variable in setup.iter().filter_map(|e| e.strip_prefix(IMAGE_ENV_KEY)) {
        variable_ok(variable)?;
        out.push(variable);
    }
    Ok(out)
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
        } else if let Some(value) = entry.strip_prefix(WORKDIR_KEY) {
            parse_workdir(value)?.0
        } else if let Some(value) = entry.strip_prefix(ARG_KEY) {
            parse_arg(value)?.0
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

/// `in-member=<index>`: an exec's setup entry that runs its command in the member of that
/// declared index (plan step 6, D243): born in its PID namespace and cgroup, in the
/// namespaces of its command, and confined as its command is, with its environment under
/// the exec's own and its working directory where the exec names none. One at most.
pub const EXEC_KEY: &[u8] = b"in-member=";

/// The setup entry that runs an exec in member `member`.
pub fn exec_in(member: u32) -> Vec<u8> {
    let mut entry = Vec::new();
    entry.extend_from_slice(EXEC_KEY);
    push_decimal(&mut entry, member);
    entry
}

/// The member an exec's `setup` runs it in, where it names one: every `in-member=` entry
/// checked, and a second refusing the whole.
pub fn exec_member(setup: &[Vec<u8>]) -> Result<Option<u32>, Error> {
    let mut found = None;
    for value in setup.iter().filter_map(|e| e.strip_prefix(EXEC_KEY)) {
        if found.replace(decimal(value)?).is_some() {
            return Err(Error::SecondMember);
        }
    }
    Ok(found)
}

/// The setup entry giving member `member` a terminal of its own ([`TERMINAL_KEY`]).
pub fn terminal(member: u32) -> Vec<u8> {
    let mut entry = Vec::new();
    entry.extend_from_slice(TERMINAL_KEY);
    push_decimal(&mut entry, member);
    entry
}

/// The members `setup` gives a terminal of their own, in its order: every entry checked,
/// and a member named twice refusing the whole.
pub fn terminals(setup: &[Vec<u8>]) -> Result<Vec<u32>, Error> {
    let mut seen = alloc::collections::BTreeSet::new();
    let mut found = Vec::new();
    for value in setup.iter().filter_map(|e| e.strip_prefix(TERMINAL_KEY)) {
        let member = decimal(value)?;
        if !seen.insert(member) {
            return Err(Error::SecondTerminal);
        }
        found.push(member);
    }
    Ok(found)
}

/// [`builtin::RESIZE`](super::builtin::RESIZE)'s arguments: the member's index, then its
/// terminal's rows and columns, each in decimal.
pub fn resize_args(member: u32, size: super::Size) -> Vec<Vec<u8>> {
    [member, u32::from(size.rows), u32::from(size.cols)]
        .into_iter()
        .map(|n| {
            let mut arg = Vec::new();
            push_decimal(&mut arg, n);
            arg
        })
        .collect()
}

/// The member and the size [`resize_args`] gives, checked: three arguments, the index a
/// decimal u32, the rows and columns each a decimal u16.
pub fn parse_resize(args: &[Vec<u8>]) -> Result<(u32, super::Size), Error> {
    let [member, rows, cols] = args else {
        return Err(Error::Shape);
    };
    let cells = |arg: &[u8]| -> Result<u16, Error> {
        decimal(arg)
            .map_err(|_| Error::Size)
            .and_then(|n| u16::try_from(n).map_err(|_| Error::Size))
    };
    let size = super::Size {
        rows: cells(rows)?,
        cols: cells(cols)?,
    };
    Ok((decimal(member)?, size))
}

/// Why init ran no command in a member (plan step 6, D243), as its exec's
/// [`kind::SYSTEM_ERR`](super::kind::SYSTEM_ERR) says after the class
/// [`exec_failed::MEMBER`](super::exec_failed::MEMBER): a code, what that code carries, then
/// the words for a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// No member of that index started in this run.
    Unknown,
    /// It runs no process of its own: it has ended.
    Ended,
    /// Its cgroup runs as many processes as its `pids.max` lets it.
    Processes,
    /// The exec asks for what its member has not: a user, or capabilities, of its own.
    Widening,
    /// Its namespaces could not be entered, with the errno.
    Joining(i32),
    /// A step of its confinement failed: which (init's `domains::Confining`, by its
    /// number), and the errno.
    Confining(u8, i32),
}

impl Refusal {
    const UNKNOWN: u8 = 1;
    const ENDED: u8 = 2;
    const PROCESSES: u8 = 3;
    const WIDENING: u8 = 4;
    const JOINING: u8 = 5;
    const CONFINING: u8 = 6;

    /// Its code and what it carries, then `words`.
    pub fn encode(self, words: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(6 + words.len());
        match self {
            Refusal::Unknown => out.push(Self::UNKNOWN),
            Refusal::Ended => out.push(Self::ENDED),
            Refusal::Processes => out.push(Self::PROCESSES),
            Refusal::Widening => out.push(Self::WIDENING),
            Refusal::Joining(errno) => {
                out.push(Self::JOINING);
                out.extend_from_slice(&errno.to_be_bytes());
            }
            Refusal::Confining(step, errno) => {
                out.extend_from_slice(&[Self::CONFINING, step]);
                out.extend_from_slice(&errno.to_be_bytes());
            }
        }
        out.extend_from_slice(words);
        out
    }

    /// The refusal `bytes` begins with, and its words; none for a code no refusal has, or
    /// one cut short.
    pub fn decode(bytes: &[u8]) -> Option<(Refusal, &[u8])> {
        let (&code, rest) = bytes.split_first()?;
        let errno = |b: &[u8]| -> Option<i32> { Some(i32::from_be_bytes(*b.first_chunk::<4>()?)) };
        Some(match code {
            Self::UNKNOWN => (Refusal::Unknown, rest),
            Self::ENDED => (Refusal::Ended, rest),
            Self::PROCESSES => (Refusal::Processes, rest),
            Self::WIDENING => (Refusal::Widening, rest),
            Self::JOINING => (Refusal::Joining(errno(rest)?), rest.get(4..)?),
            Self::CONFINING => {
                let (&step, rest) = rest.split_first()?;
                (Refusal::Confining(step, errno(rest)?), rest.get(4..)?)
            }
            _ => return None,
        })
    }

    /// Whether init could not start what it would have run (`docker exec`'s 126, an OCI
    /// runtime's failure), not refused it (`docker exec`'s 1, the daemon's refusal).
    pub fn not_started(self) -> bool {
        matches!(
            self,
            Refusal::Processes | Refusal::Joining(_) | Refusal::Confining(..)
        )
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

    /// A member's working directory is absolute and plain, made and read alike, one for a
    /// member at most; it counts as staged into its member.
    #[test]
    fn a_working_directory_is_absolute_plain_and_one() {
        for good in [
            &b"/"[..],
            b"/Users/ada/proj",
            b"/home/claude/work.d",
            b"/a/.b/..c",
        ] {
            assert!(workdir_ok(good), "{good:?}");
            let entry = workdir(3, good).unwrap();
            assert_eq!(
                parse_workdir(entry.strip_prefix(WORKDIR_KEY).unwrap()),
                Ok((3, good))
            );
        }
        for bad in [
            &b""[..],
            b"proj",
            b"./proj",
            b"//x",
            b"/a//b",
            b"/a/",
            b"/a/./b",
            b"/a/../etc",
            b"/..",
            b"/a\0b",
        ] {
            assert!(!workdir_ok(bad), "{bad:?}");
            assert_eq!(workdir(0, bad), Err(Error::Workdir), "{bad:?}");
            let crafted = [&b"0\0"[..], bad].concat();
            assert_eq!(parse_workdir(&crafted), Err(Error::Workdir), "{bad:?}");
        }
        let long = [&b"/"[..], &b"a/".repeat(PATH_MOST / 2)[..], b"a"].concat();
        assert!(!workdir_ok(&long));
        let setup = vec![
            workdir(1, b"/Users/ada/one").unwrap(),
            env(2, b"A=b").unwrap(),
            workdir(2, b"/Users/ada/two").unwrap(),
        ];
        assert_eq!(workdir_of(&setup, 2), Ok(Some(&b"/Users/ada/two"[..])));
        assert_eq!(workdir_of(&setup, 0), Ok(None));
        assert_eq!(staged_members(&setup).unwrap(), [1, 2]);
        let mut twice = setup.clone();
        twice.push(workdir(1, b"/elsewhere").unwrap());
        assert_eq!(workdir_of(&twice, 2), Err(Error::Twice));
        assert_eq!(parse_workdir(b"0"), Err(Error::Shape));
        assert_eq!(parse_workdir(b"01\0/x"), Err(Error::Index));
    }

    /// A member's run-time arguments (D256) are made and read alike, any bytes but a NUL, an
    /// empty one among them; each is its member's, in the order staged, and stages into it.
    /// One holding a NUL is refused as it is made and as it is read, and refuses the whole.
    #[test]
    fn a_members_arguments_are_its_own_in_order() {
        for member in [0, 7, u32::MAX] {
            for a in [
                &b"--resume"[..],
                b"",
                b"--model=claude-opus",
                b"two words",
                b"a=b\tc",
                b"\xff\xfe",
            ] {
                let entry = arg(member, a).unwrap();
                assert_eq!(parse_arg(entry.strip_prefix(ARG_KEY).unwrap()), Ok((member, a)));
            }
        }
        assert_eq!(arg_ok(b"a\0b"), Err(Error::Argument));
        assert_eq!(arg(0, b"a\0b"), Err(Error::Argument));
        assert_eq!(parse_arg(b"0\0a\0b"), Err(Error::Argument));
        assert_eq!(parse_arg(b"0"), Err(Error::Shape));
        assert_eq!(parse_arg(b"01\0x"), Err(Error::Index));
        let setup = vec![
            arg(2, b"--resume").unwrap(),
            env(2, b"A=b").unwrap(),
            arg(1, b"--model").unwrap(),
            arg(2, b"").unwrap(),
            arg(2, b"session-1").unwrap(),
        ];
        assert_eq!(args_of(&setup, 2).unwrap(), [&b"--resume"[..], b"", b"session-1"]);
        assert_eq!(args_of(&setup, 1).unwrap(), [&b"--model"[..]]);
        assert!(args_of(&setup, 0).unwrap().is_empty());
        assert_eq!(staged_members(&setup).unwrap(), [1, 2]);
        assert_eq!(staged_members(&[arg(4, b"-p").unwrap()]).unwrap(), [4]);
        let mut bad = setup.clone();
        bad.push([ARG_KEY, b"3\0a\0b"].concat());
        assert_eq!(args_of(&bad, 2), Err(Error::Argument));
        assert_eq!(staged_members(&bad), Err(Error::Argument));
    }

    /// The image's variables reach every member in order, each checked as a staged one
    /// is, made and read alike: none names the domain or holds a NUL, and each is
    /// KEY=VALUE.
    #[test]
    fn the_images_variables_are_checked_as_staged_ones() {
        let setup = vec![
            image_env(b"PATH=/opt/bin:/usr/bin").unwrap(),
            env(0, b"A=staged").unwrap(),
            image_env(b"LANG=C.UTF-8").unwrap(),
        ];
        assert_eq!(
            image_envs(&setup).unwrap(),
            [&b"PATH=/opt/bin:/usr/bin"[..], b"LANG=C.UTF-8"]
        );
        assert_eq!(image_env(b"SHARDS_DOMAIN=agent x"), Err(Error::Reserved));
        assert_eq!(image_env(b"NOEQUALS"), Err(Error::Variable));
        assert_eq!(image_env(b"A=b\0c"), Err(Error::Variable));
        for crafted in [&b"SHARDS_DOMAIN=agent x"[..], b"=v", b"NOEQUALS"] {
            let setup = vec![[IMAGE_ENV_KEY, crafted].concat()];
            assert!(image_envs(&setup).is_err(), "{crafted:?}");
        }
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

    /// An exec names one member at most, in the index's one spelling; an exec naming none
    /// runs where it would.
    #[test]
    fn an_exec_names_one_member() {
        for member in [0, 7, u32::MAX] {
            assert_eq!(exec_member(&[exec_in(member)]), Ok(Some(member)));
        }
        assert_eq!(exec_member(&[b"caps=1".to_vec()]), Ok(None));
        assert_eq!(exec_member(&[]), Ok(None));
        assert_eq!(exec_member(&[exec_in(1), exec_in(1)]), Err(Error::SecondMember));
        for bad in [
            &b"in-member="[..],
            b"in-member=01",
            b"in-member=-1",
            b"in-member=4294967296",
        ] {
            assert_eq!(exec_member(&[bad.to_vec()]), Err(Error::Index), "{bad:?}");
        }
    }

    /// A member's terminal is asked once at most, in the index's one spelling; the setup's
    /// other entries are not terminals.
    #[test]
    fn a_member_has_one_terminal_at_most() {
        let setup = [terminal(2), b"member-ends".to_vec(), terminal(0), exec_in(1)];
        assert_eq!(terminals(&setup), Ok(vec![2, 0]));
        assert_eq!(terminals(&[]), Ok(Vec::new()));
        assert_eq!(terminals(&[terminal(1), terminal(1)]), Err(Error::SecondTerminal));
        for bad in [
            &b"member-terminal="[..],
            b"member-terminal=01",
            b"member-terminal=x",
        ] {
            assert_eq!(terminals(&[bad.to_vec()]), Err(Error::Index), "{bad:?}");
        }
    }

    /// A resize's arguments round-trip; a size past a u16, or not three arguments, is
    /// refused.
    #[test]
    fn a_resize_says_its_member_and_size() {
        let size = super::super::Size { rows: 40, cols: 120 };
        assert_eq!(parse_resize(&resize_args(3, size)), Ok((3, size)));
        let most = super::super::Size {
            rows: u16::MAX,
            cols: 0,
        };
        assert_eq!(parse_resize(&resize_args(u32::MAX, most)), Ok((u32::MAX, most)));
        let args = |a: &[&[u8]]| a.iter().map(|x| x.to_vec()).collect::<Vec<_>>();
        assert_eq!(parse_resize(&args(&[b"1", b"65536", b"80"])), Err(Error::Size));
        assert_eq!(parse_resize(&args(&[b"1", b"24", b"-1"])), Err(Error::Size));
        assert_eq!(parse_resize(&args(&[b"x", b"24", b"80"])), Err(Error::Index));
        assert_eq!(parse_resize(&args(&[b"1", b"24"])), Err(Error::Shape));
        assert_eq!(
            parse_resize(&args(&[b"1", b"24", b"80", b"9"])),
            Err(Error::Shape)
        );
    }

    /// Every refusal round-trips with its words; one cut short, or of a code no refusal
    /// has, is none.
    #[test]
    fn refusals_round_trip() {
        for r in [
            Refusal::Unknown,
            Refusal::Ended,
            Refusal::Processes,
            Refusal::Widening,
            Refusal::Joining(1),
            Refusal::Joining(i32::MIN),
            Refusal::Confining(13, 22),
            Refusal::Confining(0, -1),
        ] {
            for words in [&b""[..], b"agent main: it has ended", b"\0\xff"] {
                let said = r.encode(words);
                assert_eq!(Refusal::decode(&said), Some((r, words)), "{r:?}");
                // Cut anywhere within what the code carries, it is none.
                let carried = said.len() - words.len();
                for cut in 0..carried {
                    assert_eq!(Refusal::decode(&said[..cut]), None, "{r:?} cut at {cut}");
                }
            }
        }
        assert_eq!(Refusal::decode(&[0]), None);
        assert_eq!(Refusal::decode(&[7, b'x']), None);
        assert!(!Refusal::Unknown.not_started() && !Refusal::Widening.not_started());
        assert!(Refusal::Processes.not_started() && Refusal::Confining(1, 1).not_started());
    }
}
