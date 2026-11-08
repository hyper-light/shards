//! A snapshot's state file (shards_vmm::snapshot), as a restore reads one a user was
//! given: no panic, and no allocation past what its bytes can describe.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = shards_vmm::snapshot::decodes(data);
});
