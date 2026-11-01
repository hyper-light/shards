//! A container log's segments (audit A12), which only the daemon makes: no VM reaches a
//! container's directory (D30), so a warm VM asks its daemon for each segment after the
//! first (`kind::LOG_SEGMENT`).

use std::fs;
use std::io;

use shards_vmm::platform;

/// A log's segments (audit A12): the first `log` and `log.idx`, then `log.1` and
/// `log.1.idx`, and on. A writer starts the next once the last would pass
/// [`LogRetention::size`](crate::spec::LogRetention) bytes, and removes the oldest past
/// [`LogRetention::files`](crate::spec::LogRetention). A
/// segment is there once its index is: it is made after its log, and removed before it.
/// Nothing is renamed, so a reader never pairs one segment's log with another's index,
/// and a segment's successor there says it is whole.
pub fn log_segment(seq: u64) -> (String, String) {
    if seq == 0 {
        ("log".into(), "log.idx".into())
    } else {
        (format!("log.{seq}"), format!("log.{seq}.idx"))
    }
}

/// The name of segment `seq`'s log once its daemon has compressed it, rotated (D162):
/// `log.zst`, `log.1.zst`, and on.
pub fn packed_segment(seq: u64) -> String {
    format!("{}.zst", log_segment(seq).0)
}

/// Makes segment `seq` of the log in the container directory `dir` holds open, and removes
/// the oldest past `files` segments, its log compressed or not. A segment is there for
/// readers once its index is, so its log is made first, and its index goes first; the
/// index there says the segment before is done ([`log_segment`]). A segment already there
/// is not taken, and one that cannot be made whole leaves nothing of itself.
pub fn new_segment(dir: &fs::File, seq: u64, files: u64) -> io::Result<(fs::File, fs::File)> {
    let (log_name, index_name) = log_segment(seq);
    let log = platform::append_new_in(dir, &log_name)?;
    let index = match platform::append_new_in(dir, &index_name) {
        Ok(index) => index,
        Err(e) => {
            let _ = platform::unlink_in(dir, &log_name);
            return Err(e);
        }
    };
    if let Some(gone) = seq.checked_sub(files) {
        let (log_name, index_name) = log_segment(gone);
        let _ = platform::unlink_in(dir, &index_name);
        let _ = platform::unlink_in(dir, &log_name);
        let _ = platform::unlink_in(dir, &packed_segment(gone));
    }
    Ok((log, index))
}
