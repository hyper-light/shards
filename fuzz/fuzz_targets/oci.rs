//! Manifests, indexes and configs as a registry sends them (shards_image::oci), under
//! each media type a registry may name: no panic.
#![no_main]

use libfuzzer_sys::fuzz_target;
use shards_image::oci::{self, media};

fuzz_target!(|data: &[u8]| {
    for content_type in [
        "",
        media::OCI_MANIFEST,
        media::OCI_INDEX,
        media::DOCKER_MANIFEST,
        media::DOCKER_LIST,
        media::DOCKER_SCHEMA1_SIGNED,
    ] {
        if let Ok(oci::Document::Manifest(m)) = oci::parse_document(data, content_type) {
            for d in std::iter::once(&m.config).chain(&m.layers) {
                let _ = d.digest();
                let _ = d.size();
            }
        }
    }
    let _ = oci::parse_config(data);
});
