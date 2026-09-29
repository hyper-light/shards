//! Container image layers flattened by shards-image into one EROFS image, mounted by a
//! real guest kernel over virtio-pmem, and checked from inside against the manifest the
//! top layer carries: the host's whole path from layer archives to what a guest sees.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::fs::File;
use std::io::{self, BufReader, Write};
use std::time::Duration;

use common::{TempDir, cannot_run_vms, kernel, test_guest, vm_run};
use shards_image::{erofs, layer};

const TIMEOUT: Duration = Duration::from_secs(60);

/// Writes a ustar archive, as layer tools do.
#[derive(Default)]
struct Tar(Vec<u8>);

#[derive(Clone, Copy)]
struct Header<'a> {
    name: &'a str,
    flag: u8,
    mode: u32,
    owner: (u32, u32),
    link: &'a str,
    dev: (u32, u32),
}

impl Tar {
    fn put(&mut self, h: Header<'_>, data: &[u8]) -> &mut Tar {
        let mut b = [0u8; 512];
        let mut octal = |at: usize, len: usize, v: u64| {
            b[at..at + len].copy_from_slice(format!("{v:0w$o}\0", w = len - 1).as_bytes());
        };
        octal(100, 8, h.mode.into());
        octal(108, 8, h.owner.0.into());
        octal(116, 8, h.owner.1.into());
        octal(124, 12, data.len() as u64);
        octal(136, 12, 1_700_000_000);
        octal(329, 8, h.dev.0.into());
        octal(337, 8, h.dev.1.into());
        b[..h.name.len()].copy_from_slice(h.name.as_bytes());
        b[156] = h.flag;
        b[157..157 + h.link.len()].copy_from_slice(h.link.as_bytes());
        b[257..265].copy_from_slice(b"ustar\x0000");
        b[148..156].fill(b' ');
        let sum: u64 = b.iter().map(|&c| u64::from(c)).sum();
        b[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        self.0.extend_from_slice(&b);
        self.0.extend_from_slice(data);
        self.0.resize(self.0.len().next_multiple_of(512), 0);
        self
    }

    fn entry(&mut self, name: &str, flag: u8, mode: u32, owner: (u32, u32)) -> &mut Tar {
        let h = Header {
            name,
            flag,
            mode,
            owner,
            link: "",
            dev: (0, 0),
        };
        self.put(h, &[])
    }

    fn dir(&mut self, name: &str, mode: u32, owner: (u32, u32)) -> &mut Tar {
        self.entry(name, b'5', mode, owner)
    }

    /// A file of `size` pattern bytes, which the guest checks by `salt`.
    fn file(&mut self, name: &str, mode: u32, owner: (u32, u32), size: usize, salt: u64) -> &mut Tar {
        let mut data = vec![0; size];
        shards_testguest::fill(salt, 0, &mut data);
        let h = Header {
            name,
            flag: b'0',
            mode,
            owner,
            link: "",
            dev: (0, 0),
        };
        self.put(h, &data)
    }

    /// A symlink (`b'2'`) or hard link (`b'1'`); a hard link carries its file's mode, as
    /// tar writers give every name of a file the file's attributes.
    fn link(&mut self, name: &str, flag: u8, mode: u32, target: &str) -> &mut Tar {
        let h = Header {
            name,
            flag,
            mode,
            owner: (0, 0),
            link: target,
            dev: (0, 0),
        };
        self.put(h, &[])
    }

    /// PAX records for the next entry.
    fn pax(&mut self, records: &[(&str, &[u8])]) -> &mut Tar {
        let mut body = Vec::new();
        for (key, value) in records {
            let rest = [b" ", key.as_bytes(), b"=", value, b"\n"].concat();
            let mut len = rest.len() + 1;
            while len.to_string().len() + rest.len() != len {
                len += 1;
            }
            body.extend_from_slice(len.to_string().as_bytes());
            body.extend_from_slice(&rest);
        }
        let h = Header {
            name: "PaxHeaders/next",
            flag: b'x',
            mode: 0o644,
            owner: (0, 0),
            link: "",
            dev: (0, 0),
        };
        self.put(h, &body)
    }

    fn finish(&mut self) -> Vec<u8> {
        let mut out = std::mem::take(&mut self.0);
        out.extend_from_slice(&[0; 1024]);
        out
    }
}

const ROOT: (u32, u32) = (0, 0);
const CAPABILITY: [u8; 20] = [1, 0, 0, 2, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// Three layers, as a distribution, a package install and an application write them, and
/// the manifest of the tree they stack to (the guest's `erofs` check).
fn layers() -> (Vec<Vec<u8>>, String) {
    let base = Tar::default()
        .dir("etc/", 0o755, ROOT)
        .file("etc/hostname", 0o644, ROOT, 12, 1)
        .file("etc/passwd", 0o644, ROOT, 700, 2)
        .file("etc/shadow", 0o640, (0, 42), 300, 3)
        .dir("usr/", 0o755, ROOT)
        .dir("usr/lib/", 0o755, ROOT)
        .file("usr/lib/libc.so.6", 0o755, ROOT, (2 << 20) + 123, 4)
        .link("lib", b'2', 0o777, "usr/lib")
        .dir("usr/bin/", 0o755, ROOT)
        .file("usr/bin/tool", 0o755, ROOT, 5000, 5)
        .link("usr/bin/tool-alias", b'1', 0o755, "usr/bin/tool")
        .dir("var/", 0o755, ROOT)
        .dir("var/cache/", 0o755, ROOT)
        .file("var/cache/stale", 0o644, ROOT, 10, 6)
        .dir("var/cache/sub/", 0o755, ROOT)
        .file("var/cache/sub/stale", 0o644, ROOT, 10, 7)
        .dir("tmp/", 0o1777, ROOT)
        .put(
            Header {
                name: "dev/null",
                flag: b'3',
                mode: 0o666,
                owner: ROOT,
                link: "",
                dev: (1, 3),
            },
            &[],
        )
        .finish();
    let install = Tar::default()
        .dir("etc/", 0o755, ROOT)
        .file("etc/hostname", 0o644, ROOT, 20, 8)
        .file("etc/.wh.shadow", 0o644, ROOT, 0, 0)
        .dir("usr/bin/", 0o755, ROOT)
        .pax(&[("SCHILY.xattr.security.capability", &CAPABILITY)])
        .file("usr/bin/ping", 0o4755, ROOT, 64, 9)
        .dir("var/cache/", 0o755, ROOT)
        .file("var/cache/.wh..wh..opq", 0o644, ROOT, 0, 0)
        .file("var/cache/fresh", 0o644, ROOT, 10, 10)
        .dir("opt/", 0o755, ROOT)
        .dir("opt/app/", 0o750, (1000, 1000))
        .file("opt/app/data.bin", 0o640, (1000, 1000), 3 * 4096, 11)
        .finish();
    let capability: String = CAPABILITY.iter().map(|b| format!("{b:02x}")).collect();
    let manifest: String = [
        "d /etc 755 0 0",
        "f /etc/hostname 644 0 0 20 8",
        "f /etc/passwd 644 0 0 700 2",
        "d /usr 755 0 0",
        "d /usr/lib 755 0 0",
        "f /usr/lib/libc.so.6 755 0 0 2097275 4",
        "l /lib usr/lib",
        "d /usr/bin 755 0 0",
        // Replacing one name of a hard link leaves the other with the old file.
        "f /usr/bin/tool 755 0 0 100 12",
        "f /usr/bin/tool-alias 755 0 0 5000 5",
        "f /usr/bin/new 755 0 0 30 13",
        "h /usr/bin/new-alias /usr/bin/new",
        "f /usr/bin/ping 4755 0 0 64 9",
        &format!("x /usr/bin/ping security.capability {capability}"),
        "d /var 755 0 0",
        "d /var/cache 755 0 0",
        "f /var/cache/fresh 644 0 0 10 10",
        "d /tmp 1777 0 0",
        "d /dev 755 0 0",
        "c /dev/null 1 3",
        "d /opt 755 0 0",
        "d /opt/app 750 1000 1000",
        "f /opt/app/data.bin 640 1000 1000 12288 11",
    ]
    .map(|line| format!("{line}\n"))
    .concat();
    let app = Tar::default()
        .dir("usr/bin/", 0o755, ROOT)
        .file("usr/bin/tool", 0o755, ROOT, 100, 12)
        .file("usr/bin/new", 0o755, ROOT, 30, 13)
        .link("usr/bin/new-alias", b'1', 0o755, "usr/bin/new")
        .put(
            Header {
                name: "MANIFEST",
                flag: b'0',
                mode: 0o444,
                owner: ROOT,
                link: "",
                dev: (0, 0),
            },
            manifest.as_bytes(),
        )
        .finish();
    (vec![base, install, app], manifest)
}

#[test]
fn a_guest_sees_layers_stacked_as_containerd_stacks_them() {
    if cannot_run_vms() {
        return;
    }
    let dir = TempDir::new("layers");
    let (archives, manifest) = layers();
    let mut tree = layer::root();
    let mut files = Vec::new();
    for (i, bytes) in archives.iter().enumerate() {
        let path = dir.join(format!("layer-{i}.tar"));
        std::fs::write(&path, bytes).unwrap();
        layer::apply(&mut tree, i as u32, BufReader::new(File::open(&path).unwrap())).unwrap();
        files.push(File::open(&path).unwrap());
    }
    let image = dir.join("image.erofs");
    let mut out = io::BufWriter::new(File::create(&image).unwrap());
    erofs::write(&tree, &mut layer::Archives(files), &mut out).unwrap();
    out.flush().unwrap();
    drop(out);
    let r = vm_run(
        &[
            "--kernel".as_ref(),
            kernel().as_os_str(),
            "--init".as_ref(),
            test_guest().as_os_str(),
            "--memory".as_ref(),
            "256".as_ref(),
            "--pmem".as_ref(),
            image.as_os_str(),
            "--cmdline".as_ref(),
            "console=ttyS0 quiet shards_test=erofs".as_ref(),
        ],
        TIMEOUT,
    );
    assert!(r.stdout.contains("SHARDS-TEST PASS"), "{r}");
    let entries = manifest.lines().count();
    assert!(r.stdout.contains(&format!("checked {entries} entries")), "{r}");
}
