//! Image references and digests as users and registries write them
//! (shards_image::reference): no panic, and what parses prints as text that parses to
//! the same.
#![no_main]

use libfuzzer_sys::fuzz_target;
use shards_image::reference::{Digest, Reference};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(r) = Reference::parse(text) {
        let again = Reference::parse(&r.to_string()).expect("a reference's text parses");
        assert_eq!(again, r);
    }
    let _ = Reference::parse_normalized(text);
    if let Ok(d) = Digest::parse(text) {
        assert_eq!(Digest::parse(&d.to_string()).ok(), Some(d));
    }
});
