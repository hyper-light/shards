//! `WWW-Authenticate` challenges as registries send them (shards_registry::auth): no
//! panic, one challenge at most per field line.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let lines: Vec<&str> = text.lines().collect();
    let found = shards_registry::auth::challenges(lines.iter().copied());
    assert!(found.len() <= lines.len());
});
