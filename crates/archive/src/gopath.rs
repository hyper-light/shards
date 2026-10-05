//! Go's path and path/filepath on byte strings (go1.26.1 src/path/path.go,
//! src/internal/filepathlite/path.go, path_windows.go, src/path/filepath/path.go,
//! path_windows.go): go-archive decides names and copy destinations with them, so shards
//! cleans, splits and joins as they do. Windows' rules take an [`Os`], so they are tested
//! on every host.

/// Whose filepath rules apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Os {
    Unix,
    Windows,
}

/// This host's.
#[cfg(not(windows))]
pub(crate) const NATIVE: Os = Os::Unix;
#[cfg(windows)]
pub(crate) const NATIVE: Os = Os::Windows;

/// Go's path package, always `/`.
pub(crate) mod posix {
    /// path.Clean.
    pub(crate) fn clean(p: &[u8]) -> Vec<u8> {
        super::clean(super::Os::Unix, p)
    }

    /// path.Split: everything to the last slash, and the rest.
    pub(crate) fn split(p: &[u8]) -> (&[u8], &[u8]) {
        super::split(super::Os::Unix, p)
    }

    /// path.Join.
    pub(crate) fn join(parts: &[&[u8]]) -> Vec<u8> {
        super::join(super::Os::Unix, parts)
    }
}

impl Os {
    pub(crate) fn sep(self) -> u8 {
        match self {
            Os::Unix => b'/',
            Os::Windows => b'\\',
        }
    }

    pub(crate) fn is_sep(self, c: u8) -> bool {
        match self {
            Os::Unix => c == b'/',
            Os::Windows => c == b'/' || c == b'\\',
        }
    }
}

fn at(p: &[u8], i: usize) -> u8 {
    p.get(i).copied().unwrap_or(0)
}

/// filepathlite's volumeNameLen: a drive letter, UNC share, or `\\?\`-style device path
/// on Windows; nothing on Unix.
pub(crate) fn volume_name_len(os: Os, p: &[u8]) -> usize {
    if os == Os::Unix {
        return 0;
    }
    if p.len() >= 2 && at(p, 1) == b':' {
        return 2;
    }
    if p.is_empty() || !os.is_sep(at(p, 0)) {
        return 0;
    }
    if has_prefix_fold(p, br"\\.\UNC") {
        return unc_len(p, br"\\.\UNC\".len());
    }
    if has_prefix_fold(p, br"\\.") || has_prefix_fold(p, br"\\?") || has_prefix_fold(p, br"\??") {
        if p.len() == 3 {
            return 3;
        }
        let rest = p.get(4..).unwrap_or_default();
        return match rest.iter().position(|&c| os.is_sep(c)) {
            None => p.len(),
            Some(i) => p.len() - (rest.len() - i - 1) - 1,
        };
    }
    if p.len() >= 2 && os.is_sep(at(p, 1)) {
        return unc_len(p, 2);
    }
    0
}

fn has_prefix_fold(s: &[u8], prefix: &[u8]) -> bool {
    if s.len() < prefix.len() {
        return false;
    }
    for (i, &pc) in prefix.iter().enumerate() {
        let sc = at(s, i);
        if Os::Windows.is_sep(pc) {
            if !Os::Windows.is_sep(sc) {
                return false;
            }
        } else if !pc.eq_ignore_ascii_case(&sc) {
            return false;
        }
    }
    !(s.len() > prefix.len() && !Os::Windows.is_sep(at(s, prefix.len())))
}

fn unc_len(p: &[u8], prefix: usize) -> usize {
    let mut count = 0;
    for (i, &c) in p.iter().enumerate().skip(prefix) {
        if Os::Windows.is_sep(c) {
            count += 1;
            if count == 2 {
                return i;
            }
        }
    }
    p.len()
}

/// FromSlash.
pub(crate) fn from_slash(os: Os, p: &[u8]) -> Vec<u8> {
    match os {
        Os::Unix => p.to_vec(),
        Os::Windows => p.iter().map(|&c| if c == b'/' { b'\\' } else { c }).collect(),
    }
}

/// ToSlash.
pub(crate) fn to_slash(os: Os, p: &[u8]) -> Vec<u8> {
    match os {
        Os::Unix => p.to_vec(),
        Os::Windows => p.iter().map(|&c| if c == b'\\' { b'/' } else { c }).collect(),
    }
}

/// filepathlite's lazybuf: the output, and whether it still is a prefix of the input,
/// which Windows' postClean asks.
struct Lazy<'a> {
    path: &'a [u8],
    buf: Vec<u8>,
    changed: bool,
}

impl Lazy<'_> {
    fn append(&mut self, c: u8) {
        if !self.changed && self.path.get(self.buf.len()) != Some(&c) {
            self.changed = true;
        }
        self.buf.push(c);
    }
}

/// Clean: the shortest equivalent path, lexically.
pub(crate) fn clean(os: Os, original: &[u8]) -> Vec<u8> {
    let vol_len = volume_name_len(os, original);
    let (vol, p) = original.split_at_checked(vol_len).unwrap_or((&[], original));
    if p.is_empty() {
        if vol_len > 1 && os.is_sep(at(original, 0)) && os.is_sep(at(original, 1)) {
            return from_slash(os, original);
        }
        return [original, b"."].concat();
    }
    let rooted = os.is_sep(at(p, 0));
    let n = p.len();
    let mut out = Lazy {
        path: p,
        buf: Vec::new(),
        changed: false,
    };
    let (mut r, mut dotdot) = (0, 0);
    if rooted {
        out.append(os.sep());
        r = 1;
        dotdot = 1;
    }
    while r < n {
        let c = at(p, r);
        if os.is_sep(c) || (c == b'.' && (r + 1 == n || os.is_sep(at(p, r + 1)))) {
            r += 1;
        } else if c == b'.' && at(p, r + 1) == b'.' && (r + 2 == n || os.is_sep(at(p, r + 2))) {
            r += 2;
            if out.buf.len() > dotdot {
                let mut w = out.buf.len() - 1;
                while w > dotdot && !os.is_sep(at(&out.buf, w)) {
                    w -= 1;
                }
                out.buf.truncate(w);
            } else if !rooted {
                if !out.buf.is_empty() {
                    out.append(os.sep());
                }
                out.append(b'.');
                out.append(b'.');
                dotdot = out.buf.len();
            }
        } else {
            if (rooted && out.buf.len() != 1) || (!rooted && !out.buf.is_empty()) {
                out.append(os.sep());
            }
            while r < n && !os.is_sep(at(p, r)) {
                out.append(at(p, r));
                r += 1;
            }
        }
    }
    if out.buf.is_empty() {
        out.append(b'.');
    }
    // postClean: a cleaned relative path must not become a drive or device path.
    if os == Os::Windows && vol_len == 0 && out.changed {
        let first = out.buf.iter().position(|&c| os.is_sep(c) || c == b':');
        if first.is_some_and(|i| at(&out.buf, i) == b':') {
            out.buf.splice(0..0, [b'.', os.sep()]);
        } else if out.buf.len() >= 3
            && os.is_sep(at(&out.buf, 0))
            && at(&out.buf, 1) == b'?'
            && at(&out.buf, 2) == b'?'
        {
            out.buf.splice(0..0, [os.sep(), b'.']);
        }
    }
    from_slash(os, &[vol, &out.buf].concat())
}

/// VolumeName.
pub(crate) fn volume_name(os: Os, p: &[u8]) -> Vec<u8> {
    from_slash(os, p.get(..volume_name_len(os, p)).unwrap_or_default())
}

/// Split: the directory, up to and including the last separator, and the file.
pub(crate) fn split(os: Os, p: &[u8]) -> (&[u8], &[u8]) {
    let vol = volume_name_len(os, p);
    let mut i = p.len();
    while i > vol && !os.is_sep(at(p, i - 1)) {
        i -= 1;
    }
    p.split_at_checked(i).unwrap_or((p, &[]))
}

/// Base: the last element, without trailing separators.
pub(crate) fn base(os: Os, p: &[u8]) -> Vec<u8> {
    if p.is_empty() {
        return b".".to_vec();
    }
    let mut end = p.len();
    while end > 0 && os.is_sep(at(p, end - 1)) {
        end -= 1;
    }
    let p = p.get(..end).unwrap_or_default();
    let p = p.get(volume_name_len(os, p)..).unwrap_or_default();
    let start = p.iter().rposition(|&c| os.is_sep(c)).map_or(0, |i| i + 1);
    let p = p.get(start..).unwrap_or_default();
    if p.is_empty() {
        return vec![os.sep()];
    }
    p.to_vec()
}

/// Dir: all but the last element, cleaned.
pub(crate) fn dir(os: Os, p: &[u8]) -> Vec<u8> {
    let vol_len = volume_name_len(os, p);
    let mut i = p.len();
    while i > vol_len && !os.is_sep(at(p, i - 1)) {
        i -= 1;
    }
    let vol = volume_name(os, p);
    let d = clean(os, p.get(vol_len..i).unwrap_or_default());
    if d == b"." && vol.len() > 2 {
        return vol;
    }
    [vol, d].concat()
}

/// Join: the non-empty elements joined by separators, then cleaned.
pub(crate) fn join(os: Os, parts: &[&[u8]]) -> Vec<u8> {
    match os {
        Os::Unix => {
            let parts: Vec<&[u8]> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
            if parts.is_empty() {
                return Vec::new();
            }
            clean(os, &parts.join(&b'/'))
        }
        Os::Windows => {
            let mut b: Vec<u8> = Vec::new();
            let mut last = 0u8;
            for &e in parts {
                let mut e = e;
                if b.is_empty() {
                } else if os.is_sep(last) {
                    while e.first().is_some_and(|&c| os.is_sep(c)) {
                        e = e.get(1..).unwrap_or_default();
                    }
                    if b.len() == 1 && e.starts_with(b"??") && (e.len() == 2 || os.is_sep(at(e, 2))) {
                        b.extend_from_slice(b".\\");
                    }
                } else if last == b':' {
                } else {
                    b.push(b'\\');
                    last = b'\\';
                }
                if let Some(&l) = e.last() {
                    b.extend_from_slice(e);
                    last = l;
                }
            }
            if b.is_empty() {
                return Vec::new();
            }
            clean(os, &b)
        }
    }
}

/// IsAbs.
pub(crate) fn is_abs(os: Os, p: &[u8]) -> bool {
    match os {
        Os::Unix => p.first() == Some(&b'/'),
        Os::Windows => {
            let l = volume_name_len(os, p);
            if l == 0 {
                return false;
            }
            if os.is_sep(at(p, 0)) && os.is_sep(at(p, 1)) {
                return true;
            }
            p.get(l..)
                .is_some_and(|rest| rest.first().is_some_and(|&c| os.is_sep(c)))
        }
    }
}

/// IsLocal: relative, within the directory it is relative to, and on Windows no reserved
/// device name or colon.
pub(crate) fn is_local(os: Os, p: &[u8]) -> bool {
    if p.is_empty() {
        return false;
    }
    let mut dots = false;
    match os {
        Os::Unix => {
            if is_abs(os, p) {
                return false;
            }
            dots = p.split(|&c| c == b'/').any(|part| part == b"." || part == b"..");
        }
        Os::Windows => {
            if os.is_sep(at(p, 0)) || p.contains(&b':') {
                return false;
            }
            for part in p.split(|&c| os.is_sep(c)) {
                if part == b"." || part == b".." {
                    dots = true;
                }
                if is_reserved_name(part) {
                    return false;
                }
            }
        }
    }
    let p = if dots { clean(os, p) } else { p.to_vec() };
    let up = [b"..".as_slice(), &[os.sep()]].concat();
    !(p == b".." || p.starts_with(&up))
}

/// Windows' reserved device names: CON, PRN, AUX, NUL, COM1-9, LPT1-9 (with ¹²³), CONIN$
/// and CONOUT$, before any extension or trailing spaces. Go asks RtlIsDosDeviceName_U of a
/// name with an extension; shards takes it as reserved, which refuses more, never less.
pub(crate) fn is_reserved_name(name: &[u8]) -> bool {
    let end = name
        .iter()
        .position(|&c| c == b':' || c == b'.')
        .unwrap_or(name.len());
    let mut base = name.get(..end).unwrap_or_default();
    while let Some(b) = base.strip_suffix(b" ") {
        base = b;
    }
    let up: Vec<u8> = base.iter().map(u8::to_ascii_uppercase).collect();
    match up.as_slice() {
        b"CON" | b"PRN" | b"AUX" | b"NUL" | b"CONIN$" | b"CONOUT$" => true,
        [b'C', b'O', b'M', rest @ ..] | [b'L', b'P', b'T', rest @ ..] => {
            matches!(rest, [b'1'..=b'9'] | b"\xc2\xb2" | b"\xc2\xb3" | b"\xc2\xb9")
        }
        _ => false,
    }
}

/// sameWord: equal, or on Windows equal under case folding.
fn same_word(os: Os, a: &[u8], b: &[u8]) -> bool {
    match os {
        Os::Unix => a == b,
        Os::Windows => match (std::str::from_utf8(a), std::str::from_utf8(b)) {
            (Ok(a), Ok(b)) => a.to_lowercase() == b.to_lowercase(),
            _ => a.eq_ignore_ascii_case(b),
        },
    }
}

/// Rel: `targ` relative to `base`, or None where Go's Rel fails ("can't make ...").
pub(crate) fn rel(os: Os, base_path: &[u8], targ_path: &[u8]) -> Option<Vec<u8>> {
    let base_vol = volume_name(os, base_path);
    let targ_vol = volume_name(os, targ_path);
    let base_c = clean(os, base_path);
    let targ_c = clean(os, targ_path);
    if same_word(os, &targ_c, &base_c) {
        return Some(b".".to_vec());
    }
    let mut base = base_c.get(base_vol.len()..).unwrap_or_default().to_vec();
    let targ = targ_c.get(targ_vol.len()..).unwrap_or_default();
    if base == b"." {
        base.clear();
    } else if base.is_empty() && volume_name_len(os, &base_vol) > 2 {
        base = vec![os.sep()];
    }
    let sep = os.sep();
    let base_slashed = base.first() == Some(&sep);
    let targ_slashed = targ.first() == Some(&sep);
    if base_slashed != targ_slashed || !same_word(os, &base_vol, &targ_vol) {
        return None;
    }
    let (bl, tl) = (base.len(), targ.len());
    let (mut b0, mut bi, mut t0, mut ti) = (0, 0, 0, 0);
    loop {
        while bi < bl && at(&base, bi) != sep {
            bi += 1;
        }
        while ti < tl && at(targ, ti) != sep {
            ti += 1;
        }
        if !same_word(
            os,
            targ.get(t0..ti).unwrap_or_default(),
            base.get(b0..bi).unwrap_or_default(),
        ) {
            break;
        }
        if bi < bl {
            bi += 1;
        }
        if ti < tl {
            ti += 1;
        }
        b0 = bi;
        t0 = ti;
    }
    if base.get(b0..bi) == Some(b"..") {
        return None;
    }
    if b0 != bl {
        let seps = base
            .get(b0..bl)
            .unwrap_or_default()
            .iter()
            .filter(|&&c| c == sep)
            .count();
        let mut buf = b"..".to_vec();
        for _ in 0..seps {
            buf.push(sep);
            buf.extend_from_slice(b"..");
        }
        if t0 != tl {
            buf.push(sep);
            buf.extend_from_slice(targ.get(t0..).unwrap_or_default());
        }
        return Some(clean(os, &buf));
    }
    Some(targ.get(t0..).unwrap_or_default().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: Vec<u8>) -> String {
        String::from_utf8(v).unwrap()
    }

    // Cases from go1.26.1 src/path/filepath/path_test.go (cleantests, wincleantests,
    // jointests, winjointests, reltests, winreltests, islocaltests, winislocaltests).
    #[test]
    fn clean_as_go() {
        for (i, o) in [
            ("abc", "abc"),
            ("abc/def", "abc/def"),
            ("", "."),
            ("..", ".."),
            ("../..", "../.."),
            ("/abc", "/abc"),
            ("/", "/"),
            ("abc/", "abc"),
            ("abc//def//ghi", "abc/def/ghi"),
            ("abc/./def", "abc/def"),
            ("/./abc/def", "/abc/def"),
            ("abc/def/../ghi/../jkl", "abc/jkl"),
            ("abc/def/../..", "."),
            ("/abc/def/../..", "/"),
            ("abc/def/../../..", ".."),
            ("/abc/def/../../..", "/"),
            ("abc/./../def", "def"),
            ("abc//./../def", "def"),
            ("abc/../../././../def", "../../def"),
        ] {
            assert_eq!(s(clean(Os::Unix, i.as_bytes())), o, "{i}");
        }
        for (i, o) in [
            (r"c:", r"c:."),
            (r"c:\", r"c:\"),
            (r"c:\abc", r"c:\abc"),
            (r"c:abc\..\..\.\.\..\def", r"c:..\..\def"),
            (r"c:\abc\def\..\..", r"c:\"),
            (r"c:\..\abc", r"c:\abc"),
            (r"c:..\abc", r"c:..\abc"),
            (r"\", r"\"),
            (r"/", r"\"),
            (r"\\i\..\c$", r"\\i\..\c$"),
            (r"\\i\..\i\c$", r"\\i\..\i\c$"),
            (r"\\i\..\I\c$", r"\\i\..\I\c$"),
            (r"\\host\share\foo\..\bar", r"\\host\share\bar"),
            (r"//host/share/foo/../baz", r"\\host\share\baz"),
            (r"\\host\share\foo\..\..\..\..\bar", r"\\host\share\bar"),
            (r"\\.\C:\a\..\..\..\..\bar", r"\\.\C:\bar"),
            (r"\\.\C:\\\\a", r"\\.\C:\a"),
            (r"\\a\b\..\c", r"\\a\b\c"),
            (r"\\a\b", r"\\a\b"),
            (r".\c:", r".\c:"),
            (r".\c:\foo", r".\c:\foo"),
            (r".\c:foo", r".\c:foo"),
            (r"//abc", r"\\abc"),
            (r"///abc", r"\\\abc"),
            (r"//abc//", r"\\abc\\"),
            (r"\\?\C:\", r"\\?\C:\"),
            (r"\\?\C:\a", r"\\?\C:\a"),
            (r"a/../c:", r".\c:"),
            (r"a\..\c:", r".\c:"),
            (r"a/../c:/a", r".\c:\a"),
            (r"a/../../c:", r"..\c:"),
            (r"foo:bar", r"foo:bar"),
            (r"/a/../??/a", r"\.\??\a"),
        ] {
            assert_eq!(s(clean(Os::Windows, i.as_bytes())), o, "{i}");
        }
    }

    #[test]
    fn join_and_rel_as_go() {
        assert_eq!(s(join(Os::Unix, &[b"a", b"", b"b/../c"])), "a/c");
        assert_eq!(s(join(Os::Unix, &[b"", b""])), "");
        assert_eq!(s(join(Os::Windows, &[b"C:", b"a"])), r"C:a");
        assert_eq!(s(join(Os::Windows, &[b"C:\\", b"a"])), r"C:\a");
        assert_eq!(s(join(Os::Windows, &[b"\\", b"??", b"a"])), r"\.\??\a");
        assert_eq!(s(join(Os::Windows, &[b"//", b"host", b"share"])), r"\\host\share");
        for (root, path, want) in [
            ("a/b", "a/b", Some(".")),
            ("a/b/.", "a/b", Some(".")),
            ("a/b", "a/b/.", Some(".")),
            ("./a/b", "a/b", Some(".")),
            ("a/b", "a/b/c", Some("c")),
            ("a/b/c", "a/b", Some("..")),
            ("a/b/c", "a/c/d", Some("../../c/d")),
            ("a/b", "c/d", Some("../../c/d")),
            ("../../a/b", "../../a/b/c/d", Some("c/d")),
            ("/a/b", "/a/b/c/d", Some("c/d")),
            ("/", "/a/b", Some("a/b")),
            (".", "a/b", Some("a/b")),
            (".", "..", Some("..")),
            ("..", ".", None),
            ("..", "a", None),
            ("../..", "..", None),
            ("a", "/a", None),
            ("/a", "a", None),
        ] {
            assert_eq!(
                rel(Os::Unix, root.as_bytes(), path.as_bytes()).map(s).as_deref(),
                want,
                "{root} {path}"
            );
        }
        for (root, path, want) in [
            (r"C:a\b\c", r"C:a/b/d", Some(r"..\d")),
            (r"C:\", r"D:\", None),
            (r"C:", r"D:", None),
            (r"C:\Projects", r"c:\projects\src", Some(r"src")),
            (r"C:\Projects", r"c:\projects", Some(r".")),
            (r"C:\Projects\a\..", r"c:\projects", Some(r".")),
            (r"\\host\share", r"\\host\share\file.txt", Some(r"file.txt")),
        ] {
            assert_eq!(
                rel(Os::Windows, root.as_bytes(), path.as_bytes())
                    .map(s)
                    .as_deref(),
                want,
                "{root} {path}"
            );
        }
    }

    #[test]
    fn is_local_as_go() {
        for (p, want) in [
            ("", false),
            (".", true),
            ("..", false),
            ("../a", false),
            ("/", false),
            ("/a", false),
            ("/a/../..", false),
            ("a", true),
            ("a/../a", true),
            ("a/", true),
            ("a/.", true),
            ("a/./b/./c", true),
            ("a/../b:/../../c", false),
        ] {
            assert_eq!(is_local(Os::Unix, p.as_bytes()), want, "{p}");
        }
        for (p, want) in [
            ("NUL", false),
            ("nul", false),
            ("nul ", false),
            ("nul.", false),
            ("a/nul:", false),
            ("a/nul : a", false),
            ("com0", true),
            ("com1", false),
            ("com2", false),
            ("com3:", false),
            ("com¹", false),
            ("com²", false),
            ("com³", false),
            ("com¹ : a", false),
            ("cOm1", false),
            ("lpt1", false),
            ("LPT1", false),
            ("lpt³", false),
            ("./nul", false),
            (r"\", false),
            (r"\a", false),
            (r"C:", false),
            (r"C:\a", false),
            (r"..\a", false),
            (r"a/../c:", false),
            (r"CONIN$", false),
            (r"conin$", false),
            (r"CONOUT$", false),
            (r"conout$", false),
            (r"dollar$", true),
        ] {
            assert_eq!(is_local(Os::Windows, p.as_bytes()), want, "{p}");
        }
    }

    #[test]
    fn split_base_dir_as_go() {
        assert_eq!(split(Os::Unix, b"a/b"), (&b"a/"[..], &b"b"[..]));
        assert_eq!(split(Os::Unix, b"a/b/"), (&b"a/b/"[..], &b""[..]));
        assert_eq!(split(Os::Unix, b"a"), (&b""[..], &b"a"[..]));
        assert_eq!(split(Os::Windows, b"c:a"), (&b"c:"[..], &b"a"[..]));
        assert_eq!(s(base(Os::Unix, b"/a/b/")), "b");
        assert_eq!(s(base(Os::Unix, b"////")), "/");
        assert_eq!(s(base(Os::Unix, b"")), ".");
        assert_eq!(s(base(Os::Windows, br"c:\")), r"\");
        assert_eq!(s(base(Os::Windows, br"c:\a\b")), "b");
        assert_eq!(s(dir(Os::Unix, b"/a/b/c")), "/a/b");
        assert_eq!(s(dir(Os::Unix, b"a")), ".");
        assert_eq!(s(dir(Os::Unix, b"/")), "/");
        assert_eq!(s(dir(Os::Windows, br"c:\a\b")), r"c:\a");
        assert_eq!(s(dir(Os::Windows, br"\\host\share")), r"\\host\share");
        assert_eq!(s(dir(Os::Windows, br"\\host\share\a")), r"\\host\share\");
    }
}
