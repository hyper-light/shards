use std::fmt::Display;
use std::io::Write;
#[path="allocation.rs"] mod faults;
fn report(message: impl Display) {
    let _ = writeln!(std::io::stderr(), "shards: {message}");
}
fn main() {
 let mode=std::env::args().nth(1).expect("explicit private probe mode");
 match mode.as_str() {
  "ordinary" => report("private static refusal probe"),
  "cold-refused" => faults::fail_after(0, || report("private static refusal probe")),
  "warmed-control" => {
   report("private warmed negative control; never a remedy");
   faults::fail_after(0, || report("private static refusal probe"));
  },
  _ => panic!("unknown private probe mode"),
 }
}
