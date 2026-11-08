//! A guest that breaks a device's ring after every reset writes one line of the host's
//! log, not one a reset (audit V07): a test of its own, whose process's log goes to a file.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use shards_vmm::devices::virtio::block::Block;
use shards_vmm::devices::virtio::mmio::MmioTransport;
use shards_vmm::devices::{Interrupt, MmioDevice};
use shards_vmm::memory::GuestMemory;

struct Line;
impl Interrupt for Line {
    fn set_level(&self, _: bool) {}
}

const BASE: u64 = 0x8000_0000;
const STATUS: u64 = 0x070;
const DEVICE_NEEDS_RESET: u32 = 64;

fn write(t: &MmioTransport, offset: u64, v: u32) {
    t.write(offset, &v.to_le_bytes());
}

fn status(t: &MmioTransport) -> u32 {
    let mut b = [0u8; 4];
    t.read(STATUS, &mut b);
    u32::from_le_bytes(b)
}

#[test]
fn a_device_failing_after_every_reset_is_said_once() {
    let dir = shards_testdir::TempDir::new("reset-log").unwrap();
    let log = dir.join("log");
    shards_vmm::log::init();
    shards_vmm::log::to(std::fs::File::create(&log).unwrap());
    let disk = dir.join("disk");
    std::fs::write(&disk, vec![0u8; 8 * 512]).unwrap();
    let mem = Arc::new(GuestMemory::anonymous(&[(BASE, 1 << 20)]).unwrap());
    // avail.idx: 100 published on a ring of 8, which fails the device as it starts.
    mem.access()
        .unwrap()
        .write(BASE + 0x1000 + 2, &100u16.to_le_bytes())
        .unwrap();
    let t = MmioTransport::new(
        Box::new(Block::open(&disk, true, "reset").unwrap()),
        mem,
        Arc::new(Line),
    );
    for _ in 0..20 {
        write(&t, STATUS, 0);
        write(&t, STATUS, 1 | 2);
        write(&t, 0x024, 1);
        write(&t, 0x020, 1); // VERSION_1
        write(&t, 0x024, 0);
        write(&t, 0x020, 0);
        write(&t, STATUS, 1 | 2 | 8);
        write(&t, 0x030, 0);
        write(&t, 0x038, 8);
        for (low, addr) in [(0x080, BASE), (0x090, BASE + 0x1000), (0x0a0, BASE + 0x2000)] {
            write(&t, low, addr as u32);
            write(&t, low + 4, (addr >> 32) as u32);
        }
        write(&t, 0x044, 1);
        write(&t, STATUS, 1 | 2 | 8 | 4);
        let t0 = std::time::Instant::now();
        while status(&t) & DEVICE_NEEDS_RESET == 0 {
            assert!(t0.elapsed().as_secs() < 10, "the device never failed");
            std::thread::yield_now();
        }
    }
    write(&t, STATUS, 0);
    let text = std::fs::read_to_string(&log).unwrap();
    let said = text.lines().filter(|l| l.contains("needs reset")).count();
    assert_eq!(said, 1, "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}
