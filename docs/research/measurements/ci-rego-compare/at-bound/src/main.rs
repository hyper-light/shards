use shards_rego::funcs::{self, Context};
use shards_rego::value::{self, Value};
use std::time::Instant;

fn main() {
    let cases = [
        ("abs", "[1e30102]"), ("abs", "[1e-30103]"), ("ceil", "[1e30102]"), ("round", "[-1e30102]"),
        ("abs", "[1e-30000]"), ("plus", "[1e30102,1e-30103]"), ("div", "[1,3e30102]"),
        ("bits.lsh", "[1,99999]"), ("format_int", "[1e30102,10]"), ("units.parse_bytes", "[\"1e30102\"]"),
        ("lt", "[33,3e33333333332]"), ("lt", "[\"x\",1]"), ("abs", "[9e9999995]"), ("ceil", "[266000000e2028020]"),
    ];
    for (name, args) in cases {
        let f = funcs::lookup(name).unwrap();
        let doc = value::from_json(args).unwrap();
        let Value::Array(args) = &doc else { return };
        let mut times = Vec::new();
        let mut len = 0;
        for _ in 0..21 {
            let mut ctx = Context::default();
            let t = Instant::now();
            let r = f(&mut ctx, args);
            times.push(t.elapsed().as_secs_f64() * 1e3);
            len = match &r { Ok(Some(Value::Number(n))) => n.text().len(), Ok(Some(_)) => 1, _ => 0 };
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("{name}{}: n=21 p50 {:.3} p90 {:.3} max {:.3} ms, text {len} chars", value::to_json(&doc).unwrap(), times[10], times[18], times[20]);
    }
}
