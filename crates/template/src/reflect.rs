//! What exec.go holds in a `reflect.Value`: nothing at all (an invalid value), a value of
//! a concrete type, or a value of interface type, which Go's executor tells apart in its
//! errors and nil handling.

use std::borrow::Cow;
use std::fmt;

use crate::value::{Kind, Value};

#[derive(Debug, Clone)]
pub(crate) enum R<'d> {
    /// The zero `reflect.Value`.
    Invalid,
    /// A value of the type it holds.
    Plain(Cow<'d, Value>),
    /// A value of type `interface {}`: an element of a `[]interface {}` or
    /// `map[string]interface {}`. `Value::Nil` here is a nil interface.
    Iface(Cow<'d, Value>),
}

impl<'d> R<'d> {
    /// A value a function or method made.
    pub(crate) fn owned(v: Value) -> R<'d> {
        if v.is_nil() {
            R::Iface(Cow::Owned(v))
        } else {
            R::Plain(Cow::Owned(v))
        }
    }

    /// An element of a list or map whose elements are of the kind given.
    pub(crate) fn elem(kind: Kind, v: Cow<'d, Value>) -> R<'d> {
        if kind == Kind::Any || v.is_nil() {
            R::Iface(v)
        } else {
            R::Plain(v)
        }
    }

    /// `v.Type().String()`; the invalid value has no type.
    pub(crate) fn type_name(&self) -> String {
        match self {
            R::Invalid => "<nil>".into(),
            R::Plain(v) => v.type_name(),
            R::Iface(_) => "interface {}".into(),
        }
    }

    /// The value held, if any: what a Go function taking `any` receives (None is nil).
    pub(crate) fn value(&self) -> Option<&Value> {
        match self {
            R::Invalid => None,
            R::Plain(v) | R::Iface(v) => (!v.is_nil()).then_some(v.as_ref()),
        }
    }

    /// As a function taking `any` receives it.
    pub(crate) fn to_any(&self) -> Value {
        self.value().cloned().unwrap_or(Value::Nil)
    }
}

/// exec.go's indirectInterface.
pub(crate) fn indirect_interface(r: R<'_>) -> R<'_> {
    match r {
        R::Iface(v) if v.is_nil() => R::Invalid,
        R::Iface(v) => R::Plain(v),
        r => r,
    }
}

/// exec.go's indirect: the value behind an interface, and whether that is nil.
pub(crate) fn indirect(r: R<'_>) -> (R<'_>, bool) {
    match r {
        R::Iface(v) if v.is_nil() => (R::Iface(v), true),
        R::Iface(v) => (R::Plain(v), false),
        r => (r, false),
    }
}

/// exec.go's isTrue; every kind here has a truth value.
pub(crate) fn is_true(r: &R<'_>) -> bool {
    match r {
        R::Invalid => false,
        R::Iface(v) => !v.is_nil(),
        R::Plain(v) => match v.as_ref() {
            Value::Nil => false,
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            Value::Uint(u) => *u != 0,
            Value::Float(f) => *f != 0.0,
            Value::String(s) => !s.is_empty(),
            Value::List(_, l) => !l.is_empty(),
            Value::Map(_, m) => !m.is_empty(),
            Value::Object(_) => true,
        },
    }
}

/// Where a list's element or a map's value is.
pub(crate) enum Key<'k> {
    Index(usize),
    Name(&'k str),
}

fn get<'x>(v: &'x Value, key: &Key<'_>) -> Option<&'x Value> {
    match (v, key) {
        (Value::List(_, items), Key::Index(i)) => items.get(*i),
        (Value::Map(_, m), Key::Name(k)) => m.get(*k),
        _ => None,
    }
}

/// A list's element or a map's value, borrowed when the container is.
pub(crate) fn child<'d>(v: &Cow<'d, Value>, key: Key<'_>) -> Option<Cow<'d, Value>> {
    match v {
        Cow::Borrowed(b) => get(b, &key).map(Cow::Borrowed),
        Cow::Owned(o) => get(o, &key).cloned().map(Cow::Owned),
    }
}

/// The type of a function's or method's parameter, as exec.go's evalArg and
/// validateType read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Param {
    /// `reflect.Value`, which the builtins take: anything, interfaces kept.
    Value,
    /// `interface {}`.
    Any,
    Bool,
    Int,
    Uint,
    Float,
    String,
}

impl Param {
    pub(crate) fn of(k: Kind) -> Param {
        match k {
            Kind::Any => Param::Any,
            Kind::Bool => Param::Bool,
            Kind::Int => Param::Int,
            Kind::Uint => Param::Uint,
            Kind::Float => Param::Float,
            Kind::String => Param::String,
        }
    }

    /// Whether a value of a concrete type is assignable to it.
    pub(crate) fn accepts(self, v: &Value) -> bool {
        matches!(
            (self, v),
            (Param::Value | Param::Any, _)
                | (Param::Bool, Value::Bool(_))
                | (Param::Int, Value::Int(_))
                | (Param::Uint, Value::Uint(_))
                | (Param::Float, Value::Float(_))
                | (Param::String, Value::String(_))
        )
    }
}

impl fmt::Display for Param {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Param::Value => "reflect.Value",
            Param::Any => "interface {}",
            Param::Bool => "bool",
            Param::Int => "int",
            Param::Uint => "uint",
            Param::Float => "float64",
            Param::String => "string",
        })
    }
}
