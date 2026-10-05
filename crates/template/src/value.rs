//! The values templates run against: Go's kinds as text/template sees them through
//! reflect, and objects, which stand for Go's structs and pointers to them.

use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

/// The Go type of a list's elements, a map's values, or a method's parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `interface {}`: any value, nil included.
    Any,
    Bool,
    /// Go's `int`.
    Int,
    /// Go's `uint`.
    Uint,
    /// Go's `float64`.
    Float,
    String,
}

impl Kind {
    /// The type's name, as Go prints it.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Any => "interface {}",
            Kind::Bool => "bool",
            Kind::Int => "int",
            Kind::Uint => "uint",
            Kind::Float => "float64",
            Kind::String => "string",
        }
    }

    /// The type's zero value.
    pub fn zero(self) -> Value {
        match self {
            Kind::Any => Value::Nil,
            Kind::Bool => Value::Bool(false),
            Kind::Int => Value::Int(0),
            Kind::Uint => Value::Uint(0),
            Kind::Float => Value::Float(0.0),
            Kind::String => Value::String(String::new()),
        }
    }
}

/// A value a template reads: what Go's `reflect.Value` holds.
#[derive(Debug, Clone)]
pub enum Value {
    /// A nil `interface {}`: JSON's null. As the data of a whole template, Go's
    /// `Execute(w, nil)`.
    Nil,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    String(String),
    /// A slice whose elements are of the kind given.
    List(Kind, Vec<Value>),
    /// A map with string keys whose values are of the kind given.
    Map(Kind, BTreeMap<String, Value>),
    /// A struct, or a pointer to one.
    Object(Rc<dyn Object>),
}

impl Value {
    /// A `[]interface {}`.
    pub fn list(items: Vec<Value>) -> Value {
        Value::List(Kind::Any, items)
    }

    /// A `[]string`.
    pub fn strings<S: Into<String>>(items: impl IntoIterator<Item = S>) -> Value {
        Value::List(
            Kind::String,
            items.into_iter().map(|s| Value::String(s.into())).collect(),
        )
    }

    /// A `map[string]interface {}`.
    pub fn map(entries: BTreeMap<String, Value>) -> Value {
        Value::Map(Kind::Any, entries)
    }

    /// A `map[string]string`.
    pub fn string_map<K: Into<String>, V: Into<String>>(entries: impl IntoIterator<Item = (K, V)>) -> Value {
        Value::Map(
            Kind::String,
            entries
                .into_iter()
                .map(|(k, v)| (k.into(), Value::String(v.into())))
                .collect(),
        )
    }

    /// An object.
    pub fn object(o: impl Object + 'static) -> Value {
        Value::Object(Rc::new(o))
    }

    /// The value's Go type, as error messages print it.
    pub fn type_name(&self) -> String {
        match self {
            Value::Nil => "<nil>".into(),
            Value::Bool(_) => "bool".into(),
            Value::Int(_) => "int".into(),
            Value::Uint(_) => "uint".into(),
            Value::Float(_) => "float64".into(),
            Value::String(_) => "string".into(),
            Value::List(k, _) => format!("[]{}", k.name()),
            Value::Map(k, _) => format!("map[string]{}", k.name()),
            Value::Object(o) => o.type_name().into(),
        }
    }

    pub(crate) fn is_nil(&self) -> bool {
        matches!(self, Value::Nil)
    }

    /// Go's encoding/json, as `json` encodes it (HTML characters left as they are).
    pub(crate) fn json(&self, out: &mut String) -> Result<(), String> {
        match self {
            Value::Nil => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Int(i) => out.push_str(&i.to_string()),
            Value::Uint(u) => out.push_str(&u.to_string()),
            Value::Float(f) => json_float(*f, out)?,
            Value::String(s) => json_string(s, out),
            Value::List(_, items) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.json(out)?;
                }
                out.push(']');
            }
            Value::Map(_, entries) => {
                out.push('{');
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    json_string(k, out);
                    out.push(':');
                    v.json(out)?;
                }
                out.push('}');
            }
            Value::Object(o) => o.json(out)?,
        }
        Ok(())
    }
}

/// A Go struct, or a pointer to one, as templates see it: fields, methods, and how fmt
/// and encoding/json print it.
pub trait Object: fmt::Debug {
    /// Go's name for the type (`formatter.ContainerContext`, `*formatter.ContainerContext`).
    fn type_name(&self) -> &str;

    /// The exported field of that name.
    fn field(&self, name: &str) -> Option<Value> {
        let _ = name;
        None
    }

    /// The parameters of the exported method of that name. Methods take precedence over
    /// fields, as reflect's MethodByName does in exec.go's evalField.
    fn method(&self, name: &str) -> Option<&'static [Kind]> {
        let _ = name;
        None
    }

    /// Calls a method `method` named, with arguments of the kinds it gave. An error is
    /// the error a Go method returns beside its value.
    fn call(&self, name: &str, args: &[Value]) -> Result<Value, String> {
        let _ = args;
        Err(format!("no method {name}"))
    }

    /// The struct as fmt's `%v` prints it (`{a b}`). For a pointer (a type name starting
    /// with `*`), fmt writes the `&` before it where Go would.
    fn format(&self, out: &mut String);

    /// The value as encoding/json encodes it. An error is encoding/json's.
    fn json(&self, out: &mut String) -> Result<(), String>;
}

/// encode.go's floatEncoder: as ES6 prints numbers.
fn json_float(f: f64, out: &mut String) -> Result<(), String> {
    if f.is_nan() || f.is_infinite() {
        return Err(format!(
            "json: unsupported value: {}",
            crate::strconv::format_float(f, b'g', -1)
        ));
    }
    let a = f.abs();
    let fmt = if a != 0.0 && !(1e-6..1e21).contains(&a) {
        b'e'
    } else {
        b'f'
    };
    let mut s = crate::strconv::format_float(f, fmt, -1);
    if fmt == b'e' {
        // e-09 to e-9
        let b = s.as_bytes();
        let n = b.len();
        if n >= 4 && b.get(n - 4) == Some(&b'e') && b.get(n - 3) == Some(&b'-') && b.get(n - 2) == Some(&b'0')
        {
            s.remove(n - 2);
        }
    }
    out.push_str(&s);
    Ok(())
}

/// encode.go's appendString, with escapeHTML false.
pub(crate) fn json_string(s: &str, out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let hex = |n: u32| char::from(HEX.get((n & 0xf) as usize).copied().unwrap_or(b'0'));
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' | '\u{2029}' => {
                out.push_str("\\u202");
                out.push(hex(u32::from(c)));
            }
            _ if c < ' ' => {
                out.push_str("\\u00");
                out.push(hex(u32::from(c) >> 4));
                out.push(hex(u32::from(c)));
            }
            _ => out.push(c),
        }
    }
    out.push('"');
}
