//! Go structs made of data: a type, its fields in their declared order, each with its Go
//! name, its JSON name and options, and its value. Templates read them by field as
//! reflect does, fmt prints them as `{a b}` (`&{a b}` for a pointer), and encoding/json
//! encodes them in field order, `omitempty` and `-` honoured: what the Docker API's types
//! are to docker/cli's `--format` (container.InspectResponse and the like).

use crate::value::{Object, Value, json_string};

/// One field of a [`Struct`].
#[derive(Debug, Clone)]
struct Field {
    name: String,
    /// The JSON name, or none for `json:"-"`.
    json: Option<String>,
    omitempty: bool,
    value: Value,
}

/// A Go struct value, or a pointer to one, or a nil pointer of its type.
#[derive(Debug, Clone)]
pub struct Struct {
    type_name: String,
    nil: bool,
    fields: Vec<Field>,
}

impl Struct {
    /// A struct of type `type_name` (`container.State`), with no fields yet.
    pub fn new(type_name: &str) -> Struct {
        Struct {
            type_name: type_name.to_owned(),
            nil: false,
            fields: Vec::new(),
        }
    }

    /// A pointer to a struct of type `type_name`: `*container.State`.
    pub fn pointer(type_name: &str) -> Struct {
        Struct {
            type_name: format!("*{type_name}"),
            nil: false,
            fields: Vec::new(),
        }
    }

    /// A nil pointer to a struct of type `type_name`, as a value.
    pub fn nil(type_name: &str) -> Value {
        Value::object(Struct {
            type_name: format!("*{type_name}"),
            nil: true,
            fields: Vec::new(),
        })
    }

    /// Field `name`, its JSON name the same.
    #[must_use]
    pub fn field(self, name: &str, value: Value) -> Struct {
        self.tagged(name, Some(name), false, value)
    }

    /// Field `name`, its JSON name `json` (none for `json:"-"`), `omitempty` or not.
    #[must_use]
    pub fn tagged(mut self, name: &str, json: Option<&str>, omitempty: bool, value: Value) -> Struct {
        self.fields.push(Field {
            name: name.to_owned(),
            json: json.map(str::to_owned),
            omitempty,
            value,
        });
        self
    }

    /// The struct as a template value.
    pub fn value(self) -> Value {
        Value::object(self)
    }
}

/// encoding/json's isEmptyValue: what `omitempty` leaves out.
fn is_empty(v: &Value) -> bool {
    match v {
        Value::Nil | Value::NilList(_) | Value::NilMap(_) => true,
        Value::Bool(b) => !b,
        Value::Int(i) => *i == 0,
        Value::Uint(u) => *u == 0,
        Value::Float(f) => *f == 0.0,
        Value::String(s) => s.is_empty(),
        Value::List(_, l) => l.is_empty(),
        Value::Map(_, m) => m.is_empty(),
        Value::Object(o) => o.is_nil(),
    }
}

impl Object for Struct {
    fn type_name(&self) -> &str {
        &self.type_name
    }

    fn is_nil(&self) -> bool {
        self.nil
    }

    fn field(&self, name: &str) -> Option<Value> {
        if self.nil {
            return None;
        }
        self.fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.value.clone())
    }

    fn format(&self, out: &mut String) {
        if self.nil {
            out.push_str("<nil>");
            return;
        }
        out.push('{');
        for (i, f) in self.fields.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            match &f.value {
                // fmt prints a nested pointer's address; this prints what it points to,
                // which no address could be relied on to say.
                Value::Object(o) if o.type_name().starts_with('*') && !o.is_nil() => {
                    out.push('&');
                    o.format(out);
                }
                v => out.push_str(&crate::fmt::sprint(std::slice::from_ref(v))),
            }
        }
        out.push('}');
    }

    fn json(&self, out: &mut String) -> Result<(), String> {
        if self.nil {
            out.push_str("null");
            return Ok(());
        }
        out.push('{');
        let mut first = true;
        for f in &self.fields {
            let Some(name) = &f.json else {
                continue;
            };
            if f.omitempty && is_empty(&f.value) {
                continue;
            }
            if !first {
                out.push(',');
            }
            first = false;
            json_string(name, out);
            out.push(':');
            f.value.json(out)?;
        }
        out.push('}');
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Kind, Template};

    fn state(health: Value) -> Value {
        Struct::pointer("container.State")
            .field("Status", Value::String("running".into()))
            .field("Pid", Value::Int(7))
            .tagged("Health", Some("Health"), true, health)
            .value()
    }

    fn run(text: &str, data: &Value) -> Result<String, String> {
        Template::parse("", text)?.execute(data)
    }

    /// As Go reads, prints and encodes container.State and its Health.
    #[test]
    fn structs_are_read_printed_and_encoded_as_go_does() {
        let doc = Struct::new("container.InspectResponse")
            .tagged("ID", Some("Id"), false, Value::String("abc".into()))
            .field("State", state(Struct::nil("container.Health")))
            .field("ExecIDs", Value::NilList(Kind::String))
            .tagged("Hidden", None, false, Value::Int(1))
            .value();
        assert_eq!(run("{{.ID}}", &doc), Ok("abc".into()));
        assert_eq!(
            run("{{.Id}}", &doc),
            Err(r#"template: :1:2: executing "" at <.Id>: can't evaluate field Id in type container.InspectResponse"#.into())
        );
        assert_eq!(
            run("{{.State.Status}} {{.State.Pid}}", &doc),
            Ok("running 7".into())
        );
        assert_eq!(run("{{.State.Health}}", &doc), Ok("<nil>".into()));
        assert_eq!(
            run("{{.State.Health.Status}}", &doc),
            Err(r#"template: :1:8: executing "" at <.State.Health.Status>: nil pointer evaluating *container.Health.Status"#.into())
        );
        // exec.go's printableValue follows the pointer first.
        assert_eq!(run("{{.State}}", &doc), Ok("{running 7 <nil>}".into()));
        assert_eq!(run("{{.ExecIDs}} {{json .ExecIDs}}", &doc), Ok("[] null".into()));
        assert_eq!(
            run("{{json .}}", &doc),
            Ok(r#"{"Id":"abc","State":{"Status":"running","Pid":7},"ExecIDs":null}"#.into())
        );
    }
}
