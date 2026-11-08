static GUEST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/guest.bin"));
fn main() {
    print!("{}", include_str!(concat!(env!("OUT_DIR"), "/report.txt")));
    println!("EXP_CONFIG at compile time: {}", option_env!("EXP_CONFIG").unwrap_or("unset"));
    println!("GUEST len {}", GUEST.len());
}
