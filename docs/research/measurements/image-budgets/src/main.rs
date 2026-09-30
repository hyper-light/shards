//! What an image's entries cost a build (PM M47): the peak RSS of applying one layer of N
//! empty files, in directories of 1000, and writing its EROFS image.
use std::io::{Cursor, Write};

fn header(name: &str, dir: bool) -> [u8; 512] {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    h[100..108].copy_from_slice(if dir { b"0000755\0" } else { b"0000644\0" });
    h[108..116].copy_from_slice(b"0000000\0");
    h[116..124].copy_from_slice(b"0000000\0");
    h[124..136].copy_from_slice(b"00000000000\0");
    h[136..148].copy_from_slice(b"00000000000\0");
    h[148..156].copy_from_slice(b"        ");
    h[156] = if dir { b'5' } else { b'0' };
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    h
}

fn rss_kib() -> i64 {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    // macOS reports bytes, Linux KiB.
    if cfg!(target_os = "macos") { u.ru_maxrss / 1024 } else { u.ru_maxrss }
}

fn main() {
    let n: usize = std::env::args().nth(1).unwrap().parse().unwrap();
    let mut tar = Vec::with_capacity(n * 512 + n / 1000 * 512 + 1024);
    for i in 0..n {
        if i % 1000 == 0 {
            tar.extend_from_slice(&header(&format!("d{:05}/", i / 1000), true));
        }
        tar.extend_from_slice(&header(&format!("d{:05}/f{:03}", i / 1000, i % 1000), false));
    }
    tar.extend_from_slice(&[0u8; 1024]);
    let before = rss_kib();
    let mut tree = shards_image::layer::root();
    shards_image::layer::apply(&mut tree, 0, Cursor::new(&tar), &mut |_| Ok(())).unwrap();
    let applied = rss_kib();
    let mut out = std::io::sink();
    let mut src = shards_image::layer::Archives(vec![Cursor::new(&tar)]);
    shards_image::erofs::write(&tree, &mut src, &mut out).unwrap();
    out.flush().unwrap();
    let written = rss_kib();
    println!("n {n} tar {} MiB rss before {} MiB applied +{} MiB written +{} MiB per entry {} B",
        tar.len() >> 20, before >> 10, (applied - before) >> 10, (written - before) >> 10,
        (written - before) * 1024 / n as i64);
}
