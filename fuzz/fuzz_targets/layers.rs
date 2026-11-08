//! A layer applied to a tree, and the tree written as EROFS (shards_image::layer, erofs),
//! as a root filesystem is built from an image a registry sends: no panic, and the
//! entries a build counts are the entries it takes.
#![no_main]

use libfuzzer_sys::fuzz_target;
use shards_image::{erofs, layer};

fuzz_target!(|data: &[u8]| {
    let mut tree = layer::root();
    let mut counted = 0u64;
    let applied = layer::apply(&mut tree, 0, std::io::Cursor::new(data), &mut |_| {
        counted += 1;
        Ok(())
    });
    assert!(counted * 512 <= data.len() as u64, "more entries than headers");
    if applied.is_ok() {
        let mut archives = layer::Archives(vec![std::io::Cursor::new(data)]);
        let _ = erofs::write(&tree, &mut archives, &mut std::io::sink());
    }
});
