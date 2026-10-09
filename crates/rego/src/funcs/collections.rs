//! OPA's builtins over collections, numbers, types and comparisons (topdown
//! aggregates.go, array.go, binary.go, casts.go, comparison.go, sets.go, type.go,
//! type_name.go), with numbers (arithmetic.go, bits.go, numbers.go), objects
//! (object.go, subset.go) and texts (semver.go, uuid.go, template_string.go) in the
//! submodules.

use std::collections::BTreeSet;

use super::{Builtin, BuiltinError, Context, arg};
use crate::value::Value;

mod gorand;
mod numbers;
mod objects;
mod text;

pub fn lookup(name: &str) -> Option<Builtin> {
    let f: Builtin = match name {
        "abs" => numbers::abs,
        "round" => numbers::round,
        "ceil" => numbers::ceil,
        "floor" => numbers::floor,
        "plus" => numbers::plus,
        "minus" => numbers::minus,
        "mul" => numbers::mul,
        "div" => numbers::div,
        "rem" => numbers::rem,
        "bits.and" => numbers::bits_and,
        "bits.or" => numbers::bits_or,
        "bits.xor" => numbers::bits_xor,
        "bits.negate" => numbers::bits_negate,
        "bits.lsh" => numbers::bits_lsh,
        "bits.rsh" => numbers::bits_rsh,
        "numbers.range" => numbers::range,
        "numbers.range_step" => numbers::range_step,
        "rand.intn" => numbers::rand_intn,
        "to_number" => numbers::to_number,
        "count" => count,
        "sum" => numbers::sum,
        "product" => numbers::product,
        "max" => max,
        "min" => min,
        "sort" => sort,
        "all" => all,
        "any" => any,
        "internal.member_2" => member,
        "internal.member_3" => member_with_key,
        "array.concat" => array_concat,
        "array.flatten" => array_flatten,
        "array.slice" => array_slice,
        "array.reverse" => array_reverse,
        "and" => and,
        "or" => or,
        "set_diff" => set_diff,
        "intersection" => intersection,
        "union" => union,
        "cast_array" => cast_array,
        "cast_set" => cast_set,
        "cast_string" => cast_string,
        "cast_boolean" => cast_boolean,
        "cast_null" => cast_null,
        "cast_object" => cast_object,
        "is_number" => is_number,
        "is_string" => is_string,
        "is_boolean" => is_boolean,
        "is_array" => is_array,
        "is_set" => is_set,
        "is_object" => is_object,
        "is_null" => is_null,
        "type_name" => type_name,
        "gt" => gt,
        "gte" => gte,
        "lt" => lt,
        "lte" => lte,
        "neq" => neq,
        "equal" => equal,
        "object.union" => objects::union,
        "object.union_n" => objects::union_n,
        "object.remove" => objects::remove,
        "object.filter" => objects::filter,
        "object.get" => objects::get,
        "object.keys" => objects::keys,
        "object.subset" => objects::subset,
        "semver.compare" => text::semver_compare,
        "semver.is_valid" => text::semver_is_valid,
        "uuid.rfc4122" => text::uuid_rfc4122,
        "uuid.parse" => text::uuid_parse,
        "internal.template_string" => text::template_string,
        _ => return None,
    };
    Some(f)
}

type Out = Result<Option<Value>, BuiltinError>;

fn ok(v: Value) -> Out {
    Ok(Some(v))
}

/// builtins.NewOperandElementErr with one expected type.
fn element_err(pos: usize, composite: &Value, got: &Value, expected: &str) -> BuiltinError {
    let tpe = composite.type_name();
    BuiltinError::operand(
        pos,
        format!(
            "must be {tpe} of {expected}s but got {tpe} containing {}",
            got.type_name()
        ),
    )
}

/// builtins.SetOperand.
fn set_operand(v: &Value, pos: usize) -> Result<&BTreeSet<Value>, BuiltinError> {
    match v {
        Value::Set(s) => Ok(s),
        _ => Err(BuiltinError::operand_type(pos, v, &["set"])),
    }
}

/// builtins.ArrayOperand.
fn array_operand(v: &Value, pos: usize) -> Result<&[Value], BuiltinError> {
    match v {
        Value::Array(a) => Ok(a),
        _ => Err(BuiltinError::operand_type(pos, v, &["array"])),
    }
}

fn count(_: &mut Context, args: &[Value]) -> Out {
    let a = arg(args, 0)?;
    let n = match a {
        Value::Array(x) => x.len(),
        Value::Object(x) => x.len(),
        Value::Set(x) => x.len(),
        Value::String(s) => s.chars().count(),
        _ => {
            return Err(BuiltinError::operand_type(
                1,
                a,
                &["array", "object", "set", "string"],
            ));
        }
    };
    ok(Value::int(i64::try_from(n).unwrap_or(i64::MAX)))
}

/// The elements of an array, or of a set in its order.
fn elements(v: &Value) -> Option<Vec<&Value>> {
    match v {
        Value::Array(a) => Some(a.iter().collect()),
        Value::Set(s) => Some(s.iter().collect()),
        _ => None,
    }
}

fn max(_: &mut Context, args: &[Value]) -> Out {
    let a = arg(args, 0)?;
    let Some(items) = elements(a) else {
        return Err(BuiltinError::operand_type(1, a, &["set", "array"]));
    };
    if items.is_empty() {
        return Ok(None);
    }
    // Both start from null and keep the last element not less than the maximum.
    let mut max = &Value::Null;
    for x in items {
        if max <= x {
            max = x;
        }
    }
    ok(max.clone())
}

fn min(_: &mut Context, args: &[Value]) -> Out {
    let a = arg(args, 0)?;
    match a {
        Value::Array(items) => {
            let Some(mut min) = items.first() else {
                return Ok(None);
            };
            for x in items.iter() {
                if min >= x {
                    min = x;
                }
            }
            ok(min.clone())
        }
        Value::Set(items) => {
            if items.is_empty() {
                return Ok(None);
            }
            // OPA reduces from null and takes any element over a null minimum, so
            // a set's null is its minimum only when it is all the set holds.
            let mut min = &Value::Null;
            for x in items.iter() {
                if *min == Value::Null || min >= x {
                    min = x;
                }
            }
            ok(min.clone())
        }
        _ => Err(BuiltinError::operand_type(1, a, &["set", "array"])),
    }
}

fn sort(_: &mut Context, args: &[Value]) -> Out {
    let a = arg(args, 0)?;
    match a {
        Value::Array(items) => {
            let mut v = items.to_vec();
            v.sort();
            ok(Value::array(v))
        }
        Value::Set(items) => ok(Value::array(items.iter().cloned().collect())),
        _ => Err(BuiltinError::operand_type(1, a, &["set", "array"])),
    }
}

fn all(_: &mut Context, args: &[Value]) -> Out {
    let a = arg(args, 0)?;
    let Some(items) = elements(a) else {
        return Err(BuiltinError::operand_type(1, a, &["array", "set"]));
    };
    ok(Value::Bool(items.iter().all(|x| **x == Value::Bool(true))))
}

fn any(_: &mut Context, args: &[Value]) -> Out {
    let a = arg(args, 0)?;
    let Some(items) = elements(a) else {
        return Err(BuiltinError::operand_type(1, a, &["array", "set"]));
    };
    ok(Value::Bool(items.iter().any(|x| **x == Value::Bool(true))))
}

fn member(_: &mut Context, args: &[Value]) -> Out {
    let x = arg(args, 0)?;
    let found = match arg(args, 1)? {
        Value::Set(s) => s.contains(x),
        Value::Array(a) => a.iter().any(|e| e == x),
        Value::Object(o) => o.values().any(|v| v == x),
        _ => false,
    };
    ok(Value::Bool(found))
}

fn member_with_key(_: &mut Context, args: &[Value]) -> Out {
    let (key, val) = (arg(args, 0)?, arg(args, 1)?);
    // Arrays and objects have Get; sets and scalars do not.
    let found = match arg(args, 2)? {
        c @ (Value::Array(_) | Value::Object(_)) => c.get(key).is_some_and(|v| v == val),
        _ => false,
    };
    ok(Value::Bool(found))
}

fn array_concat(_: &mut Context, args: &[Value]) -> Out {
    let a = array_operand(arg(args, 0)?, 1)?;
    let b = array_operand(arg(args, 1)?, 2)?;
    let mut out = Vec::with_capacity(a.len().saturating_add(b.len()));
    out.extend_from_slice(a);
    out.extend_from_slice(b);
    ok(Value::array(out))
}

fn array_flatten(_: &mut Context, args: &[Value]) -> Out {
    let a = array_operand(arg(args, 0)?, 1)?;
    let mut out = Vec::with_capacity(a.len());
    for x in a {
        match x {
            Value::Array(nested) => out.extend_from_slice(nested),
            x => out.push(x.clone()),
        }
    }
    ok(Value::array(out))
}

fn array_slice(_: &mut Context, args: &[Value]) -> Out {
    let a = array_operand(arg(args, 0)?, 1)?;
    let start = super::int_operand(arg(args, 1)?, 2)?;
    let stop = super::int_operand(arg(args, 2)?, 3)?;
    let len = i64::try_from(a.len()).unwrap_or(i64::MAX);
    let stop = stop.clamp(0, len);
    let start = start.clamp(0, stop);
    let (Ok(start), Ok(stop)) = (usize::try_from(start), usize::try_from(stop)) else {
        return ok(Value::array(Vec::new()));
    };
    ok(Value::array(a.get(start..stop).unwrap_or_default().to_vec()))
}

fn array_reverse(_: &mut Context, args: &[Value]) -> Out {
    let a = array_operand(arg(args, 0)?, 1)?;
    ok(Value::array(a.iter().rev().cloned().collect()))
}

/// ast.Set's Intersect: the elements of the smaller set (the first, when equal in
/// size) that the other holds.
fn intersect(s: &BTreeSet<Value>, o: &BTreeSet<Value>) -> BTreeSet<Value> {
    let (small, other) = if o.len() < s.len() { (o, s) } else { (s, o) };
    small.iter().filter(|x| other.contains(*x)).cloned().collect()
}

/// ast.Set's Diff.
fn diff(s: &BTreeSet<Value>, o: &BTreeSet<Value>) -> BTreeSet<Value> {
    s.iter().filter(|x| !o.contains(*x)).cloned().collect()
}

fn and(_: &mut Context, args: &[Value]) -> Out {
    let a = set_operand(arg(args, 0)?, 1)?;
    let b = set_operand(arg(args, 1)?, 2)?;
    ok(Value::set(intersect(a, b)))
}

fn or(_: &mut Context, args: &[Value]) -> Out {
    let a = set_operand(arg(args, 0)?, 1)?;
    let b = set_operand(arg(args, 1)?, 2)?;
    let mut out = a.clone();
    for x in b {
        if !out.contains(x) {
            out.insert(x.clone());
        }
    }
    ok(Value::set(out))
}

fn set_diff(_: &mut Context, args: &[Value]) -> Out {
    let a = set_operand(arg(args, 0)?, 1)?;
    let b = set_operand(arg(args, 1)?, 2)?;
    ok(Value::set(diff(a, b)))
}

fn intersection(_: &mut Context, args: &[Value]) -> Out {
    let input = set_operand(arg(args, 0)?, 1)?;
    let mut result: Option<BTreeSet<Value>> = None;
    for x in input {
        let n = set_operand(x, 1)?;
        result = Some(match result {
            None => n.clone(),
            Some(r) => intersect(&r, n),
        });
    }
    ok(Value::set(result.unwrap_or_default()))
}

fn union(_: &mut Context, args: &[Value]) -> Out {
    let input = set_operand(arg(args, 0)?, 1)?;
    let mut sets = Vec::with_capacity(input.len());
    for x in input {
        sets.push(set_operand(x, 1)?);
    }
    let mut out = BTreeSet::new();
    for s in sets {
        for x in s {
            if !out.contains(x) {
                out.insert(x.clone());
            }
        }
    }
    ok(Value::set(out))
}

fn cast_array(_: &mut Context, args: &[Value]) -> Out {
    let a = arg(args, 0)?;
    match a {
        Value::Array(_) => ok(a.clone()),
        Value::Set(s) => ok(Value::array(s.iter().cloned().collect())),
        _ => Err(BuiltinError::operand_type(1, a, &["array", "set"])),
    }
}

fn cast_set(_: &mut Context, args: &[Value]) -> Out {
    let a = arg(args, 0)?;
    match a {
        Value::Array(items) => {
            let mut s = BTreeSet::new();
            for x in items.iter() {
                if !s.contains(x) {
                    s.insert(x.clone());
                }
            }
            ok(Value::set(s))
        }
        Value::Set(_) => ok(a.clone()),
        _ => Err(BuiltinError::operand_type(1, a, &["array", "set"])),
    }
}

fn cast_to(args: &[Value], want: &str, is: fn(&Value) -> bool) -> Out {
    let a = arg(args, 0)?;
    if is(a) {
        ok(a.clone())
    } else {
        Err(BuiltinError::operand_type(1, a, &[want]))
    }
}

fn cast_string(_: &mut Context, args: &[Value]) -> Out {
    cast_to(args, "string", |v| matches!(v, Value::String(_)))
}

fn cast_boolean(_: &mut Context, args: &[Value]) -> Out {
    cast_to(args, "boolean", |v| matches!(v, Value::Bool(_)))
}

fn cast_null(_: &mut Context, args: &[Value]) -> Out {
    cast_to(args, "null", |v| matches!(v, Value::Null))
}

fn cast_object(_: &mut Context, args: &[Value]) -> Out {
    cast_to(args, "object", |v| matches!(v, Value::Object(_)))
}

fn is_type(args: &[Value], name: &str) -> Out {
    ok(Value::Bool(arg(args, 0)?.type_name() == name))
}

fn is_number(_: &mut Context, args: &[Value]) -> Out {
    is_type(args, "number")
}

fn is_string(_: &mut Context, args: &[Value]) -> Out {
    is_type(args, "string")
}

fn is_boolean(_: &mut Context, args: &[Value]) -> Out {
    is_type(args, "boolean")
}

fn is_array(_: &mut Context, args: &[Value]) -> Out {
    is_type(args, "array")
}

fn is_set(_: &mut Context, args: &[Value]) -> Out {
    is_type(args, "set")
}

fn is_object(_: &mut Context, args: &[Value]) -> Out {
    is_type(args, "object")
}

fn is_null(_: &mut Context, args: &[Value]) -> Out {
    is_type(args, "null")
}

fn type_name(_: &mut Context, args: &[Value]) -> Out {
    ok(Value::string(arg(args, 0)?.type_name()))
}

fn compare(args: &[Value], f: fn(std::cmp::Ordering) -> bool) -> Out {
    let (a, b) = (arg(args, 0)?, arg(args, 1)?);
    ok(Value::Bool(f(a.cmp(b))))
}

fn gt(_: &mut Context, args: &[Value]) -> Out {
    compare(args, std::cmp::Ordering::is_gt)
}

fn gte(_: &mut Context, args: &[Value]) -> Out {
    compare(args, std::cmp::Ordering::is_ge)
}

fn lt(_: &mut Context, args: &[Value]) -> Out {
    compare(args, std::cmp::Ordering::is_lt)
}

fn lte(_: &mut Context, args: &[Value]) -> Out {
    compare(args, std::cmp::Ordering::is_le)
}

fn neq(_: &mut Context, args: &[Value]) -> Out {
    compare(args, std::cmp::Ordering::is_ne)
}

fn equal(_: &mut Context, args: &[Value]) -> Out {
    compare(args, std::cmp::Ordering::is_eq)
}
