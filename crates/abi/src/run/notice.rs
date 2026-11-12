//! What a host saw change in a directory it shares, told to its guest (D235): records of
//! `NAME\0PATH\0`, NAME the share's (its virtio-fs tag where the guest reads them, the
//! share's index where its share process says them), PATH the file's within the share,
//! its components joined by `/`, empty for the share's root. Neither holds a NUL; PATH's
//! components are plain, never empty, `.` or `..`, so that no record names anything
//! outside its share.

use alloc::vec::Vec;

/// `name` and `path` as a record, appended to `out`; `false`, with nothing appended, where
/// either is not one a record holds.
pub fn encode(name: &[u8], path: &[u8], out: &mut Vec<u8>) -> bool {
    if name.is_empty() || name.contains(&0) || !plain(path) {
        return false;
    }
    out.extend_from_slice(name);
    out.push(0);
    out.extend_from_slice(path);
    out.push(0);
    true
}

/// What the start of a stream of records holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decoded<'a> {
    /// A whole record, and the bytes past it.
    Record {
        name: &'a [u8],
        path: &'a [u8],
        rest: &'a [u8],
    },
    /// Part of one: the rest is to come.
    Partial,
    /// Not one a record holds: the stream is not records.
    Malformed,
}

/// The first record of `bytes`.
pub fn decode(bytes: &[u8]) -> Decoded<'_> {
    let mut parts = bytes.splitn(3, |&b| b == 0);
    let (Some(name), Some(path), Some(rest)) = (parts.next(), parts.next(), parts.next()) else {
        return Decoded::Partial;
    };
    if name.is_empty() || !plain(path) {
        return Decoded::Malformed;
    }
    Decoded::Record { name, path, rest }
}

/// Whether `path` is one a record holds: empty, or components joined by `/`, none empty,
/// `.` or `..`, and no NUL.
pub fn plain(path: &[u8]) -> bool {
    path.is_empty()
        || (!path.contains(&0) && path.split(|&b| b == b'/').all(|c| !matches!(c, b"" | b"." | b"..")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_read_as_written_and_nothing_outside_a_share_is_one() {
        let mut out = Vec::new();
        assert!(encode(b"shards0", b"src/main.rs", &mut out));
        assert!(encode(b"shards-plug1", b"", &mut out));
        let Decoded::Record { name, path, rest } = decode(&out) else {
            panic!("{out:?}");
        };
        assert_eq!((name, path), (&b"shards0"[..], &b"src/main.rs"[..]));
        assert_eq!(
            decode(rest),
            Decoded::Record {
                name: b"shards-plug1",
                path: b"",
                rest: b""
            }
        );
        // Part of one is none yet.
        assert_eq!(decode(b"shards0\0src"), Decoded::Partial);
        assert_eq!(decode(b"shards0"), Decoded::Partial);
        assert_eq!(decode(b""), Decoded::Partial);
        for bad in [&b"../x"[..], b"a/../b", b"/a", b"a//b", b"a/", b".", b"a/./b"] {
            let mut none = Vec::new();
            assert!(!encode(b"t", bad, &mut none), "{bad:?}");
            assert!(none.is_empty());
            let mut record = b"t\0".to_vec();
            record.extend_from_slice(bad);
            record.push(0);
            assert_eq!(decode(&record), Decoded::Malformed, "{bad:?}");
        }
        assert!(!encode(b"", b"a", &mut Vec::new()));
        assert!(!encode(b"t\0u", b"a", &mut Vec::new()));
        assert_eq!(decode(b"\0a\0"), Decoded::Malformed);
    }
}
