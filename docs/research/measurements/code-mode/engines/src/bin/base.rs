//! No engine: what a static binary of this toolchain costs, for the others' sizes and peaks.
fn main() {
    println!("base peak_kib={}", engines::peak_kib());
}
