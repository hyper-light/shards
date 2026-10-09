//! OPA's types (types/types.go), as builtins declare them and the type checker infers
//! them. Read from the JSON OPA marshals (`types.Unmarshal`).

/// A static object property's key: OPA keeps it as a JSON value.
pub type Key = serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Null,
    Boolean,
    Number,
    String,
    /// A union; empty for "any type at all".
    Any(Vec<Type>),
    Array {
        fixed: Vec<Type>,
        dynamic: Option<Box<Type>>,
    },
    Object {
        fixed: Vec<(Key, Type)>,
        dynamic: Option<(Box<Type>, Box<Type>)>,
    },
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

impl Type {
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
            "any" => Type::Any(list("of")?),
            "array" => Type::Array {
                fixed: list("static")?,
                dynamic: one("dynamic")?,
            },
            "set" => Type::Set(one("of")?),
            "object" => {
                let mut fixed = Vec::new();
                if let Some(s) = o.get("static") {
                    for p in s.as_array()? {
                        let key = p.get("key")?.clone();
                        fixed.push((key, Type::from_json(p.get("value")?)?));
                    }
                }
                let dynamic = match o.get("dynamic") {
                    None => None,
                    Some(d) => Some((
                        Box::new(Type::from_json(d.get("key")?)?),
                        Box::new(Type::from_json(d.get("value")?)?),
                    )),
                };
                Type::Object { fixed, dynamic }
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
