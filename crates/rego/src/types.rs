//! OPA's types (types/types.go), as builtins declare them and the type checker infers
//! them: read from the JSON OPA marshals (`types.Unmarshal`), compared, joined and
//! written as OPA does.

use std::cmp::Ordering;
use std::fmt;

use crate::value::number_compare;

/// A static object property's key: the Go value OPA keeps (`ast.JSON` of a term).
#[derive(Debug, Clone, PartialEq)]
pub enum Key {
    Null,
    Bool(bool),
    /// A json.Number, as written.
    Number(String),
    String(String),
    Array(Vec<Key>),
    /// A map[string]any, keys sorted.
    Object(Vec<(String, Key)>),
}

impl Key {
    pub fn from_json(v: &serde_json::Value) -> Key {
        match v {
            serde_json::Value::Null => Key::Null,
            serde_json::Value::Bool(b) => Key::Bool(*b),
            serde_json::Value::Number(n) => Key::Number(n.to_string()),
            serde_json::Value::String(s) => Key::String(s.clone()),
            serde_json::Value::Array(a) => Key::Array(a.iter().map(Key::from_json).collect()),
            serde_json::Value::Object(o) => {
                let mut m: Vec<(String, Key)> =
                    o.iter().map(|(k, v)| (k.clone(), Key::from_json(v))).collect();
                m.sort_by(|a, b| a.0.cmp(&b.0));
                Key::Object(m)
            }
        }
    }

    fn sort_order(&self) -> u8 {
        match self {
            Key::Null => 0,
            Key::Bool(_) => 1,
            Key::Number(_) => 2,
            Key::String(_) => 3,
            Key::Array(_) => 4,
            Key::Object(_) => 5,
        }
    }

    /// util.Compare.
    pub fn compare(&self, other: &Key) -> Ordering {
        let (a, b) = (self.sort_order(), other.sort_order());
        if a != b {
            return a.cmp(&b);
        }
        match (self, other) {
            (Key::Bool(x), Key::Bool(y)) => x.cmp(y),
            (Key::Number(x), Key::Number(y)) => number_compare(x, y),
            (Key::String(x), Key::String(y)) => x.as_bytes().cmp(y.as_bytes()),
            (Key::Array(x), Key::Array(y)) => {
                for (p, q) in x.iter().zip(y) {
                    let c = p.compare(q);
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                x.len().cmp(&y.len())
            }
            (Key::Object(x), Key::Object(y)) => {
                for ((ka, va), (kb, vb)) in x.iter().zip(y) {
                    let c = ka.as_bytes().cmp(kb.as_bytes()).then_with(|| va.compare(vb));
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                x.len().cmp(&y.len())
            }
            _ => Ordering::Equal,
        }
    }
}

/// The key as Go's `%v` writes it.
impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Key::Null => f.write_str("<nil>"),
            Key::Bool(b) => write!(f, "{b}"),
            Key::Number(n) => f.write_str(n),
            Key::String(s) => f.write_str(s),
            Key::Array(a) => {
                f.write_str("[")?;
                for (i, k) in a.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" ")?;
                    }
                    write!(f, "{k}")?;
                }
                f.write_str("]")
            }
            Key::Object(o) => {
                f.write_str("map[")?;
                for (i, (k, v)) in o.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" ")?;
                    }
                    write!(f, "{k}:{v}")?;
                }
                f.write_str("]")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    /// Go's nil: a type not known.
    Nil,
    Null,
    Boolean,
    Number,
    String,
    /// A union, sorted; empty for "any type at all".
    Any(Vec<Type>),
    Array {
        fixed: Vec<Type>,
        dynamic: Option<Box<Type>>,
    },
    /// Static properties sorted by key; a dynamic property's key or value may be Nil.
    Object {
        fixed: Vec<(Key, Type)>,
        dynamic: Option<(Box<Type>, Box<Type>)>,
    },
    /// The element type; None for nil.
    Set(Option<Box<Type>>),
    Function {
        args: Vec<Type>,
        result: Option<Box<Type>>,
        variadic: Option<Box<Type>>,
    },
    Named {
        name: String,
        ty: Box<Type>,
    },
}

/// types.A.
pub const A: Type = Type::Any(Vec::new());

fn boxed(t: Type) -> Option<Box<Type>> {
    match t {
        Type::Nil => None,
        t => Some(Box::new(t)),
    }
}

fn opt(t: &Option<Box<Type>>) -> &Type {
    t.as_deref().unwrap_or(&Type::Nil)
}

/// The arguments a function takes (types.FuncArgs).
#[derive(Debug, Clone, PartialEq)]
pub struct FuncArgs {
    pub args: Vec<Type>,
    pub variadic: Type,
}

impl FuncArgs {
    /// FuncArgs.Arg.
    pub fn arg(&self, i: usize) -> &Type {
        self.args.get(i).unwrap_or(&self.variadic)
    }
}

impl fmt::Display for FuncArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf: Vec<String> = self.args.iter().map(ToString::to_string).collect();
        if !self.variadic.is_nil() {
            buf.push(format!("{}...", self.variadic));
        }
        write!(f, "({})", buf.join(", "))
    }
}

impl Type {
    /// types.NewArray.
    pub fn array(fixed: Vec<Type>, dynamic: Type) -> Type {
        Type::Array {
            fixed,
            dynamic: boxed(dynamic),
        }
    }

    /// types.NewObject: the static properties sorted by key.
    pub fn object(mut fixed: Vec<(Key, Type)>, dynamic: Option<(Type, Type)>) -> Type {
        fixed.sort_by(|a, b| a.0.compare(&b.0));
        Type::Object {
            fixed,
            dynamic: dynamic.map(|(k, v)| (Box::new(k), Box::new(v))),
        }
    }

    /// types.NewSet.
    pub fn set(of: Type) -> Type {
        Type::Set(boxed(of))
    }

    /// types.NewFunction.
    pub fn function(args: Vec<Type>, result: Type) -> Type {
        Type::Function {
            args,
            result: boxed(result),
            variadic: None,
        }
    }

    /// types.NewAny: the members sorted.
    pub fn any(mut of: Vec<Type>) -> Type {
        of.sort_by(compare);
        Type::Any(of)
    }

    /// unwrap: a named type's type.
    pub fn unwrap(&self) -> &Type {
        match self {
            Type::Named { ty, .. } => ty.unwrap(),
            t => t,
        }
    }

    pub fn is_nil(&self) -> bool {
        matches!(self.unwrap(), Type::Nil)
    }

    /// Function.FuncArgs: the arguments, names dropped.
    pub fn func_args(&self) -> FuncArgs {
        match self.unwrap() {
            Type::Function { args, variadic, .. } => FuncArgs {
                args: args.iter().map(|a| a.unwrap().clone()).collect(),
                variadic: opt(variadic).unwrap().clone(),
            },
            _ => FuncArgs {
                args: Vec::new(),
                variadic: Type::Nil,
            },
        }
    }

    /// Function.NamedFuncArgs.
    pub fn named_func_args(&self) -> FuncArgs {
        match self.unwrap() {
            Type::Function { args, variadic, .. } => FuncArgs {
                args: args.clone(),
                variadic: opt(variadic).clone(),
            },
            _ => FuncArgs {
                args: Vec::new(),
                variadic: Type::Nil,
            },
        }
    }

    /// Function.Result, its name dropped.
    pub fn result(&self) -> Type {
        self.named_result().unwrap().clone()
    }

    /// Function.NamedResult.
    pub fn named_result(&self) -> Type {
        match self.unwrap() {
            Type::Function { result, .. } => opt(result).clone(),
            _ => Type::Nil,
        }
    }

    /// Function.Arity: the declared arguments.
    pub fn arity(&self) -> usize {
        match self.unwrap() {
            Type::Function { args, .. } => args.len(),
            _ => 0,
        }
    }

    /// types.Unmarshal.
    pub fn from_json(v: &serde_json::Value) -> Option<Type> {
        let o = v.as_object()?;
        if let Some(name) = o.get("name").and_then(|n| n.as_str()) {
            let mut inner = o.clone();
            inner.remove("name");
            inner.remove("description");
            let ty = Type::from_json(&serde_json::Value::Object(inner))?;
            return Some(Type::Named {
                name: name.to_string(),
                ty: Box::new(ty),
            });
        }
        let list = |k: &str| -> Option<Vec<Type>> {
            match o.get(k) {
                None => Some(Vec::new()),
                Some(a) => a.as_array()?.iter().map(Type::from_json).collect(),
            }
        };
        let one = |k: &str| -> Option<Option<Box<Type>>> {
            match o.get(k) {
                None => Some(None),
                Some(t) => Some(Some(Box::new(Type::from_json(t)?))),
            }
        };
        Some(match o.get("type")?.as_str()? {
            "null" => Type::Null,
            "boolean" => Type::Boolean,
            "number" => Type::Number,
            "string" => Type::String,
            "any" => Type::any(list("of")?),
            "array" => Type::Array {
                fixed: list("static")?,
                dynamic: one("dynamic")?,
            },
            "set" => Type::Set(one("of")?),
            "object" => {
                let mut fixed = Vec::new();
                if let Some(s) = o.get("static") {
                    for p in s.as_array()? {
                        fixed.push((Key::from_json(p.get("key")?), Type::from_json(p.get("value")?)?));
                    }
                }
                let dynamic = match o.get("dynamic") {
                    None => None,
                    Some(d) => Some((Type::from_json(d.get("key")?)?, Type::from_json(d.get("value")?)?)),
                };
                Type::object(fixed, dynamic)
            }
            "function" => Type::Function {
                args: list("args")?,
                result: one("result")?,
                variadic: one("variadic")?,
            },
            _ => return None,
        })
    }
}

/// Sprint: a type's text, "???" for nil.
impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Nil => f.write_str("???"),
            Type::Null => f.write_str("null"),
            Type::Boolean => f.write_str("boolean"),
            Type::Number => f.write_str("number"),
            Type::String => f.write_str("string"),
            Type::Any(of) => {
                f.write_str("any")?;
                if !of.is_empty() {
                    let buf: Vec<String> = of.iter().map(ToString::to_string).collect();
                    write!(f, "<{}>", buf.join(", "))?;
                }
                Ok(())
            }
            Type::Array { fixed, dynamic } => {
                f.write_str("array")?;
                if !fixed.is_empty() {
                    let buf: Vec<String> = fixed.iter().map(ToString::to_string).collect();
                    write!(f, "<{}>", buf.join(", "))?;
                }
                if let Some(d) = dynamic {
                    write!(f, "[{d}]")?;
                }
                Ok(())
            }
            Type::Object { fixed, dynamic } => {
                f.write_str("object")?;
                if !fixed.is_empty() {
                    let buf: Vec<String> = fixed.iter().map(|(k, v)| format!("{k}: {v}")).collect();
                    write!(f, "<{}>", buf.join(", "))?;
                }
                if let Some((k, v)) = dynamic {
                    write!(f, "[{k}: {v}]")?;
                }
                Ok(())
            }
            Type::Set(of) => write!(f, "set[{}]", opt(of)),
            Type::Function { .. } => write!(f, "{} => {}", self.func_args(), self.result()),
            Type::Named { name, ty } => write!(f, "{name}: {ty}"),
        }
    }
}

/// Go's `%v` of a type: "<nil>" for nil, else its text.
pub fn go_value(t: &Type) -> String {
    match t {
        Type::Nil => "<nil>".to_string(),
        t => t.to_string(),
    }
}

fn type_order(t: &Type) -> i8 {
    match t.unwrap() {
        Type::Nil => -1,
        Type::Null => 0,
        Type::Boolean => 1,
        Type::Number => 2,
        Type::String => 3,
        Type::Array { .. } => 4,
        Type::Object { .. } => 5,
        Type::Set(_) => 6,
        Type::Any(_) => 7,
        Type::Function { .. } => 8,
        // unwrap never returns a named type.
        Type::Named { .. } => 9,
    }
}

fn slice_compare(a: &[Type], b: &[Type]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let c = compare(x, y);
        if c != Ordering::Equal {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

/// types.Compare.
pub fn compare(a: &Type, b: &Type) -> Ordering {
    let (a, b) = (a.unwrap(), b.unwrap());
    let (x, y) = (type_order(a), type_order(b));
    if x != y {
        return x.cmp(&y);
    }
    match (a, b) {
        (
            Type::Array {
                fixed: sa,
                dynamic: da,
            },
            Type::Array {
                fixed: sb,
                dynamic: db,
            },
        ) => {
            match (da, db) {
                (Some(_), None) => return Ordering::Greater,
                (None, Some(_)) => return Ordering::Less,
                (Some(x), Some(y)) => {
                    let c = compare(x, y);
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                (None, None) => {}
            }
            slice_compare(sa, sb)
        }
        (
            Type::Object {
                fixed: sa,
                dynamic: da,
            },
            Type::Object {
                fixed: sb,
                dynamic: db,
            },
        ) => {
            match (da, db) {
                (Some(_), None) => return Ordering::Greater,
                (None, Some(_)) => return Ordering::Less,
                (Some((ka, va)), Some((kb, vb))) => {
                    let c = compare(ka, kb).then_with(|| compare(va, vb));
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                (None, None) => {}
            }
            for ((ka, va), (kb, vb)) in sa.iter().zip(sb) {
                let c = ka.compare(kb).then_with(|| compare(va, vb));
                if c != Ordering::Equal {
                    return c;
                }
            }
            sa.len().cmp(&sb.len())
        }
        (Type::Set(x), Type::Set(y)) => match (x, y) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(x), Some(y)) => compare(x, y),
        },
        (Type::Any(x), Type::Any(y)) => slice_compare(x, y),
        (
            Type::Function {
                args: aa,
                result: ra,
                variadic: va,
            },
            Type::Function {
                args: ab,
                result: rb,
                variadic: vb,
            },
        ) => {
            let c = aa.len().cmp(&ab.len());
            if c != Ordering::Equal {
                return c;
            }
            for (x, y) in aa.iter().zip(ab) {
                let c = compare(x, y);
                if c != Ordering::Equal {
                    return c;
                }
            }
            compare(opt(ra), opt(rb)).then_with(|| compare(opt(va), opt(vb)))
        }
        _ => Ordering::Equal,
    }
}

/// Any.Contains.
fn any_contains(t: &[Type], other: &Type) -> bool {
    if matches!(other.unwrap(), Type::Function { .. }) {
        return false;
    }
    let i = t.partition_point(|x| compare(x, other) == Ordering::Less);
    if t.get(i).is_some_and(|x| compare(x, other) == Ordering::Equal) {
        return true;
    }
    t.is_empty()
}

/// types.Contains: whether a is a superset of b, or equal to it.
pub fn contains(a: &Type, b: &Type) -> bool {
    if let Type::Any(x) = a.unwrap() {
        return any_contains(x, b);
    }
    compare(a, b) == Ordering::Equal
}

/// Any.Union: two sorted unions merged.
fn any_union(t: &[Type], other: &[Type]) -> Vec<Type> {
    if t.is_empty() || other.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(t.len().max(other.len()));
    let (mut a, mut b) = (t.iter().peekable(), other.iter().peekable());
    loop {
        match (a.peek(), b.peek()) {
            (None, None) => break,
            (Some(_), None) => out.extend(a.by_ref().cloned()),
            (None, Some(_)) => out.extend(b.by_ref().cloned()),
            (Some(x), Some(y)) => match compare(x, y) {
                Ordering::Less => out.extend(a.next().cloned()),
                Ordering::Equal => {
                    out.extend(a.next().cloned());
                    b.next();
                }
                Ordering::Greater => out.extend(b.next().cloned()),
            },
        }
    }
    out
}

/// Any.Merge.
fn any_merge(t: &[Type], other: &Type) -> Type {
    if let Type::Any(o) = other {
        return Type::Any(any_union(t, o));
    }
    if any_contains(t, other) {
        return Type::Any(t.to_vec());
    }
    let i = t.partition_point(|x| compare(x, other) == Ordering::Less);
    let mut out = t.to_vec();
    out.insert(i.min(out.len()), other.clone());
    Type::Any(out)
}

/// Function.Union: functions of one arity joined argument by argument.
fn func_union(a: &Type, b: &Type) -> Type {
    if a.arity() != b.arity() {
        return Type::Nil;
    }
    let (fa, fb) = (a.func_args(), b.func_args());
    if fa.variadic.is_nil() != fb.variadic.is_nil() {
        return Type::Nil;
    }
    let args = fa.args.iter().zip(&fb.args).map(|(x, y)| or(x, y)).collect();
    Type::Function {
        args,
        result: boxed(or(&a.result(), &b.result())),
        variadic: boxed(or(&fa.variadic, &fb.variadic)),
    }
}

/// types.Or: the union of a and b; the superset when one contains the other.
pub fn or(a: &Type, b: &Type) -> Type {
    let (a, b) = (a.unwrap(), b.unwrap());
    if a.is_nil() {
        return b.clone();
    }
    if b.is_nil() {
        return a.clone();
    }
    let (fa, fb) = (
        matches!(a, Type::Function { .. }),
        matches!(b, Type::Function { .. }),
    );
    if fa && fb {
        return func_union(a, b);
    }
    if fa || fb {
        return Type::Nil;
    }
    if let Type::Any(x) = a {
        return any_merge(x, b);
    }
    if let Type::Any(y) = b {
        return any_merge(y, a);
    }
    if compare(a, b) == Ordering::Equal {
        return a.clone();
    }
    Type::any(vec![a.clone(), b.clone()])
}

/// Array.Select of a static position.
pub fn array_elem(t: &Type, pos: usize) -> Type {
    match t.unwrap() {
        Type::Array { fixed, dynamic } => match fixed.get(pos) {
            Some(x) => x.clone(),
            None => opt(dynamic).clone(),
        },
        _ => Type::Nil,
    }
}

/// Object.Select.
pub fn object_select(t: &Type, name: &Key) -> Type {
    let Type::Object { fixed, dynamic } = t.unwrap() else {
        return Type::Nil;
    };
    let pos = fixed.partition_point(|(k, _)| k.compare(name) == Ordering::Less);
    if let Some((k, v)) = fixed.get(pos)
        && k.compare(name) == Ordering::Equal
    {
        return v.clone();
    }
    if let Some((k, v)) = dynamic
        && contains(k, &type_of(name))
    {
        return (**v).clone();
    }
    Type::Nil
}

/// types.Select: a property or item of a.
pub fn select(a: &Type, x: &Key) -> Type {
    match a.unwrap() {
        t @ Type::Array { .. } => {
            // json.Number.Int64: a base-10 integer.
            let Key::Number(n) = x else { return Type::Nil };
            match n.parse::<i64>().ok().and_then(|p| usize::try_from(p).ok()) {
                Some(pos) => array_elem(t, pos),
                None => Type::Nil,
            }
        }
        t @ Type::Object { .. } => object_select(t, x),
        Type::Set(of) => {
            let tpe = type_of(x);
            let of = opt(of);
            if compare(of, &tpe) == Ordering::Equal {
                return of.clone();
            }
            if let Type::Any(o) = of
                && any_contains(o, &tpe)
            {
                return tpe;
            }
            Type::Nil
        }
        Type::Any(of) => {
            if of.is_empty() {
                return A;
            }
            let mut tpe = Type::Nil;
            for t in of {
                tpe = or(&select(t, x), &tpe);
            }
            tpe
        }
        _ => Type::Nil,
    }
}

/// types.Keys: the type of the keys that enumerate a.
pub fn keys(a: &Type) -> Type {
    match a.unwrap() {
        Type::Array { .. } => Type::Number,
        Type::Object { fixed, dynamic } => {
            let mut tpe = Type::Nil;
            for (k, _) in fixed {
                tpe = or(&tpe, &type_of(k));
            }
            if let Some((k, _)) = dynamic {
                tpe = or(&tpe, k);
            }
            tpe
        }
        Type::Set(of) => opt(of).clone(),
        Type::Any(of) => {
            if of.is_empty() {
                return A;
            }
            let mut tpe = Type::Nil;
            for t in of {
                tpe = or(&keys(t), &tpe);
            }
            tpe
        }
        _ => Type::Nil,
    }
}

/// types.Values: the type of the values that enumerate a.
pub fn values(a: &Type) -> Type {
    match a.unwrap() {
        Type::Array { fixed, dynamic } => {
            let mut tpe = Type::Nil;
            for t in fixed {
                tpe = or(&tpe, t);
            }
            or(&tpe, opt(dynamic))
        }
        Type::Object { fixed, dynamic } => {
            let mut tpe = Type::Nil;
            for (_, v) in fixed {
                tpe = or(&tpe, v);
            }
            if let Some((_, v)) = dynamic {
                tpe = or(&tpe, v);
            }
            tpe
        }
        Type::Set(of) => opt(of).clone(),
        Type::Any(of) => {
            if of.is_empty() {
                return A;
            }
            let mut tpe = Type::Nil;
            for t in of {
                tpe = or(&values(t), &tpe);
            }
            tpe
        }
        _ => Type::Nil,
    }
}

/// types.Nil: whether any part of a is not known.
pub fn has_nil(a: &Type) -> bool {
    match a.unwrap() {
        Type::Nil => true,
        Type::Function { args, result, .. } => args.iter().any(has_nil) || has_nil(opt(result)),
        Type::Array { fixed, dynamic } => {
            fixed.iter().any(has_nil) || dynamic.as_deref().is_some_and(has_nil)
        }
        Type::Object { fixed, dynamic } => {
            fixed.iter().any(|(_, v)| has_nil(v))
                || dynamic.as_ref().is_some_and(|(k, v)| has_nil(k) || has_nil(v))
        }
        Type::Set(of) => has_nil(opt(of)),
        _ => false,
    }
}

/// types.TypeOf: the type of a Go value.
pub fn type_of(x: &Key) -> Type {
    match x {
        Key::Null => Type::Null,
        Key::Bool(_) => Type::Boolean,
        Key::String(_) => Type::String,
        Key::Number(_) => Type::Number,
        Key::Object(o) => Type::object(
            o.iter()
                .map(|(k, v)| (Key::String(k.clone()), type_of(v)))
                .collect(),
            None,
        ),
        Key::Array(a) => Type::array(a.iter().map(type_of).collect(), Type::Nil),
    }
}
