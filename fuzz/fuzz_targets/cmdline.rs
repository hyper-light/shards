//! Command lines, NUL-separated words, read as the Docker CLI reads them for each command
//! shards serves (shards_cmdline::flags): no panic, and every outcome is one of the three.
#![no_main]

use libfuzzer_sys::fuzz_target;
use shards_cmdline::{commands, flags};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let argv: Vec<String> = text.split('\0').map(str::to_string).collect();
    for command in [
        &commands::RUN,
        &commands::PS,
        &commands::WAIT,
        &commands::LOGS,
        &commands::RM,
        &commands::STOP,
        &commands::KILL,
    ] {
        let _ = flags::parse(command, "shards command", &argv, &|_, v| Ok(v.to_string()));
    }
});
