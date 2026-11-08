//! Reproduces save/from_file behavior for accepted, unsorted guest RAM ranges.

use std::error::Error;
use std::fs::OpenOptions;

use shards_vmm::{memory::GuestMemory, platform};

fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args_os().nth(1).ok_or("usage: range-order FILE")?;
    let page = platform::page_size()?;
    let (low, high) = (0x8000_0000, 0x9000_0000);
    let ranges = [(high, page), (low, page)];
    let memory = GuestMemory::anonymous(&ranges)?;
    memory.write(low, b"L")?;
    memory.write(high, b"H")?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    memory.save(&file)?;
    let mut saved_low = [0];
    let mut saved_high = [0];
    platform::read_exact_at(&file, &mut saved_low, 0)?;
    platform::read_exact_at(&file, &mut saved_high, page as u64)?;
    // The save must contain the original low/high tags in the documented region order.
    assert_eq!((saved_low[0], saved_high[0]), (b'L', b'H'));
    let restored = GuestMemory::from_file(&ranges, &file)?;
    let (actual_low, actual_high) = (restored.read_obj::<u8>(low)?, restored.read_obj::<u8>(high)?);
    drop(restored);
    drop(memory);
    drop(file);
    // Reuse a target with old bytes in a page that is now all zero in guest RAM.
    std::fs::write(&path, vec![0xaa; 2 * page])?;
    let file = OpenOptions::new().read(true).write(true).open(&path)?;
    let memory = GuestMemory::anonymous(&[(low, 2 * page)])?;
    memory.write(low, b"L")?;
    memory.save(&file)?;
    let restored = GuestMemory::from_file(&[(low, 2 * page)], &file)?;
    let saved_zero_page = restored.read_obj::<u8>(low + page as u64)?;
    println!(
        "{{\"input_range_order\":\"high,low\",\"page_bytes\":{page},\"expected_low\":76,\"expected_high\":72,\"actual_low\":{actual_low},\"actual_high\":{actual_high},\"correct\":{},\"reused_file_expected_zero\":0,\"reused_file_actual\":{saved_zero_page},\"reused_file_correct\":{}}}",
        (actual_low, actual_high) == (b'L', b'H'),
        saved_zero_page == 0
    );
    drop(restored);
    drop(memory);
    drop(file);
    std::fs::remove_file(path)?;
    Ok(())
}
