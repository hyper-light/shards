//! Rego's builtins as a policy calls them, with any operands (shards_rego::funcs, D98):
//! no panic. The input is the builtin's name, a newline, and its operands as a JSON
//! array, where `{"$set": [...]}` is a set.
#![no_main]

use libfuzzer_sys::fuzz_target;
use shards_rego::funcs::{self, Context};
use shards_rego::value::{self, Value};

mod holdable;

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
    let mut ctx = Context {
        time_ns: 1_700_000_000_000_000_000,
        seed: vec![7; 64],
        holdable: holdable::holdable(),
        ..Default::default()
    };
    if let Ok(Some(v)) = builtin(&mut ctx, args) {
        let _ = value::to_json(&v);
    }
});
