//! OPA v1.14.1's builtins (src/builtins.json, written by scripts/rego/generate from
//! `ast.Builtins`): their names, infix operators, declarations, and whether buildx's
//! policies may call them (buildx policy/builtins.go).

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::types::Type;

#[derive(Debug, Clone)]
pub struct Builtin {
    pub name: String,
    pub infix: Option<String>,
    pub relation: bool,
    pub deprecated: bool,
    pub allowed: bool,
    /// The declaration: a function type.
    pub decl: Type,
}

/// Every OPA builtin by name. Empty only if the embedded data failed to read, which
/// the crate's tests rule out.
pub fn registry() -> &'static HashMap<String, Builtin> {
    static REGISTRY: OnceLock<HashMap<String, Builtin>> = OnceLock::new();
    REGISTRY.get_or_init(|| read(include_str!("builtins.json")).unwrap_or_default())
}

fn read(text: &str) -> Option<HashMap<String, Builtin>> {
    let list: serde_json::Value = serde_json::from_str(text).ok()?;
    let mut out = HashMap::new();
    for e in list.as_array()? {
        let name = e.get("name")?.as_str()?.to_string();
        let flag = |k: &str| e.get(k).and_then(serde_json::Value::as_bool).unwrap_or(false);
        let b = Builtin {
            name: name.clone(),
            infix: e.get("infix").and_then(|v| v.as_str()).map(str::to_string),
            relation: flag("relation"),
            deprecated: flag("deprecated"),
            allowed: flag("allowed"),
            decl: Type::from_json(e.get("decl")?)?,
        };
        out.insert(name, b);
    }
    Some(out)
}
