//! How fast a guest can fail a device, reset it and fail it again, and what each failure
//! writes to the host's log (audit V07): a virtio-blk device behind its MMIO transport,
//! driven as a driver drives it, its queue's available index 100 ahead of a ring of 8
//! (AvailIndexJump). Each cycle writes STATUS 0, sets the device up, writes DRIVER_OK and
//! waits for DEVICE_NEEDS_RESET. Runs `--seconds` and prints
//! `{"cycles": N, "seconds": S, "log_bytes": B, "log_lines": L}`.
#![allow(clippy::unwrap_used, clippy::print_stdout)]

use std::sync::Arc;
use std::time::{Duration, Instant};

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

fn write(t: &MmioTransport, offset: u64, v: u32) {
    t.write(offset, &v.to_le_bytes());
}

fn status(t: &MmioTransport) -> u32 {
    let mut b = [0u8; 4];
    t.read(STATUS, &mut b);
    u32::from_le_bytes(b)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let seconds: f64 = args
        .iter()
        .position(|a| a == "--seconds")
        .map_or(5.0, |i| args[i + 1].parse().unwrap());
    let dir = std::env::temp_dir().join(format!("device-reset-log-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("log");
    shards_vmm::log::init();
    shards_vmm::log::to(std::fs::File::create(&log).unwrap());
    let disk = dir.join("disk");
    std::fs::write(&disk, vec![0u8; 8 * 512]).unwrap();
    let mem = Arc::new(GuestMemory::anonymous(&[(BASE, 1 << 20)]).unwrap());
    // avail.idx: 100 published on a ring of 8.
    mem.access()
        .unwrap()
        .write(BASE + 0x1000 + 2, &100u16.to_le_bytes())
        .unwrap();
    let block = Block::open(&disk, true, "reset").unwrap();
    let t = MmioTransport::new(Box::new(block), mem, Arc::new(Line));
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let t0 = Instant::now();
    let mut cycles = 0u64;
    while Instant::now() < deadline {
        write(&t, STATUS, 0);
        write(&t, STATUS, 1 | 2);
        write(&t, 0x024, 1); // DRIVER_FEATURES_SEL: the high word
        write(&t, 0x020, 1); // VERSION_1
        write(&t, 0x024, 0);
        write(&t, 0x020, 0);
        write(&t, STATUS, 1 | 2 | 8);
        write(&t, 0x030, 0); // QUEUE_SEL
        write(&t, 0x038, 8); // QUEUE_NUM
        for (low, addr) in [(0x080, BASE), (0x090, BASE + 0x1000), (0x0a0, BASE + 0x2000)] {
            write(&t, low, addr as u32);
            write(&t, low + 4, (addr >> 32) as u32);
        }
        write(&t, 0x044, 1); // QUEUE_READY
        write(&t, STATUS, 1 | 2 | 8 | 4);
        while status(&t) & 64 == 0 {
            std::hint::spin_loop();
        }
        cycles += 1;
    }
    let elapsed = t0.elapsed().as_secs_f64();
    write(&t, STATUS, 0);
    let text = std::fs::read_to_string(&log).unwrap();
    println!(
        "{{\"cycles\": {cycles}, \"seconds\": {elapsed:.3}, \"log_bytes\": {}, \"log_lines\": {}}}",
        text.len(),
        text.lines().count()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
