//! `rootfs-layout VARIANT STORE LAYOUT...`: builds each `docker save` layout's root
//! filesystem once, the VARIANT way, and prints `VARIANT NAME MS CPU_MS RSS_MB`: wall
//! time, CPU time (user and system, every thread) and the process's peak RSS.
//!
//! - `today`: `Store::rootfs`, as shards builds it: layers decompressed and checked on
//!   threads into temporary archives, stacked in order, one EROFS image written from
//!   them and synced.
//! - `memory`: the same image, from layers decompressed into memory: one write of the
//!   data, at the cost of holding every layer.
//! - `blobs`: each layer's file data written once as it decompresses, every file at a
//!   4 KiB block, into the layer's own blob, synced; then a metadata image of the stacked
//!   tree. That image is approximated as the EROFS writer's image of the tree with no
//!   file data, plus the 8-byte chunk index entry (`erofs_inode_chunk_index`) a 4 KiB
//!   block of data would take.
use sha2::Digest as _;
use shards_image::erofs::{self, DataRef, Source};
use shards_image::layer;
use shards_image::reference::Digest;
use shards_image::store::{Layer, Limits, Store};
use shards_image::tar::{self, Type, writer};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

const BLOCK: u64 = 4096;

struct Image {
    name: String,
    blobs: Vec<PathBuf>,
    layers: Vec<Layer>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let variant = args[0].as_str();
    let store_dir = Path::new(&args[1]);
    for layout in &args[2..] {
        let image = read(Path::new(layout));
        let dir = store_dir.join(&image.name);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir).unwrap();
        if variant == "today" {
            for (blob, layer) in image.blobs.iter().zip(&image.layers) {
                if !store.blob_path(&layer.blob).is_file() {
                    let bytes = std::fs::read(blob).unwrap();
                    store.ingest(&layer.blob, bytes.len() as u64, &mut &bytes[..]).unwrap();
                }
            }
        }
        let out = dir.join("out");
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).unwrap();
        let (t, cpu) = (Instant::now(), cpu_ms());
        match variant {
            "today" => {
                let built = store.rootfs(&image.layers, &Limits::none()).unwrap();
                std::fs::remove_file(built).unwrap();
            }
            "memory" => memory(&image, &out),
            "blobs" => blobs(&image, &out),
            _ => panic!("no variant {variant}"),
        }
        let (ms, cpu) = (t.elapsed().as_secs_f64() * 1e3, cpu_ms() - cpu);
        if std::env::var_os("SIZES").is_some() {
            for e in std::fs::read_dir(&out).unwrap() {
                let e = e.unwrap();
                eprintln!("{variant} {} {:?} {}", image.name, e.file_name(), e.metadata().unwrap().len());
            }
        }
        std::fs::remove_dir_all(&out).unwrap();
        println!("{variant} {} {ms:.1} {cpu:.1} {:.0}", image.name, rss_mb());
    }
}

fn read(layout: &Path) -> Image {
    let name = layout.file_name().unwrap().to_string_lossy().into_owned();
    let get = |p: &str| std::fs::read(layout.join(p)).expect(p);
    let manifest: serde_json::Value = serde_json::from_slice(&get("manifest.json")).unwrap();
    let config: serde_json::Value =
        serde_json::from_slice(&get(manifest[0]["Config"].as_str().unwrap())).unwrap();
    let mut blobs = Vec::new();
    let layers = manifest[0]["Layers"]
        .as_array()
        .unwrap()
        .iter()
        .zip(config["rootfs"]["diff_ids"].as_array().unwrap())
        .map(|(path, diff_id)| {
            let path = path.as_str().unwrap();
            blobs.push(layout.join(path));
            Layer {
                blob: Digest::parse(&path.replacen("blobs/sha256/", "sha256:", 1)).unwrap(),
                media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
                diff_id: Digest::parse(diff_id.as_str().unwrap()).unwrap(),
            }
        })
        .collect();
    Image { name, blobs, layers }
}

/// A layer's decompressed stream, hashed as it is read; checked against its DiffID at
/// its end.
struct Hashing<R> {
    inner: R,
    hasher: sha2::Sha256,
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

fn decoded(blob: &Path) -> Hashing<impl Read> {
    let file = BufReader::with_capacity(1 << 20, File::open(blob).unwrap());
    Hashing {
        inner: flate2::bufread::MultiGzDecoder::new(file),
        hasher: sha2::Sha256::new(),
    }
}

fn check(h: Hashing<impl Read>, layer: &Layer) {
    let mut h = h;
    io::copy(&mut h, &mut io::sink()).unwrap();
    let got: String = h.hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    let got = format!("sha256:{got}");
    assert_eq!(got, layer.diff_id.to_string(), "layer {}", layer.blob);
}

/// Each layer on its own thread, in order of the layers.
fn each_layer<T: Send>(image: &Image, f: impl Fn(usize, &Path, &Layer) -> T + Sync) -> Vec<T> {
    std::thread::scope(|s| {
        let f = &f;
        let threads: Vec<_> = image
            .blobs
            .iter()
            .zip(&image.layers)
            .enumerate()
            .map(|(i, (b, l))| s.spawn(move || f(i, b, l)))
            .collect();
        threads.into_iter().map(|t| t.join().unwrap()).collect()
    })
}

struct Memory(Vec<Vec<u8>>);

impl Source for Memory {
    fn read_at(&mut self, data: DataRef, at: u64, buf: &mut [u8]) -> io::Result<()> {
        let from = (data.offset + at) as usize;
        buf.copy_from_slice(&self.0[data.source as usize][from..from + buf.len()]);
        Ok(())
    }
}

fn memory(image: &Image, out: &Path) {
    let tars = each_layer(image, |_, blob, layer| {
        let mut d = decoded(blob);
        let mut tar = Vec::new();
        d.read_to_end(&mut tar).unwrap();
        check(d, layer);
        tar
    });
    let mut tree = layer::root();
    for (i, tar) in tars.iter().enumerate() {
        layer::apply(&mut tree, i as u32, Cursor::new(&tar[..]), &mut |_| Ok(())).unwrap();
        tree.compact();
    }
    let mut file = BufWriter::with_capacity(1 << 20, File::create(out.join("image")).unwrap());
    let w = erofs::write(&tree, &mut Memory(tars), &mut file).unwrap();
    if std::env::var_os("SIZES").is_some() {
        eprintln!("memory nodes {} inodes {}", tree.len(), w.inodes);
    }
    durable(&file.into_inner().unwrap());
}

/// Every file's data a run of zeros: the image is written for its metadata alone.
struct Nothing;

impl Source for Nothing {
    fn read_at(&mut self, _: DataRef, _: u64, buf: &mut [u8]) -> io::Result<()> {
        buf.fill(0);
        Ok(())
    }
}

fn blobs(image: &Image, out: &Path) {
    let layers = each_layer(image, |i, blob, layer| {
        let mut d = decoded(blob);
        let mut data = BufWriter::with_capacity(1 << 20, File::create(out.join(format!("blob-{i}"))).unwrap());
        let mut headers = writer::Writer::new(Vec::new());
        let (mut at, mut chunks) = (0u64, 0u64);
        {
            let mut r = tar::Reader::new(&mut d);
            while let Some(e) = r.next_entry().unwrap() {
                if e.path.is_empty() {
                    continue;
                }
                let mut pax = BTreeMap::new();
                for (k, v) in &e.xattrs {
                    let mut key = b"SCHILY.xattr.".to_vec();
                    key.extend_from_slice(k);
                    pax.insert(key, v.clone());
                }
                headers
                    .header(&writer::Header {
                        name: e.path.clone(),
                        typeflag: match e.kind {
                            Type::File => b'0',
                            Type::HardLink => b'1',
                            Type::Symlink => b'2',
                            Type::CharDevice => b'3',
                            Type::BlockDevice => b'4',
                            Type::Dir => b'5',
                            Type::Fifo => b'6',
                        },
                        linkname: e.link.clone(),
                        mode: i64::from(e.mode),
                        uid: i64::from(e.uid),
                        gid: i64::from(e.gid),
                        size: 0,
                        mtime: e.mtime,
                        devmajor: i64::from(e.devmajor),
                        devminor: i64::from(e.devminor),
                        pax,
                        format: writer::Format::Unspecified,
                    })
                    .unwrap();
                if e.kind == Type::File && e.size > 0 {
                    let pad = (BLOCK - at % BLOCK) % BLOCK;
                    data.write_all(&vec![0; pad as usize]).unwrap();
                    at += pad + r.copy_data(&mut data).unwrap();
                    chunks += e.size.div_ceil(BLOCK);
                }
            }
        }
        check(d, layer);
        durable(&data.into_inner().unwrap());
        (headers.finish().unwrap(), chunks)
    });
    let mut tree = layer::root();
    for (i, (headers, _)) in layers.iter().enumerate() {
        layer::apply(&mut tree, i as u32, Cursor::new(&headers[..]), &mut |_| Ok(())).unwrap();
        tree.compact();
    }
    let mut file = BufWriter::with_capacity(1 << 20, File::create(out.join("image")).unwrap());
    let w = erofs::write(&tree, &mut Nothing, &mut file).unwrap();
    if std::env::var_os("SIZES").is_some() {
        eprintln!("blobs nodes {} inodes {}", tree.len(), w.inodes);
    }
    let index: u64 = layers.iter().map(|(_, c)| c * 8).sum();
    file.write_all(&vec![0; index as usize]).unwrap();
    durable(&file.into_inner().unwrap());
}

/// Made durable as the store makes its images: F_FULLFSYNC on macOS, fsync elsewhere.
fn durable(file: &File) {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == 0 {
            return;
        }
    }
    file.sync_all().unwrap();
}

fn cpu_ms() -> f64 {
    let u = usage();
    let ms = |t: libc::timeval| t.tv_sec as f64 * 1e3 + t.tv_usec as f64 / 1e3;
    ms(u.ru_utime) + ms(u.ru_stime)
}

fn rss_mb() -> f64 {
    // Bytes on macOS, KiB on Linux.
    let r = usage().ru_maxrss as f64;
    if cfg!(target_os = "macos") { r / 1e6 } else { r * 1024.0 / 1e6 }
}

fn usage() -> libc::rusage {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) }, 0);
    u
}

