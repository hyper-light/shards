//! Objects: topdown/object.go and subset.go.

use std::collections::{BTreeMap, BTreeSet};

use super::super::{BuiltinError, Context, arg};
use super::{Out, array_operand, element_err, ok};
use crate::value::Value;

type Map = BTreeMap<Value, Value>;

/// builtins.ObjectOperand.
fn object_operand(v: &Value, pos: usize) -> Result<&Map, BuiltinError> {
    match v {
        Value::Object(o) => Ok(o),
        _ => Err(BuiltinError::operand_type(pos, v, &["object"])),
    }
}

/// mergeWithOverwrite: b's values over a's, objects in both merged.
fn merge(a: &Map, b: &Map) -> Map {
    let mut out = a.clone();
    for (k, vb) in b {
        let merged = match (out.get(k), vb) {
            (Some(Value::Object(x)), Value::Object(y)) => Value::object(merge(x, y)),
            _ => vb.clone(),
        };
        // BTreeMap keeps a's key, as MergeWith does.
        out.insert(k.clone(), merged);
    }
    out
}

pub(super) fn union(_: &mut Context, args: &[Value]) -> Out {
    let a = object_operand(arg(args, 0)?, 1)?;
    let b = object_operand(arg(args, 1)?, 2)?;
    if a.is_empty() {
        return ok(arg(args, 1)?.clone());
    }
    if b.is_empty() || a == b {
        return ok(arg(args, 0)?.clone());
    }
    ok(Value::object(merge(a, b)))
}

/// A value of object.union_n's result as mergewithOverwriteInPlace builds it: its
/// objects open to merging until a later non-object freezes them.
enum Node {
    Leaf(Value),
    Object {
        map: BTreeMap<Value, Node>,
        frozen: bool,
    },
}

impl Node {
    fn from(v: &Value) -> Node {
        match v {
            Value::Object(o) => Node::Object {
                map: o.iter().map(|(k, v)| (k.clone(), Node::from(v))).collect(),
                frozen: false,
            },
            v => Node::Leaf(v.clone()),
        }
    }

    fn value(self) -> Value {
        match self {
            Node::Leaf(v) => v,
            Node::Object { map, .. } => Value::object(map.into_iter().map(|(k, n)| (k, n.value())).collect()),
        }
    }
}

/// mergewithOverwriteInPlace: `other`, an earlier object, under `obj`.
fn merge_in_place(obj: &mut BTreeMap<Value, Node>, other: &Map) {
    for (k, v) in other {
        match obj.get_mut(k) {
            None => {
                obj.insert(k.clone(), Node::from(v));
            }
            Some(Node::Object { map, frozen }) => match v {
                Value::Object(o) => {
                    if !*frozen {
                        merge_in_place(map, o);
                    }
                }
                _ => *frozen = true,
            },
            Some(Node::Leaf(_)) => {}
        }
    }
}

pub(super) fn union_n(_: &mut Context, args: &[Value]) -> Out {
    let a = arg(args, 0)?;
    let items = array_operand(a, 1)?;
    let mut objects = Vec::with_capacity(items.len());
    for x in items {
        match x {
            Value::Object(o) => objects.push(o),
            _ => return Err(element_err(1, a, x, "object")),
        }
    }
    let mut result = BTreeMap::new();
    for o in objects.iter().rev() {
        merge_in_place(&mut result, o);
    }
    ok(Node::Object {
        map: result,
        frozen: false,
    }
    .value())
}

/// getObjectKeysParam.
fn keys_param(v: &Value) -> Result<BTreeSet<Value>, BuiltinError> {
    match v {
        Value::Array(a) => {
            let mut s = BTreeSet::new();
            for x in a.iter() {
                if !s.contains(x) {
                    s.insert(x.clone());
                }
            }
            Ok(s)
        }
        Value::Set(s) => Ok((**s).clone()),
        Value::Object(o) => Ok(o.keys().cloned().collect()),
        _ => Err(BuiltinError::operand_type(2, v, &["object", "set", "array"])),
    }
}

pub(super) fn remove(_: &mut Context, args: &[Value]) -> Out {
    let obj = object_operand(arg(args, 0)?, 1)?;
    let keys = keys_param(arg(args, 1)?)?;
    ok(Value::object(
        obj.iter()
            .filter(|(k, _)| !keys.contains(*k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    ))
}

pub(super) fn filter(_: &mut Context, args: &[Value]) -> Out {
    let obj = object_operand(arg(args, 0)?, 1)?;
    let keys = keys_param(arg(args, 1)?)?;
    // filterObject walks the larger of the two (the object, when equal), keeping
    // its keys.
    let mut out = Map::new();
    if obj.len() < keys.len() {
        for k in &keys {
            if let Some(v) = obj.get(k) {
                out.insert(k.clone(), v.clone());
            }
        }
    } else {
        for (k, v) in obj {
            if keys.contains(k) {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    ok(Value::object(out))
}

/// ast.Value's Find along a path.
fn find<'a>(v: &'a Value, path: &'a [Value]) -> Option<&'a Value> {
    let Some((first, rest)) = path.split_first() else {
        return Some(v);
    };
    match v {
        Value::Object(o) => find(o.get(first)?, rest),
        Value::Array(_) => find(v.get(first)?, rest),
        // A set's Find goes on from the path's term, not the member.
        Value::Set(s) => {
            if s.contains(first) {
                find(first, rest)
            } else {
                None
            }
        }
        _ => None,
    }
}

pub(super) fn get(_: &mut Context, args: &[Value]) -> Out {
    let whole = arg(args, 0)?;
    let obj = object_operand(whole, 1)?;
    let key = arg(args, 1)?;
    let default = arg(args, 2)?;
    let Value::Array(path) = key else {
        return ok(obj.get(key).unwrap_or(default).clone());
    };
    if path.is_empty() {
        return ok(whole.clone());
    }
    ok(find(whole, path).unwrap_or(default).clone())
}

pub(super) fn keys(_: &mut Context, args: &[Value]) -> Out {
    let obj = object_operand(arg(args, 0)?, 1)?;
    ok(Value::set(obj.keys().cloned().collect()))
}

fn object_subset(sup: &Map, sub: &Map) -> bool {
    sub.iter().all(|(k, sub_v)| {
        let Some(sup_v) = sup.get(k) else {
            return false;
        };
        if sub_v == sup_v {
            return true;
        }
        match (sup_v, sub_v) {
            (Value::Object(a), Value::Object(b)) => object_subset(a, b),
            (Value::Set(a), Value::Set(b)) => set_subset(a, b),
            (Value::Array(a), Value::Array(b)) => array_subset(a, b),
            _ => false,
        }
    })
}

fn set_subset(sup: &BTreeSet<Value>, sub: &BTreeSet<Value>) -> bool {
    sub.iter().all(|x| sup.contains(x))
}

/// subset.go's arraySubset: a search that, on a mismatch, moves on without
/// rechecking the element that broke the run, as OPA's does.
fn array_subset(sup: &[Value], sub: &[Value]) -> bool {
    if sub.len() > sup.len() {
        return false;
    }
    if sub == sup {
        return true;
    }
    let (mut sup_at, mut sub_at) = (0usize, 0usize);
    loop {
        if sub_at == sub.len() {
            return true;
        }
        let at = sup_at.saturating_add(sub_at);
        if at == sup.len() {
            return false;
        }
        let (Some(a), Some(b)) = (sup.get(at), sub.get(sub_at)) else {
            return false;
        };
        if a == b {
            sub_at += 1;
        } else {
            sup_at += 1;
            sub_at = 0;
        }
    }
}

/// arraySetSubset: whether the array holds every member, decided as OPA counts them
/// (an empty array never holds even the empty set).
fn array_set_subset(sup: &[Value], sub: &BTreeSet<Value>) -> bool {
    let mut unmatched = sub.len();
    sup.iter().any(|t| {
        if sub.contains(t) {
            unmatched = unmatched.saturating_sub(1);
        }
        unmatched == 0
    })
}

pub(super) fn subset(_: &mut Context, args: &[Value]) -> Out {
    let found = match (arg(args, 0)?, arg(args, 1)?) {
        (Value::Object(a), Value::Object(b)) => object_subset(a, b),
        (Value::Set(a), Value::Set(b)) => set_subset(a, b),
        (Value::Array(a), Value::Array(b)) => array_subset(a, b),
        (Value::Array(a), Value::Set(b)) => array_set_subset(a, b),
        _ => {
            return Err(BuiltinError::Operand(
                "both arguments object.subset must be of the same type or array and set".into(),
            ));
        }
    };
    ok(Value::Bool(found))
}
