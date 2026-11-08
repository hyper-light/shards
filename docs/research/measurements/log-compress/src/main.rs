//! For each segment, each codec: the compressed size as a share of the segment's, and
//! the µs to compress it whole and to read it back as `logs` would, a buffer at a time:
//! n, p50, p90, p99 and max of N turns, the codecs interleaved in each turn. Then the
//! segment cut into frames of each size, each compressed alone, as a seekable file is
//! (zstd's seekable format; gzip members): their size together, with 8 bytes a frame
//! for the seek table, and the µs to read one frame back, a frame chosen in turn.
use std::io::{Read, Write};
use std::time::Instant;

#[derive(Clone, Copy)]
enum Codec {
    /// flate2 on zlib-rs, at a level.
    Gzip(u32),
    /// shards-zstd at a level (klauspost's EncoderLevelFromZstd), read by ruzstd.
    Zstd(i32),
}

impl Codec {
    fn name(self) -> String {
        match self {
            Codec::Gzip(l) => format!("gzip -{l}"),
            Codec::Zstd(l) => format!("zstd -{l}"),
        }
    }

    fn compress(self, input: &[u8]) -> Vec<u8> {
        match self {
            Codec::Gzip(l) => {
                let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(l));
                e.write_all(input).unwrap();
                e.finish().unwrap()
            }
            Codec::Zstd(l) => shards_zstd::compress(Some(l), input).unwrap(),
        }
    }

    /// Reads `packed` back a buffer at a time, as a reader streams a segment: how many
    /// bytes it gave.
    fn read_back(self, packed: &[u8]) -> usize {
        let mut r: Box<dyn Read + '_> = match self {
            Codec::Gzip(_) => Box::new(flate2::read::GzDecoder::new(packed)),
            Codec::Zstd(_) => Box::new(ruzstd::decoding::StreamingDecoder::new(packed).unwrap()),
        };
        let mut buf = vec![0u8; 64 << 10];
        let mut total = 0;
        loop {
            match r.read(&mut buf).unwrap() {
                0 => return total,
                n => total += n,
            }
        }
    }
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    let k = (sorted.len() - 1) as f64 * p;
    let (lo, hi) = (k.floor() as usize, k.ceil() as usize);
    sorted[lo] + (sorted[hi] - sorted[lo]) * (k - lo as f64)
}

fn summary(mut xs: Vec<f64>) -> String {
    xs.sort_by(f64::total_cmp);
    format!(
        "n={} p50={:.0} p90={:.0} p99={:.0} max={:.0}",
        xs.len(),
        pct(&xs, 0.5),
        pct(&xs, 0.9),
        pct(&xs, 0.99),
        xs[xs.len() - 1]
    )
}

fn main() {
    let mut args = std::env::args().skip(1);
    let n: usize = args.next().expect("N").parse().expect("N, a count");
    let codecs = [
        Codec::Gzip(1),
        Codec::Gzip(6),
        Codec::Gzip(9),
        Codec::Zstd(1),
        Codec::Zstd(3),
        Codec::Zstd(7),
        Codec::Zstd(11),
    ];
    println!("host: {} {}", std::env::consts::OS, std::env::consts::ARCH);
    let mut totals = vec![(0usize, 0usize); codecs.len()];
    let mut framed_totals = vec![(0usize, 0usize); framings().len()];
    for path in args {
        let input = std::fs::read(&path).expect("a segment");
        let mut packed: Vec<Vec<u8>> = codecs.iter().map(|c| c.compress(&input)).collect();
        for (c, p) in codecs.iter().zip(&packed) {
            assert_eq!(c.read_back(p), input.len(), "{}: read back whole", c.name());
        }
        let mut times = vec![(Vec::new(), Vec::new()); codecs.len()];
        for turn in 0..n {
            for k in 0..codecs.len() {
                // Each turn starts at another codec, so none always goes first.
                let i = (k + turn) % codecs.len();
                let start = Instant::now();
                packed[i] = codecs[i].compress(&input);
                times[i].0.push(start.elapsed().as_secs_f64() * 1e6);
                let start = Instant::now();
                let len = codecs[i].read_back(&packed[i]);
                times[i].1.push(start.elapsed().as_secs_f64() * 1e6);
                assert_eq!(len, input.len());
            }
        }
        let name = std::path::Path::new(&path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        println!("== {name}: {} bytes", input.len());
        for (i, c) in codecs.iter().enumerate() {
            totals[i].0 += input.len();
            totals[i].1 += packed[i].len();
            let (pack, read) = std::mem::take(&mut times[i]);
            let mbps = |us: &Vec<f64>| {
                let mut s = us.clone();
                s.sort_by(f64::total_cmp);
                input.len() as f64 / pct(&s, 0.5)
            };
            println!(
                "  {:8} {:6.2}% | compress µs {} ({:.0} MB/s) | read µs {} ({:.0} MB/s)",
                c.name(),
                100.0 * packed[i].len() as f64 / input.len() as f64,
                summary(pack.clone()),
                mbps(&pack),
                summary(read.clone()),
                mbps(&read),
            );
        }
        for (k, (c, size)) in framings().into_iter().enumerate() {
            let packed = frames(c, &input, size);
            let bytes: usize = packed.iter().map(Vec::len).sum::<usize>() + 8 * packed.len();
            framed_totals[k].0 += input.len();
            framed_totals[k].1 += bytes;
            let mut reads = Vec::with_capacity(n);
            for turn in 0..n {
                let frame = &packed[turn % packed.len()];
                let start = Instant::now();
                let len = c.read_back(frame);
                reads.push(start.elapsed().as_secs_f64() * 1e6);
                assert!(len > 0 && len <= size);
            }
            println!(
                "  {:8} {:>5} KiB frames {:6.2}% | read a frame µs {}",
                c.name(),
                size >> 10,
                100.0 * bytes as f64 / input.len() as f64,
                summary(reads),
            );
        }
    }
    println!("== all segments, whole");
    for (c, (raw, packed)) in codecs.iter().zip(totals) {
        println!(
            "  {:8} {:6.2}% of {raw} bytes",
            c.name(),
            100.0 * packed as f64 / raw as f64
        );
    }
    println!("== all segments, in frames");
    for ((c, f), (raw, packed)) in framings().into_iter().zip(framed_totals) {
        println!(
            "  {:8} {:>5} KiB frames {:6.2}% of {raw} bytes",
            c.name(),
            f >> 10,
            100.0 * packed as f64 / raw as f64
        );
    }
}

/// The codecs and frame sizes framed: the levels each codec is chosen at.
fn framings() -> Vec<(Codec, usize)> {
    let mut v = Vec::new();
    for c in [
        Codec::Gzip(6),
        Codec::Gzip(9),
        Codec::Zstd(3),
        Codec::Zstd(7),
        Codec::Zstd(11),
    ] {
        for f in [64 << 10, 256 << 10, 1 << 20, 4 << 20] {
            v.push((c, f));
        }
    }
    v
}

/// `input` cut into frames of `size`, each compressed alone.
fn frames(c: Codec, input: &[u8], size: usize) -> Vec<Vec<u8>> {
    input.chunks(size).map(|chunk| c.compress(chunk)).collect()
}
