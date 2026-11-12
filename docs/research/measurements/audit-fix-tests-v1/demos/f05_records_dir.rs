// Finding 5: managed_network.rs:56 opened SHARDS_NETWORK_RECORDS with create_dir_all, which
// accepts a directory an earlier run left behind, records included.
use std::fs;
use std::path::Path;

fn main() {
    let arg = std::env::args().nth(1).expect("a new directory path");
    let dir = Path::new(&arg);
    fs::create_dir(dir).unwrap();
    fs::write(dir.join("case.stdout"), "an earlier run's record").unwrap();
    println!("old: create_dir_all(existing) = {:?}", fs::create_dir_all(dir).map_err(|e| e.kind()));
    println!(
        "old: the earlier run's case.stdout is still there for this run's reader: {}",
        dir.join("case.stdout").exists()
    );
    println!("new: create_dir(existing) = {:?}", fs::create_dir(dir).map_err(|e| e.kind()));
    fs::remove_dir_all(dir).unwrap();
}
