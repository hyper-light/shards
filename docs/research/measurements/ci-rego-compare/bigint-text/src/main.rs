use num_bigint::BigUint;
use std::time::Instant;
fn main() {
    for bits in [100_000u64, 332_193, 1_000_000, 3_321_929, 10_000_000] {
        let x = (BigUint::from(0xdead_beef_1234_5677u64)) << bits;
        let t = Instant::now();
        let s = x.to_str_radix(10);
        let conv = t.elapsed();
        let t = Instant::now();
        let p = BigUint::from(5u32).pow((bits / 2) as u32);
        let pow = t.elapsed();
        println!("bits={bits} digits={} to_str_radix={:?} 5^(bits/2)={:?} ({} bits)", s.len(), conv, pow, p.bits());
    }
}
