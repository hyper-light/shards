//! Writes the images shards-ext4's unit tests hold to e2fsck (scripts/ext4/generate): each
//! size's, made as the tests make it (`tests::options`), into DIR as BYTES.img.
//!
//!   cargo run -p shards-ext4 --example images -- DIR BYTES...

use std::io::Write as _;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(dir) = args.next().map(std::path::PathBuf::from) else {
        let _ = writeln!(std::io::stderr(), "usage: images DIR BYTES...");
        return ExitCode::from(2);
    };
    for size in args {
        let made = size
            .to_str()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| format!("{size:?}: not a size in bytes"))
            .and_then(|bytes| write(&dir, bytes));
        if let Err(e) = made {
            let _ = writeln!(std::io::stderr(), "images: {e}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

fn write(dir: &std::path::Path, bytes: u64) -> Result<(), String> {
    let path = dir.join(format!("{bytes}.img"));
    let mut file = std::fs::File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    file.set_len(bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let options = shards_ext4::Options {
        bytes,
        uuid: *b"shards-ext4-test",
        hash_seed: *b"shards-hashseed!",
        time: 1_792_486_936,
    };
    shards_ext4::format(&mut file, &options).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}
