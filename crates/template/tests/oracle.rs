//! shards-template runs templates as Go's text/template does, with docker/cli's
//! functions: every case in oracle.json (scripts/template/generate, Go 1.26.1), parsed and
//! run here, gives Go's output and error byte for byte.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;

use shards_template::{Kind, Object, Template, Value};

/// oracle_test.go's Person: a struct value.
#[derive(Debug)]
struct Person {
    name: String,
    age: i64,
    tags: Option<Vec<String>>,
    meta: Option<BTreeMap<String, String>>,
}

fn strings(v: &Option<Vec<String>>) -> Value {
    Value::strings(v.clone().unwrap_or_default())
}

fn string_map(m: &Option<BTreeMap<String, String>>) -> Value {
    Value::string_map(m.clone().unwrap_or_default())
}

impl Object for Person {
    fn type_name(&self) -> &str {
        "oracle.Person"
    }

    fn field(&self, name: &str) -> Option<Value> {
        Some(match name {
            "Name" => Value::String(self.name.clone()),
            "Age" => Value::Int(self.age),
            "Tags" => strings(&self.tags),
            "Meta" => string_map(&self.meta),
            _ => return None,
        })
    }

    fn method(&self, name: &str) -> Option<&'static [Kind]> {
        Some(match name {
            "Greet" | "Label" => &[Kind::String],
            "Initial" | "Fail" => &[],
            "Sum" => &[Kind::Int, Kind::Int],
            "Scale" => &[Kind::Float],
            "Older" => &[Kind::Uint],
            "Pick" => &[Kind::Bool],
            "Kind" => &[Kind::Any],
            _ => return None,
        })
    }

    fn call(&self, name: &str, args: &[Value]) -> Result<Value, String> {
        let s = |i: usize| match args.get(i) {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        let int = |i: usize| match args.get(i) {
            Some(Value::Int(n)) => *n,
            _ => 0,
        };
        Ok(match name {
            "Greet" => Value::String(format!("{}, {}", s(0), self.name)),
            "Label" => Value::String(
                self.meta
                    .as_ref()
                    .and_then(|m| m.get(&s(0)).cloned())
                    .unwrap_or_default(),
            ),
            "Initial" => Value::String(self.name.chars().take(1).collect()),
            "Fail" => return Err("person failed".into()),
            "Sum" => Value::Int(int(0) + int(1)),
            "Scale" => match args.first() {
                Some(Value::Float(f)) => Value::Float(self.age as f64 * f),
                _ => Value::Float(0.0),
            },
            "Older" => match args.first() {
                Some(Value::Uint(n)) => Value::Bool(self.age as u64 > *n),
                _ => Value::Bool(false),
            },
            "Pick" => Value::String(
                if matches!(args.first(), Some(Value::Bool(true))) {
                    "yes"
                } else {
                    "no"
                }
                .into(),
            ),
            "Kind" => Value::String(args.first().map_or("<nil>".into(), Value::type_name)),
            _ => return Err(format!("no method {name}")),
        })
    }

    fn format(&self, out: &mut String) {
        let tags = self.tags.clone().unwrap_or_default().join(" ");
        let meta: Vec<String> = self
            .meta
            .clone()
            .unwrap_or_default()
            .iter()
            .map(|(k, v)| format!("{k}:{v}"))
            .collect();
        out.push_str(&format!(
            "{{{} {} [{tags}] map[{}]}}",
            self.name,
            self.age,
            meta.join(" ")
        ));
    }

    fn json(&self, out: &mut String) -> Result<(), String> {
        let tags = match &self.tags {
            Some(t) => serde_json::to_string(t).unwrap(),
            None => "null".into(),
        };
        let meta = match &self.meta {
            Some(m) => serde_json::to_string(m).unwrap(),
            None => "null".into(),
        };
        let name = serde_json::to_string(&self.name).unwrap();
        out.push_str(&format!(
            r#"{{"Name":{name},"Age":{},"Tags":{tags},"Meta":{meta}}}"#,
            self.age
        ));
        Ok(())
    }
}

/// oracle_test.go's Container: a pointer to a struct with methods only.
#[derive(Debug)]
struct Container {
    id: String,
    names: Vec<String>,
    labels: BTreeMap<String, String>,
}

impl Object for Container {
    fn type_name(&self) -> &str {
        "*oracle.Container"
    }

    fn method(&self, name: &str) -> Option<&'static [Kind]> {
        Some(match name {
            "ID" | "Names" | "Labels" => &[],
            "Label" => &[Kind::String],
            _ => return None,
        })
    }

    fn call(&self, name: &str, args: &[Value]) -> Result<Value, String> {
        Ok(Value::String(match name {
            "ID" => self.id.clone(),
            "Names" => self.names.join(","),
            "Label" => match args.first() {
                Some(Value::String(k)) => self.labels.get(k).cloned().unwrap_or_default(),
                _ => String::new(),
            },
            "Labels" => self
                .labels
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(","),
            _ => return Err(format!("no method {name}")),
        }))
    }

    fn format(&self, out: &mut String) {
        let labels: Vec<String> = self.labels.iter().map(|(k, v)| format!("{k}:{v}")).collect();
        out.push_str(&format!(
            "{{{} [{}] map[{}]}}",
            self.id,
            self.names.join(" "),
            labels.join(" ")
        ));
    }

    fn json(&self, out: &mut String) -> Result<(), String> {
        out.push_str("{}");
        Ok(())
    }
}

fn str_list(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn str_map(v: &serde_json::Value) -> BTreeMap<String, String> {
    v.as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect()
}

/// A nil `*Person`.
#[derive(Debug)]
struct NilPerson;

impl Object for NilPerson {
    fn type_name(&self) -> &str {
        "*oracle.Person"
    }

    fn is_nil(&self) -> bool {
        true
    }

    fn format(&self, out: &mut String) {
        out.push_str("<nil>");
    }

    fn json(&self, out: &mut String) -> Result<(), String> {
        out.push_str("null");
        Ok(())
    }
}

/// oracle_test.go's convert: JSON, with its tags for Go's other types.
fn convert(v: &serde_json::Value) -> Value {
    use serde_json::Value as J;
    match v {
        J::Null => Value::Nil,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) if n.is_f64() => Value::Float(n.as_f64().unwrap()),
        J::Number(n) => Value::Int(n.as_i64().unwrap()),
        J::String(s) => Value::String(s.clone()),
        J::Array(items) => Value::list(items.iter().map(convert).collect()),
        J::Object(m) => {
            if let Some(n) = m.get("$uint") {
                return Value::Uint(n.as_u64().unwrap());
            }
            if let Some(l) = m.get("$strings") {
                return Value::strings(str_list(l));
            }
            if let Some(s) = m.get("$strmap") {
                return Value::string_map(str_map(s));
            }
            if m.contains_key("$nilstrings") {
                return Value::NilList(Kind::String);
            }
            if m.contains_key("$nilstrmap") {
                return Value::NilMap(Kind::String);
            }
            if m.contains_key("$nilperson") {
                return Value::object(NilPerson);
            }
            match m.get("$object").and_then(J::as_str) {
                Some("Person") => {
                    return Value::object(Person {
                        name: m["Name"].as_str().unwrap().into(),
                        age: m["Age"].as_i64().unwrap(),
                        tags: m.get("Tags").map(str_list),
                        meta: m.get("Meta").map(str_map),
                    });
                }
                Some("Container") => {
                    return Value::object(Container {
                        id: m["ID"].as_str().unwrap().into(),
                        names: str_list(&m["Names"]),
                        labels: str_map(&m["Labels"]),
                    });
                }
                _ => {}
            }
            Value::map(m.iter().map(|(k, v)| (k.clone(), convert(v))).collect())
        }
    }
}

/// The cases shards answers differently on purpose, by template name, data and
/// template, and why.
const DEVIATIONS: &[(&str, &str, &str, &str)] = &[
    (
        "",
        "m",
        r#"{{define "T"}}{{template "T" .}}{{end}}{{template "T" .}}"#,
        "Go stops recursion at 100000 calls, on stacks that grow; shards at 200 (exec.rs's MAX_EXEC_DEPTH)",
    ),
    (
        "",
        "m",
        r#"{{index .str 1 | printf "%T"}}"#,
        "a string's byte is a uint here: Value has no uint8",
    ),
    (
        "a%b",
        "m",
        "{{.str.x}}",
        "Go puts the template's name into its error's format string, so a % in it garbles the error",
    ),
    ("a%b", "m", "{{.str", "as above"),
    (
        "",
        "m",
        r#"{{printf "%+v" .person}}"#,
        "objects print as their format says for every verb, without fmt's field names for %+v",
    ),
    (
        "",
        "m",
        r#"{{printf "%s" .person}}"#,
        "as above: Go applies %s to each field",
    ),
];

#[test]
fn templates_run_as_go_runs_them() {
    let golden: serde_json::Value = serde_json::from_str(include_str!("oracle.json")).unwrap();
    assert_eq!(golden["go"], "go1.26.1");
    let data: BTreeMap<String, Value> = golden["data"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), convert(v)))
        .collect();
    let cases = golden["cases"].as_array().unwrap();
    assert!(cases.len() > 800);
    let mut failures = Vec::new();
    for case in cases {
        let text = case["template"].as_str().unwrap();
        let name = case.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let header = case.get("header").and_then(|h| h.as_bool()).unwrap_or(false);
        let missing_key = case.get("missingkey").and_then(|h| h.as_bool()).unwrap_or(false);
        let want_out = case["output"].as_str().unwrap();
        let want_err = case.get("error").and_then(|e| e.as_str());
        let value = &data[case["data"].as_str().unwrap()];
        let mut out = String::new();
        let got_err = match Template::parse(name, text) {
            Err(e) => Some(e),
            Ok(t) => {
                let t = if missing_key { t.missing_key_error() } else { t };
                let r = if header {
                    t.execute_header_into(value, &mut out)
                } else {
                    t.execute_into(value, &mut out)
                };
                r.err()
            }
        };
        let deviates = DEVIATIONS
            .iter()
            .any(|(n, d, t, _)| *n == name && case["data"] == *d && *t == text);
        let same = out == want_out && got_err.as_deref() == want_err;
        if deviates {
            assert!(!same, "{text:?} is listed as a deviation but matches Go");
            continue;
        }
        if !same {
            failures.push(format!(
                "{text:?} on {}{}:\n  want {want_out:?} {want_err:?}\n  got  {out:?} {got_err:?}",
                case["data"],
                if header { " (header)" } else { "" }
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}
