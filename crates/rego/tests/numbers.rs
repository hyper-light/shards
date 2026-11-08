//! number.rs against what Go's big.Float and OPA make of testdata/numbers.json
//! (testdata/numbers-oracle.json, written by scripts/rego/generate).

use shards_rego::number::Float;

fn text(f: &Float) -> String {
    f.text(if f.is_int() { b'f' } else { b'g' })
}

#[test]
fn numbers_read_and_write_as_go_reads_and_writes_them() {
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("../testdata/numbers-oracle.json")).unwrap();
    let mut failed = Vec::new();
    for n in oracle["numbers"].as_array().unwrap() {
        let s = n["s"].as_str().unwrap();
        let got = Float::parse(s);
        if !n["ok"].as_bool().unwrap() {
            if got.is_ok() {
                failed.push(format!("{s:?}: read, Go refuses it"));
            }
            continue;
        }
        let Ok(f) = got else {
            failed.push(format!("{s:?}: refused, Go reads it"));
            continue;
        };
        let want = |k: &str| n.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let flag = |k: &str| n.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
        let (int, exact) = f.int();
        let got = (text(&f), f.is_int(), int.to_string(), exact, f.signbit());
        let wanted = (
            want("text"),
            flag("is_int"),
            want("int"),
            flag("exact"),
            flag("neg"),
        );
        if got != wanted {
            failed.push(format!("{s:?}: {got:?}, Go {wanted:?}"));
        }
    }
    for p in oracle["pairs"].as_array().unwrap() {
        let (a, b) = (p["a"].as_str().unwrap(), p["b"].as_str().unwrap());
        let (x, y) = (Float::parse(a).unwrap(), Float::parse(b).unwrap());
        let prec = Float::max_prec(&x, &y);
        let ops = [
            ("add", Float::add(&x, &y, prec)),
            ("sub", Float::sub(&x, &y, prec)),
            ("mul", Float::mul(&x, &y, prec)),
        ];
        for (op, r) in ops {
            let got = text(&r.unwrap());
            if got != p[op].as_str().unwrap() {
                failed.push(format!("{a} {op} {b}: {got}, Go {}", p[op]));
            }
        }
        if let Some(q) = p.get("quo").and_then(|v| v.as_str()) {
            let got = text(&Float::quo(&x, &y, prec).unwrap());
            if got != q {
                failed.push(format!("{a} quo {b}: {got}, Go {q}"));
            }
        }
        let compare = shards_rego::value::number_compare(a, b) as i64;
        if compare != p["compare"].as_i64().unwrap() {
            failed.push(format!(
                "NumberCompare({a}, {b}): {compare}, OPA {}",
                p["compare"]
            ));
        }
        let cmp = Float::compare(&x, &y) as i64;
        if cmp != p["cmp"].as_i64().unwrap() {
            failed.push(format!("{a} cmp {b}: {cmp}, Go {}", p["cmp"]));
        }
    }
    for q in oracle["quote"].as_array().unwrap() {
        let (s, want) = (q[0].as_str().unwrap(), q[1].as_str().unwrap());
        let mut got = String::new();
        shards_rego::goquote::quote(&mut got, s);
        if got != want {
            failed.push(format!("strconv.Quote({s:?}): {got}, Go {want}"));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

#[test]
fn every_opa_builtin_is_read() {
    let r = shards_rego::builtins::registry();
    assert_eq!(r.len(), 203);
    assert_eq!(r.values().filter(|b| b.allowed).count(), 144);
}
