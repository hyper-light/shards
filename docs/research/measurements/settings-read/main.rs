// M252 (docs/research/platform-measurements.md): what reading `shards configure disk`'s
// settings costs each time they are used (D186), against a stat of the file, which a cache
// keyed by its modification time would cost instead, and a read where no file is there.
//
//   rustc -O -o settings-read main.rs && ./settings-read DIR
use std::time::Instant;

fn said(name: &str, v: &mut [u128]) {
    v.sort_unstable();
    let n = v.len();
    let at = |p: f64| v[((n as f64 * p) as usize).min(n - 1)];
    println!(
        "{name}: n={n} p50={}ns p90={}ns p99={}ns max={}ns",
        at(0.5),
        at(0.9),
        at(0.99),
        v[n - 1]
    );
}

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("DIR"));
    let file = dir.join("disk");
    // As `shards configure disk --log-history 64k --build-cache-max 50%` writes it.
    std::fs::write(
        &file,
        "# shards configure disk: what shards keeps on disk (D186)\nbuild-cache-max 50%\nlog-history 65536\n",
    )
    .unwrap();
    let missing = dir.join("missing");
    let n = 20_000;
    for _ in 0..1_000 {
        let _ = std::fs::metadata(&file);
        let _ = std::fs::read_to_string(&file);
    }
    let (mut stat, mut read, mut none) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..n {
        let t = Instant::now();
        let m = std::fs::metadata(&file).and_then(|m| m.modified());
        stat.push(t.elapsed().as_nanos());
        assert!(m.is_ok());
        let t = Instant::now();
        let s = std::fs::read_to_string(&file);
        read.push(t.elapsed().as_nanos());
        assert!(s.is_ok());
        let t = Instant::now();
        let s = std::fs::read_to_string(&missing);
        none.push(t.elapsed().as_nanos());
        assert!(s.is_err());
    }
    said("stat and modification time", &mut stat);
    said("read of the settings", &mut read);
    said("read where there are none", &mut none);
}
