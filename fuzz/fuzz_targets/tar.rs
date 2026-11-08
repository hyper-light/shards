//! Tar headers as a layer's archive holds them (shards_image::tar): whatever the bytes,
//! entries are read without panicking, each takes at least one 512-byte header of the
//! input, its path is clean (or empty, the root), and its data starts within the input.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut reader = shards_image::tar::Reader::new(data);
    let mut entries = 0u64;
    while let Ok(Some(entry)) = reader.next_entry() {
        entries += 1;
        assert!(entries * 512 <= data.len() as u64, "more entries than headers");
        assert!(entry.offset <= data.len() as u64, "data past the archive");
        // Empty for the root; otherwise no empty, `.` or `..` component (tar.rs, `Entry`).
        assert!(
            entry.path.is_empty()
                || !entry
                    .path
                    .split(|&b| b == b'/')
                    .any(|c| c.is_empty() || c == b"." || c == b".."),
            "an unclean path {:?}",
            String::from_utf8_lossy(&entry.path)
        );
    }
});
