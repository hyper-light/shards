//! Decodes each case with lzma-rust2, xz4rust and bzip2, against what was compressed.
use std::io::Read;

fn verdict(r: std::io::Result<Vec<u8>>, want: &Option<Vec<u8>>) -> String {
    match (r, want) {
        (Ok(got), Some(w)) if &got == w => "ok".into(),
        (Ok(_), Some(_)) => "WRONG BYTES".into(),
        (Err(e), Some(_)) => format!("FAILED {e}"),
        (Err(_), None) => "ok (refused)".into(),
        (Ok(_), None) => "ACCEPTED CORRUPT".into(),
    }
}

fn all(mut r: impl Read) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    r.read_to_end(&mut out)?;
    Ok(out)
}

fn main() {
    let dir = std::env::args().nth(1).expect("CASES_DIR");
    let read = |f: &str| std::fs::read(format!("{dir}/{f}")).expect(f);
    let input = Some(read("in"));
    let multi = Some([read("rnd"), read("txt")].concat());
    for (f, want) in [
        ("none.xz", &input), ("crc32.xz", &input), ("crc64.xz", &input), ("sha256.xz", &input),
        ("x86.xz", &input), ("arm64.xz", &input), ("delta.xz", &input), ("blocks.xz", &input),
        ("empty.xz", &Some(Vec::new())), ("multi.xz", &multi), ("padded.xz", &multi),
        ("corrupt.xz", &None), ("truncated.xz", &None),
    ] {
        let data = read(f);
        let a = verdict(all(lzma_rust2::XzReader::new(&data[..], true)), want);
        let b = verdict(all(xz4rust::XzReader::new(std::io::Cursor::new(data.clone()))), want);
        println!("| {f} | {a} | {b} |");
    }
    for (f, want) in [
        ("one.bz2", &input), ("multi.bz2", &multi), ("empty.bz2", &Some(Vec::new())),
        ("corrupt.bz2", &None), ("truncated.bz2", &None),
    ] {
        let data = read(f);
        println!("| {f} | {} |", verdict(all(bzip2::read::MultiBzDecoder::new(&data[..])), want));
    }
}
