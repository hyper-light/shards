//! Standard input gzipped as BuildKit gzips a layer, to standard output:
//! `cargo run --release -p shards-flate --example gzip [LEVEL] < layer.tar > layer.tar.gz`.

use std::io::{Read as _, Write as _};

fn main() -> std::io::Result<()> {
    let level = std::env::args()
        .nth(1)
        .and_then(|l| l.parse().ok())
        .unwrap_or(shards_flate::DEFAULT_COMPRESSION);
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    let mut w = shards_flate::GzipWriter::new(Vec::new(), level)?;
    w.write_all(&input)?;
    std::io::stdout().write_all(&w.finish()?)
}
