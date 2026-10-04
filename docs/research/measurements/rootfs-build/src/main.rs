//! `rootfs-build RUNS STORE LAYOUT...`: for each `docker save` layout, its layers' blobs
//! (as the registry compressed them) put in a store under STORE, then its root
//! filesystem built RUNS times from them by `Store::rootfs`, the built one removed
//! between runs. Prints `NAME MS` a run.
use shards_image::reference::Digest;
use shards_image::store::{Layer, Limits, Store};
use std::path::Path;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let runs: usize = args[0].parse().expect("RUNS");
    let store_dir = Path::new(&args[1]);
    for layout in &args[2..] {
        let layout = Path::new(layout);
        let name = layout.file_name().unwrap().to_string_lossy().into_owned();
        let read = |p: &str| std::fs::read(layout.join(p)).expect(p);
        let manifest: serde_json::Value = serde_json::from_slice(&read("manifest.json")).unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&read(manifest[0]["Config"].as_str().unwrap())).unwrap();
        std::fs::create_dir_all(store_dir.join(&name)).unwrap();
        let store = Store::open(&store_dir.join(&name)).unwrap();
        let layers: Vec<Layer> = manifest[0]["Layers"]
            .as_array()
            .unwrap()
            .iter()
            .zip(config["rootfs"]["diff_ids"].as_array().unwrap())
            .map(|(path, diff_id)| {
                let path = path.as_str().unwrap();
                let blob = Digest::parse(&path.replacen("blobs/sha256/", "sha256:", 1)).unwrap();
                if !store.blob_path(&blob).is_file() {
                    let bytes = read(path);
                    store.ingest(&blob, bytes.len() as u64, &mut &bytes[..]).unwrap();
                }
                Layer {
                    blob,
                    media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
                    diff_id: Digest::parse(diff_id.as_str().unwrap()).unwrap(),
                }
            })
            .collect();
        for _ in 0..runs {
            let t = Instant::now();
            let built = store.rootfs(&layers, &Limits::none()).unwrap();
            let ms = t.elapsed().as_secs_f64() * 1e3;
            std::fs::remove_file(&built).unwrap();
            println!("{name} {ms:.1}");
        }
    }
}
