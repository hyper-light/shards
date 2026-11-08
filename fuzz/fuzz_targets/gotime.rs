//! Durations and timestamps as `--since` and `--until` take them, read as Go reads them
//! (shards_cmdline::gotime): no panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = shards_cmdline::gotime::parse_duration(text);
    let _ = shards_cmdline::gotime::parse_unix_timestamp(text);
});
