//! Whether `sort` of numbers whose order is not total panics: OPA's trimmed-float shortcut
//! ("10.0" equal to "1" and to "10", while "1" < "10") and numbers exact_parts cannot read
//! (exponents past i64), ordered by text.
use shards_rego::funcs::{self, Context};
use shards_rego::value::{Number, Value};
use std::panic;

fn main() {
    let all = ["1", "10", "10.0", "100", "100.00", "1.0", "2", "20.0", "0.10", "0.1",
        "1e99999999999999999999", "5e99999999999999999999", "1e-99999999999999999999",
        "3", "30", "300.0", "0", "-1", "-10.0", "9"];
    let quirk: Vec<&str> = all.iter().copied().filter(|t| !t.contains("e9") && !t.contains("e-9")).collect();
    let unread: Vec<&str> = all.iter().copied().filter(|t| !t.contains('.')).collect();
    let which = std::env::args().nth(1).unwrap_or_default();
    let texts: Vec<&str> = match which.as_str() { "quirk" => quirk, "unread" => unread, _ => all.to_vec() };
    println!("{which}: {}", texts.join(" "));
    let sort = funcs::lookup("sort").unwrap();
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = |n: usize| { state ^= state >> 12; state ^= state << 25; state ^= state >> 27;
        (state.wrapping_mul(0x2545_F491_4F6C_DD1D) % n as u64) as usize };
    panic::set_hook(Box::new(|_| {}));
    let (mut panics, mut runs) = (0, 0);
    let mut first = None;
    for _ in 0..200_000 {
        let len = 2 + next(120);
        let items: Vec<Value> = (0..len).map(|_| Value::Number(Number(texts[next(texts.len())].into()))).collect();
        runs += 1;
        let arr = Value::array(items.clone());
        let r = panic::catch_unwind(|| { let mut ctx = Context::default(); let _ = sort(&mut ctx, &[arr]); });
        if r.is_err() {
            panics += 1;
            if first.is_none() { first = Some(items.iter().map(|v| match v { Value::Number(n) => n.text().to_string(), _ => String::new() }).collect::<Vec<_>>()); }
        }
    }
    println!("{panics} panics in {runs} sorts");
    if let Some(f) = first { println!("first: {}", f.join(",")); }
}
