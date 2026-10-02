//! verify HOME REFERENCE: moves the root filesystem `shards build` wrote for REFERENCE
//! aside, has Store::rootfs build it again from the stored layers, and compares bytes.
use shards_image::reference::Digest;
use shards_image::store::{Layer, Limits, Store};
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().collect();
    let store = Store::open(&Path::new(&a[1]).join("images"))?;
    let desc = store.tagged(&a[2])?.ok_or("no such tag")?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&store.content(&desc, 1 << 20)?.ok_or("no manifest")?)?;
    let cfg = &manifest["config"];
    let cdesc = shards_image::oci::Descriptor {
        media_type: cfg["mediaType"].as_str().unwrap().into(),
        digest: cfg["digest"].as_str().unwrap().into(),
        size: cfg["size"].as_i64().unwrap(),
        platform: None,
    };
    let config: serde_json::Value =
        serde_json::from_slice(&store.content(&cdesc, 1 << 20)?.ok_or("no config")?)?;
    let layers: Vec<Layer> = manifest["layers"]
        .as_array()
        .unwrap()
        .iter()
        .zip(config["rootfs"]["diff_ids"].as_array().unwrap())
        .map(|(l, d)| Layer {
            blob: Digest::parse(l["digest"].as_str().unwrap()).unwrap(),
            media_type: l["mediaType"].as_str().unwrap().into(),
            diff_id: Digest::parse(d.as_str().unwrap()).unwrap(),
        })
        .collect();
    let built = store.rootfs(&layers, &Limits::none())?;
    let aside = built.with_extension("stacked");
    std::fs::rename(&built, &aside)?;
    let again = store.rootfs(&layers, &Limits::none())?;
    let (x, y) = (std::fs::read(&aside)?, std::fs::read(&again)?);
    println!(
        "{} layers; stacked {} bytes, read back {} bytes: {}",
        layers.len(),
        x.len(),
        y.len(),
        if x == y { "IDENTICAL" } else { "DIFFERENT" }
    );
    std::fs::remove_file(&aside)?;
    Ok(())
}
