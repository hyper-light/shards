//! Users and environments as Docker resolves them for a container: `--user` as
//! moby/sys/user v0.4.1 resolves it (user/user.go, GetExecUser), groups as moby lists them
//! (daemon/oci_linux.go, getUser), and the environment as runc v1.5.2 prepares it
//! (libcontainer/env.go, prepareEnv). The tests hold this to moby/sys/user's own cases.

/// The user a workload runs as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecUser {
    pub uid: u32,
    pub gid: u32,
    /// Supplementary groups: the primary group, then the rest, as moby lists them.
    pub groups: Vec<u32>,
}

/// GetExecUser's result, before moby's conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Resolved {
    uid: i64,
    gid: i64,
    sgids: Vec<i64>,
    home: Vec<u8>,
}

/// moby/sys/user's ID range: 0 to 2^31 - 1.
const MAX_ID: i64 = (1 << 31) - 1;
/// bufio.Scanner's longest line: 64 KiB by default, which /etc/passwd gets; /etc/group
/// gets 1 MiB.
const PASSWD_LINE: usize = 64 * 1024;
const GROUP_LINE: usize = 1024 * 1024;

/// Resolves Docker's `--user` against the image's /etc/passwd and /etc/group, either of
/// which may be missing.
pub fn resolve(spec: &[u8], passwd: Option<&[u8]>, group: Option<&[u8]>) -> Result<ExecUser, String> {
    let defaults = Resolved {
        uid: 0,
        gid: 0,
        sgids: Vec::new(),
        home: Vec::new(),
    };
    let r = get_exec_user(spec, &defaults, passwd, group)?;
    // Go's uint32 conversion.
    let gid = r.gid as u32;
    let mut groups = vec![gid];
    groups.extend(r.sgids.iter().map(|&g| g as u32));
    Ok(ExecUser {
        uid: r.uid as u32,
        gid,
        groups,
    })
}

/// The largest /etc/passwd or /etc/group BuildKit reads (executor/oci/user.go,
/// maxUserFileBytes): a larger one is an error.
pub const BUILDKIT_USER_FILE: usize = 10 << 20;

/// The user a BuildKit step runs as (dockerfile/1.27.1 executor/oci/user.go, GetUser and
/// WithUIDGID): no user is `0`, whose lookup cannot fail; `uid:gid`, each a decimal
/// number or `root`, is taken as it is, the files unread, with no supplementary groups;
/// anything else is moby's GetExecUser with no defaults. The primary group is always
/// among the groups, first if it was missing.
pub fn buildkit(spec: &[u8], passwd: Option<&[u8]>, group: Option<&[u8]>) -> Result<ExecUser, String> {
    let (spec, default) = if spec.is_empty() {
        (&b"0"[..], true)
    } else {
        (spec, false)
    };
    let (uid, gid, sgids) = match fast_uid_gid(spec) {
        Some((uid, gid)) => (uid, gid, Vec::new()),
        None => {
            let defaults = Resolved {
                uid: 0,
                gid: 0,
                sgids: Vec::new(),
                home: Vec::new(),
            };
            match get_exec_user(spec, &defaults, passwd, group) {
                // Go's uint32 conversions.
                Ok(r) => (
                    r.uid as u32,
                    r.gid as u32,
                    r.sgids.iter().map(|&g| g as u32).collect(),
                ),
                Err(_) if default => (0, 0, Vec::new()),
                Err(e) => return Err(e),
            }
        }
    };
    let mut groups: Vec<u32> = sgids;
    if !groups.contains(&gid) {
        groups.insert(0, gid);
    }
    Ok(ExecUser { uid, gid, groups })
}

/// ParseUIDGID: both parts given, each `root` or what strconv.ParseUint(s, 10, 32) takes.
fn fast_uid_gid(spec: &[u8]) -> Option<(u32, u32)> {
    let colon = spec.iter().position(|&b| b == b':')?;
    let (u, g) = (spec.get(..colon)?, spec.get(colon + 1..)?);
    let id = |s: &[u8]| -> Option<u32> {
        if s == b"root" {
            return Some(0);
        }
        if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
            return None;
        }
        s.iter()
            .try_fold(0u32, |n, &d| n.checked_mul(10)?.checked_add(u32::from(d - b'0')))
    };
    Some((id(u)?, id(g)?))
}

/// moby/sys/user's GetAdditionalGroups (user.go): each named group's GID from
/// /etc/group, the first that matches; a number as it is where none matches; a name that
/// matches none an error. Each GID once, ascending (moby keeps them in a map).
pub fn additional_groups(args: &[Vec<u8>], group: Option<&[u8]>) -> Result<Vec<u32>, String> {
    let mut parsed = Vec::new();
    for a in args {
        let (gid, numeric) = if a.is_empty() {
            (0, false)
        } else {
            parse_numeric(a)?
        };
        parsed.push((a.as_slice(), gid, numeric));
    }
    let groups = match group {
        Some(data) => parse_group(data).map_err(|e| {
            let shown: Vec<String> = args
                .iter()
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect();
            format!("unable to find additional groups [{}]: {e}", shown.join(" "))
        })?,
        None => Vec::new(),
    };
    let mut gids = std::collections::BTreeSet::new();
    for (name, gid, numeric) in parsed {
        let found = groups
            .iter()
            .find(|g| if numeric { g.gid == gid } else { g.name == name });
        match found {
            Some(g) if !(0..=MAX_ID).contains(&g.gid) => {
                return Err(format!("uids and gids must be in range 0-{MAX_ID}"));
            }
            Some(g) => {
                gids.insert(g.gid as u32);
            }
            None if numeric => {
                gids.insert(gid as u32);
            }
            None => {
                return Err(format!(
                    "unable to find group {}: no matching entries in group file",
                    String::from_utf8_lossy(name)
                ));
            }
        }
    }
    Ok(gids.into_iter().collect())
}

/// runc's prepareEnv: the last value of each variable wins, in first-seen order; an empty
/// HOME is dropped, and a missing one comes from /etc/passwd for `uid`, or is `/`.
pub fn prepare_env(env: &[Vec<u8>], uid: u32, passwd: Option<&[u8]>) -> Result<Vec<Vec<u8>>, String> {
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(env.len() + 1);
    let mut seen: std::collections::HashSet<&[u8]> = std::collections::HashSet::new();
    let mut home_set = false;
    for kv in env.iter().rev() {
        let eq = kv
            .iter()
            .position(|&b| b == b'=')
            .ok_or("invalid environment variable: missing '='")?;
        if eq == 0 {
            return Err("invalid environment variable: name cannot be empty".into());
        }
        let (key, value) = (
            kv.get(..eq).unwrap_or_default(),
            kv.get(eq + 1..).unwrap_or_default(),
        );
        if !seen.insert(key) {
            continue;
        }
        if kv.contains(&0) {
            return Err(format!(
                "invalid environment variable {:?}: contains nul byte (\\x00)",
                String::from_utf8_lossy(key)
            ));
        }
        if key == b"HOME" {
            if value.is_empty() {
                continue;
            }
            home_set = true;
        }
        out.push(kv.clone());
    }
    out.reverse();
    if !home_set {
        // runc's getUserHome: the first passwd entry with this uid, else `/`.
        let home = passwd
            .and_then(|p| parse_passwd(p).ok())
            .and_then(|users| users.into_iter().find(|u| u.uid == i64::from(uid)))
            .map_or(b"/".to_vec(), |u| u.home.to_vec());
        out.push([b"HOME=".as_slice(), &home].concat());
    }
    Ok(out)
}

struct Passwd<'a> {
    name: &'a [u8],
    uid: i64,
    gid: i64,
    home: &'a [u8],
}

struct Group<'a> {
    name: &'a [u8],
    gid: i64,
    members: Vec<&'a [u8]>,
}

/// GetExecUser: `user[:group]`, each a name or a number, looked up in the files, with
/// `defaults` for whatever the spec and the files leave out.
fn get_exec_user(
    spec: &[u8],
    defaults: &Resolved,
    passwd: Option<&[u8]>,
    group: Option<&[u8]>,
) -> Result<Resolved, String> {
    let mut user = defaults.clone();
    let mut parts = spec.split(|&b| b == b':');
    let user_arg = parts.next().unwrap_or_default();
    let group_arg = parts.next().unwrap_or_default();
    let (uid_arg, is_uid) = parse_numeric(user_arg)?;
    let (gid_arg, is_gid) = parse_numeric(group_arg)?;
    let show = |b: &[u8]| String::from_utf8_lossy(b).into_owned();

    let users = match passwd.map(parse_passwd) {
        Some(Err(e)) => {
            let who = if user_arg.is_empty() {
                user.uid.to_string()
            } else {
                show(user_arg)
            };
            return Err(format!("unable to find user {who}: {e}"));
        }
        Some(Ok(users)) => users,
        None => Vec::new(),
    };
    let found = users.iter().find(|u| {
        if user_arg.is_empty() {
            u.uid == user.uid
        } else if is_uid {
            u.uid == uid_arg
        } else {
            u.name == user_arg
        }
    });
    let mut matched_name: &[u8] = &[];
    if let Some(u) = found {
        matched_name = u.name;
        user.uid = u.uid;
        user.gid = u.gid;
        user.home = u.home.to_vec();
    } else if !user_arg.is_empty() {
        if !is_uid {
            return Err(format!(
                "unable to find user {}: no matching entries in passwd file",
                show(user_arg)
            ));
        }
        user.uid = uid_arg;
    }

    if !group_arg.is_empty() || !matched_name.is_empty() {
        let groups = match group.map(parse_group) {
            Some(Err(e)) => {
                return Err(format!(
                    "unable to find groups for spec {}: {e}",
                    show(matched_name)
                ));
            }
            Some(Ok(groups)) => groups,
            None => Vec::new(),
        };
        let wanted = |g: &&Group<'_>| {
            if group_arg.is_empty() {
                g.members.contains(&matched_name)
            } else if is_gid {
                g.gid == gid_arg
            } else {
                g.name == group_arg
            }
        };
        if !group_arg.is_empty() {
            match groups.iter().find(wanted) {
                Some(g) => user.gid = g.gid,
                None if is_gid => user.gid = gid_arg,
                None => {
                    return Err(format!(
                        "unable to find group {}: no matching entries in group file",
                        show(group_arg)
                    ));
                }
            }
        } else {
            let sgids: Vec<i64> = groups.iter().filter(wanted).map(|g| g.gid).collect();
            if !sgids.is_empty() {
                user.sgids = sgids;
            }
        }
    }
    Ok(user)
}

/// moby/sys/user's parseNumeric: empty or not a number is a name; a number must be an
/// ID in range.
fn parse_numeric(v: &[u8]) -> Result<(i64, bool), String> {
    match atoi(v) {
        Err(Atoi::Syntax) => Ok((0, false)),
        Ok(id) if (0..=MAX_ID).contains(&id) => Ok((id, true)),
        _ => Err(format!("uids and gids must be in range 0-{MAX_ID}")),
    }
}

enum Atoi {
    Syntax,
    /// Go returns the clamped value with the error.
    Range(i64),
}

/// Go's strconv.Atoi on a 64-bit target: an optional sign, then decimal digits.
fn atoi(v: &[u8]) -> Result<i64, Atoi> {
    let (negative, digits) = match v {
        [b'-', rest @ ..] => (true, rest),
        [b'+', rest @ ..] => (false, rest),
        _ => (false, v),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(Atoi::Syntax);
    }
    let mut n: i64 = 0;
    for &d in digits {
        let d = i64::from(d - b'0');
        let next = n.checked_mul(10).and_then(|n| {
            if negative {
                n.checked_sub(d)
            } else {
                n.checked_add(d)
            }
        });
        n = next.ok_or(Atoi::Range(if negative { i64::MIN } else { i64::MAX }))?;
    }
    Ok(n)
}

/// A numeric field as parseLine reads it: Atoi's value, errors and all.
fn field_number(v: &[u8]) -> i64 {
    match atoi(v) {
        Ok(n) | Err(Atoi::Range(n)) => n,
        Err(Atoi::Syntax) => 0,
    }
}

/// bufio.Scanner's lines: split at `\n`, one trailing `\r` dropped. A line that fills
/// the scanner's `max`-byte buffer is an error.
fn lines(data: &[u8], max: usize) -> Result<Vec<&[u8]>, String> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let (line, next) = match rest.iter().position(|&b| b == b'\n') {
            Some(i) => (
                rest.get(..i).unwrap_or_default(),
                rest.get(i + 1..).unwrap_or_default(),
            ),
            None => (rest, &[][..]),
        };
        if line.len() >= max {
            return Err("bufio.Scanner: token too long".into());
        }
        out.push(line.strip_suffix(b"\r").unwrap_or(line));
        rest = next;
    }
    Ok(out)
}

/// Go's bytes.TrimSpace: Unicode white space off both ends.
fn trim_space(mut b: &[u8]) -> &[u8] {
    loop {
        let before = b.len();
        // Go's ASCII white space, which includes \v.
        let space = |c: &u8| matches!(c, b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | b' ');
        while let Some((first, rest)) = b.split_first()
            && space(first)
        {
            b = rest;
        }
        while let Some((last, rest)) = b.split_last()
            && space(last)
        {
            b = rest;
        }
        if let Some(n) = leading_space(b) {
            b = b.get(n..).unwrap_or_default();
        }
        if let Some(n) = trailing_space(b) {
            b = b.get(..b.len() - n).unwrap_or_default();
        }
        if b.len() == before {
            return b;
        }
    }
}

fn is_space_char(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok_and(|s| s.chars().all(char::is_whitespace))
}

fn leading_space(b: &[u8]) -> Option<usize> {
    (2..=4).find(|&n| b.get(..n).is_some_and(is_space_char))
}

fn trailing_space(b: &[u8]) -> Option<usize> {
    (2..=4).find(|&n| b.len() >= n && b.get(b.len() - n..).is_some_and(is_space_char))
}

fn parse_passwd(data: &[u8]) -> Result<Vec<Passwd<'_>>, String> {
    let mut users = Vec::new();
    for line in lines(data, PASSWD_LINE)? {
        let line = trim_space(line);
        if line.is_empty() {
            continue;
        }
        let mut f = line.split(|&b| b == b':');
        let (name, _pass) = (f.next().unwrap_or_default(), f.next());
        let uid = f.next().map_or(0, field_number);
        let gid = f.next().map_or(0, field_number);
        let (_gecos, home) = (f.next(), f.next().unwrap_or_default());
        users.push(Passwd { name, uid, gid, home });
    }
    Ok(users)
}

fn parse_group(data: &[u8]) -> Result<Vec<Group<'_>>, String> {
    let mut groups = Vec::new();
    for line in lines(data, GROUP_LINE)? {
        let line = trim_space(line);
        if line.is_empty() || line.first() == Some(&b'#') {
            continue;
        }
        let mut f = line.split(|&b| b == b':');
        let (name, _pass) = (f.next().unwrap_or_default(), f.next());
        let gid = f.next().map_or(0, field_number);
        let members = match f.next() {
            Some(list) if !list.is_empty() => list.split(|&b| b == b',').collect(),
            _ => Vec::new(),
        };
        groups.push(Group { name, gid, members });
    }
    Ok(groups)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    /// moby/sys/user's GetAdditionalGroups cases (user_test.go).
    #[test]
    fn additional_groups_are_found_as_moby_finds_them() {
        let group = b"root:x:0:\nadm:x:4343:root,adm-duplicate\nadm-duplicate:x:4344:\n";
        let groups = |args: &[&str]| {
            let args: Vec<Vec<u8>> = args.iter().map(|a| a.as_bytes().to_vec()).collect();
            additional_groups(&args, Some(group))
        };
        assert_eq!(groups(&["adm"]), Ok(vec![4343]));
        assert_eq!(groups(&["adm", "adm-duplicate"]), Ok(vec![4343, 4344]));
        assert_eq!(groups(&["4343", "9999"]), Ok(vec![4343, 9999]));
        assert_eq!(
            groups(&["nope"]),
            Err("unable to find group nope: no matching entries in group file".into())
        );
        assert_eq!(additional_groups(&[b"12".to_vec()], None), Ok(vec![12]));
    }

    /// BuildKit's rules, beside Docker's (executor/oci/user.go).
    #[test]
    fn buildkit_resolves_a_steps_user_as_buildkit_does() {
        let passwd = Some(&b"root:x:0:0:root:/root:/bin/sh\napp:x:1000:1001:app:/home/app:/bin/sh\n"[..]);
        let group = Some(&b"root:x:0:\napp:x:1001:\ndev:x:50:app\nwheel:x:10:other\n"[..]);
        let user = |spec: &[u8], p, g| buildkit(spec, p, g).map(|u| (u.uid, u.gid, u.groups));
        // No user: root, even with no files and a lookup that cannot work.
        assert_eq!(user(b"", None, None), Ok((0, 0, vec![0])));
        // Both numbers: taken as they are, the files unread, no other groups.
        assert_eq!(user(b"1000:7", passwd, group), Ok((1000, 7, vec![7])));
        assert_eq!(user(b"root:root", None, None), Ok((0, 0, vec![0])));
        assert_eq!(user(b"4294967295:0", None, None), Ok((u32::MAX, 0, vec![0])));
        // A number alone is looked up: its primary group from passwd, its groups by name.
        assert_eq!(user(b"1000", passwd, group), Ok((1000, 1001, vec![1001, 50])));
        assert_eq!(user(b"app", passwd, group), Ok((1000, 1001, vec![1001, 50])));
        // A number with no entry: gid 0.
        assert_eq!(user(b"4242", passwd, group), Ok((4242, 0, vec![0])));
        // An explicit group gives no supplementary groups.
        assert_eq!(user(b"app:dev", passwd, group), Ok((1000, 50, vec![50])));
        // A name not found is an error, worded as moby words it.
        assert!(
            user(b"nobody", passwd, group)
                .unwrap_err()
                .contains("unable to find user nobody")
        );
        // A number past u32 is no fast path, and past moby's range an error.
        assert!(user(b"4294967296:0", passwd, group).is_err());
    }

    const PASSWD: &str = "
root:x:0:0:root user:/root:/bin/bash
adm:x:42:43:adm:/var/adm:/bin/false
111:x:222:333::/var/garbage
odd:x:111:112::/home/odd:::::
2147483647:x:0:0:maxint32:/root:/bin/bash
2147483648:x:0:0:toolarge:/root:/bin/bash
9223372036854775807:x:0:0:maxint64:/root:/bin/bash
user7456:x:7456:100:Vasya:/home/user7456
this is just some garbage data
";

    fn group_content() -> String {
        let mut large = String::from("largegroup:x:1000:user1");
        for i in 2..=7500 {
            large.push_str(&format!(",user{i}"));
        }
        "
root:x:0:root
adm:x:43:
grp:x:1234:root,adm,user7456
444:x:555:111
odd:x:444:
2147483647:x:1235:
2147483648:x:1236:
9223372036854775807:x:1237:
this is just some garbage data
"
        .to_string()
            + &large
    }

    fn defaults() -> Resolved {
        Resolved {
            uid: 8888,
            gid: 8888,
            sgids: vec![8888],
            home: b"/8888".to_vec(),
        }
    }

    fn resolved(uid: i64, gid: i64, sgids: &[i64], home: &str) -> Resolved {
        Resolved {
            uid,
            gid,
            sgids: sgids.to_vec(),
            home: home.as_bytes().to_vec(),
        }
    }

    /// moby/sys/user v0.4.1 user_test.go, TestGetExecUser.
    #[test]
    fn get_exec_user_matches_moby() {
        let group = group_content();
        let d = &[8888];
        for (spec, want) in [
            ("root", resolved(0, 0, &[0, 1234], "/root")),
            ("adm", resolved(42, 43, &[1234], "/var/adm")),
            ("root:adm", resolved(0, 43, d, "/root")),
            ("adm:1234", resolved(42, 1234, d, "/var/adm")),
            ("42:1234", resolved(42, 1234, d, "/var/adm")),
            ("1337:1234", resolved(1337, 1234, d, "/8888")),
            ("1337", resolved(1337, 8888, d, "/8888")),
            ("", resolved(8888, 8888, d, "/8888")),
            ("111", resolved(111, 112, d, "/home/odd")),
            ("111:444", resolved(111, 444, d, "/home/odd")),
            ("7456", resolved(7456, 100, &[1234, 1000], "/home/user7456")),
            ("7456:2147483647", resolved(7456, 2147483647, d, "/home/user7456")),
            ("2147483647:43", resolved(2147483647, 43, d, "/8888")),
            ("2147483647", resolved(2147483647, 8888, d, "/8888")),
        ] {
            let got = get_exec_user(
                spec.as_bytes(),
                &defaults(),
                Some(PASSWD.as_bytes()),
                Some(group.as_bytes()),
            );
            assert_eq!(got, Ok(want), "{spec:?}");
        }
    }

    /// TestGetExecUserInvalid.
    #[test]
    fn invalid_specs_fail_as_in_moby() {
        let passwd = "
root:x:0:0:root user:/root:/bin/bash
adm:x:42:43:adm:/var/adm:/bin/false
-42:x:12:13:broken:/very/broken
2147483647:x:0:0:maxint32:/root:/bin/bash
2147483648:x:0:0:toolarge:/root:/bin/bash
9223372036854775807:x:0:0:maxint64:/root:/bin/bash
9223372036854775808:x:0:0:maxint64plusone:/root:/bin/bash
this is just some garbage data
";
        let group = "
root:x:0:root
adm:x:43:
grp:x:1234:root,adm
2147483647:x:1235:
2147483648:x:1236:
9223372036854775807:x:1237:
9223372036854775808:x:1238:
this is just some garbage data
";
        let zero = resolved(0, 0, &[], "");
        for spec in [
            "notuser",
            "notuser:notgroup",
            "root:notgroup",
            "notuser:adm",
            "8888:notgroup",
            "notuser:8888",
            "-1:0",
            "0:-3",
            "-5:-2",
            "-42",
            "-43",
            "42:2147483648",
            "2147483648:43",
            "2147483648",
            "7456:9223372036854775807",
            "9223372036854775807:43",
            "9223372036854775807",
            "9223372036854775808",
            "0:9223372036854775808",
        ] {
            let got = get_exec_user(
                spec.as_bytes(),
                &zero,
                Some(passwd.as_bytes()),
                Some(group.as_bytes()),
            );
            assert!(got.is_err(), "{spec:?} resolved to {got:?}");
        }
    }

    /// TestGetExecUserNilSources.
    #[test]
    fn missing_files_leave_the_defaults() {
        let passwd = "
root:x:0:0:root user:/root:/bin/bash
adm:x:42:43:adm:/var/adm:/bin/false
this is just some garbage data
";
        for (spec, has_passwd, want) in [
            ("", false, resolved(8888, 8888, &[8888], "/8888")),
            ("root", true, resolved(0, 0, &[8888], "/root")),
            ("0", false, resolved(0, 8888, &[8888], "/8888")),
            ("0:0", false, resolved(0, 0, &[8888], "/8888")),
        ] {
            let p = has_passwd.then_some(passwd.as_bytes());
            assert_eq!(
                get_exec_user(spec.as_bytes(), &defaults(), p, None),
                Ok(want),
                "{spec:?}"
            );
        }
    }

    #[test]
    fn moby_lists_the_primary_group_first() {
        let passwd = "app:x:1000:1000::/home/app:/bin/sh\n";
        let group = "app:x:1000:\nstaff:x:50:app\nwheel:x:10:app,root\n";
        let user = resolve(b"app", Some(passwd.as_bytes()), Some(group.as_bytes())).unwrap();
        assert_eq!(
            user,
            ExecUser {
                uid: 1000,
                gid: 1000,
                groups: vec![1000, 50, 10]
            }
        );
        let root = resolve(b"", None, None).unwrap();
        assert_eq!(
            root,
            ExecUser {
                uid: 0,
                gid: 0,
                groups: vec![0]
            }
        );
    }

    #[test]
    fn environments_are_prepared_as_runc_prepares_them() {
        let passwd = b"root:x:0:0::/root:/bin/sh\napp:x:1000:1000::/home/app:/bin/sh\n".as_slice();
        let env = |v: &[&str]| v.iter().map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>();
        assert_eq!(
            prepare_env(&env(&["A=1", "B=2", "A=3", "HOME="]), 1000, Some(passwd)).unwrap(),
            env(&["B=2", "A=3", "HOME=/home/app"])
        );
        assert_eq!(
            prepare_env(&env(&["HOME=/x"]), 0, Some(passwd)).unwrap(),
            env(&["HOME=/x"])
        );
        assert_eq!(
            prepare_env(&env(&[]), 4242, Some(passwd)).unwrap(),
            env(&["HOME=/"])
        );
        assert_eq!(prepare_env(&env(&[]), 0, None).unwrap(), env(&["HOME=/"]));
        for bad in [&["NOEQUALS"][..], &["=value"], &["A=\0"]] {
            assert!(prepare_env(&env(bad), 0, None).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn go_parsing_details() {
        assert_eq!(trim_space(b" \t\xc2\xa0root:x\xe3\x80\x80 \r"), b"root:x");
        assert_eq!(
            lines(b"a\r\nb\n\nc", 10).unwrap(),
            vec![&b"a"[..], b"b", b"", b"c"]
        );
        assert!(lines(&[b'x'; 10], 10).is_err());
        assert_eq!(lines(&[b'x'; 9], 10).unwrap().len(), 1);
        assert_eq!(trim_space(b"\x0bvt\x0b"), b"vt");
        assert!(matches!(atoi(b"+12"), Ok(12)));
        assert!(matches!(atoi(b"-12"), Ok(-12)));
        assert!(matches!(atoi(b"1_000"), Err(Atoi::Syntax)));
        assert!(matches!(
            atoi(b"99999999999999999999"),
            Err(Atoi::Range(i64::MAX))
        ));
        assert_eq!(field_number(b"x"), 0);
    }
}
