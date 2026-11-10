//! Fixed Linux observation formats used by the coordinator-only fixture.
use std::collections::BTreeSet;

fn decimal(bytes: &[u8]) -> Option<u64> {
    (!bytes.is_empty() && bytes.iter().all(u8::is_ascii_digit))
        .then(|| std::str::from_utf8(bytes).ok()?.parse().ok())?
}

pub(crate) fn process(bytes: &[u8]) -> Option<(u32, u64)> {
    let pid = bytes.get(..bytes.iter().position(|b| *b == b' ')?)?;
    let end = bytes.iter().rposition(|b| *b == b')')?;
    let started = bytes
        .get(end.checked_add(1)?..)?
        .split(u8::is_ascii_whitespace)
        .filter(|s| !s.is_empty())
        .nth(19)?;
    Some((u32::try_from(decimal(pid)?).ok()?, decimal(started)?))
}

pub(crate) fn socket_inode(target: &[u8]) -> Option<u64> {
    decimal(target.strip_prefix(b"socket:[")?.strip_suffix(b"]")?)
}

/// Namespace-wide tables are filtered BEFORE anything is retained or reported.
pub(crate) fn owned_udp<'a>(table: &'a [u8], inodes: &BTreeSet<u64>) -> Option<Vec<&'a [u8]>> {
    let mut lines = table.split(|b| *b == b'\n').filter(|l| !l.is_empty());
    let header = lines.next()?;
    if !header.windows(b"inode".len()).any(|b| b == b"inode") {
        return None;
    }
    let mut owned = Vec::new();
    for line in lines {
        // Linux net/ipv4/udp.c udp4_format_sock and ipv6/udp.c udp6_seq_show.
        let inode = decimal(
            line.split(u8::is_ascii_whitespace)
                .filter(|s| !s.is_empty())
                .nth(9)?,
        )?;
        if inodes.contains(&inode) {
            owned.push(line);
        }
    }
    Some(owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_identity_ignores_raw_comm_and_checks_numeric_fields() {
        let mut stat = b"12 (own \xff) ) \xfe) S ".to_vec();
        for _ in 4..22 {
            stat.extend_from_slice(b"0 ");
        }
        stat.extend_from_slice(u64::MAX.to_string().as_bytes());
        assert_eq!(process(&stat), Some((12, u64::MAX)));
        assert_eq!(process(b"12 (missing) S 0"), None);
        assert_eq!(process(b""), None);
        let mut bad = stat.clone();
        bad.push(b'0');
        assert_eq!(process(&bad), None);
        let mut bad_pid = stat;
        bad_pid[0] = b'+';
        assert_eq!(process(&bad_pid), None);
    }

    #[test]
    fn only_exact_socket_inode_links_are_accepted() {
        assert_eq!(socket_inode(b"socket:[42]"), Some(42));
        assert_eq!(socket_inode(b"socket:[18446744073709551615]"), Some(u64::MAX));
        for bad in [
            b"socket:[+42]".as_slice(),
            b"pipe:[42]",
            b"socket:[]",
            b"socket:[18446744073709551616]",
        ] {
            assert_eq!(socket_inode(bad), None);
        }
    }

    #[test]
    fn namespace_socket_rows_are_retained_only_for_sampled_owned_inodes() {
        let table = b"sl local_address rem_address st tx_queue rx_queue tr tm->when retrnsmt uid timeout inode\n 0: 0100007F:1234 00000000:0000 07 00000000:00000000 00:00000000 00000000 1000 0 42 2\n 1: unrelated secretish 07 0 0 0 1000 0 99 2\n";
        let rows = owned_udp(table, &BTreeSet::from([42])).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].windows(b"unrelated".len()).any(|b| b == b"unrelated"));
        assert!(owned_udp(table, &BTreeSet::new()).unwrap().is_empty());
        assert!(owned_udp(b"not-a-table\n", &BTreeSet::from([42])).is_none());
        assert!(owned_udp(b"inode\n malformed\n", &BTreeSet::from([42])).is_none());
    }
}
