//! Rego's builtins as a policy calls them, with any operands (shards_rego::funcs, D98):
//! no panic. The input is the builtin's name, a newline, and its operands as a JSON
//! array, where `{"$set": [...]}` is a set.
#![no_main]

use libfuzzer_sys::fuzz_target;
use shards_rego::funcs::{self, Context};
use shards_rego::number::Float;
use shards_rego::value::{self, Value};

/// A set where the JSON says `{"$set": [...]}`, every value within decoded too.
fn decode(v: Value) -> Value {
    match &v {
        Value::Array(a) => Value::array(a.iter().cloned().map(decode).collect()),
        Value::Object(o) => {
            if o.len() == 1
                && let Some(Value::Array(items)) = o.get(&Value::string("$set"))
            {
                return Value::set(items.iter().cloned().map(decode).collect());
            }
            Value::object(o.iter().map(|(k, v)| (k.clone(), decode(v.clone()))).collect())
        }
        _ => v.clone(),
    }
}

/// `numbers.range` and `numbers.range_step` make as many numbers as their operands ask,
/// as OPA's do (docs/design/architecture.md D98: a policy may ask for work without
/// bound, M510): asked here for at most one number per byte of the input.
fn asks_for_more_than(budget: usize, name: &str, args: &[Value]) -> bool {
    let step_at = match name {
        "numbers.range" => None,
        "numbers.range_step" => Some(2),
        _ => return false,
    };
    let float = |i: usize| match args.get(i) {
        Some(Value::Number(n)) => n.to_float().ok(),
        _ => None,
    };
    let one = Float::from_u64(1, 64).ok();
    let (Some(x), Some(y), Some(step)) = (float(0), float(1), step_at.map_or(one, float)) else {
        return false;
    };
    if step.is_zero() || step.signbit() {
        return false;
    }
    let count = Float::sub(&y, &x, 64).and_then(|span| Float::quo(&span.abs(), &step, 64));
    let most = Float::from_u64(u64::try_from(budget).unwrap_or(u64::MAX), 64);
    match (count, most) {
        (Ok(count), Ok(most)) => Float::compare(&count, &most).is_gt(),
        _ => true,
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Some((name, args)) = text.split_once('\n') else {
        return;
    };
    let Some(builtin) = funcs::lookup(name) else {
        return;
    };
    let Ok(args) = value::from_json(args).map(decode) else {
        return;
    };
    let Value::Array(args) = &args else {
        return;
    };
    if asks_for_more_than(data.len(), name, args) {
        return;
    }
    let mut ctx = Context {
        time_ns: 1_700_000_000_000_000_000,
        seed: vec![7; 64],
        ..Default::default()
    };
    if let Ok(Some(v)) = builtin(&mut ctx, args) {
        let _ = value::to_json(&v);
    }
});
