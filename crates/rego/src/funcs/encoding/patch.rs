//! OPA's json.filter, json.remove and json.patch (topdown/json.go), and the EditTree
//! json.patch edits through (internal/edittree): paths parsed and folded into a filter
//! object as there, and each patch operation's checks, quirks and error texts kept.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::rego_string;
use crate::funcs::BuiltinError;
use crate::value::{Number, Value};

/// A path segment tree: pathsToObject's object, `Null` where a path ends.
#[derive(Debug, Clone)]
pub enum PathNode {
    Null,
    Obj(BTreeMap<Value, PathNode>),
}

/// parsePath: a JSON pointer, or an array of segments.
pub fn parse_path(path: &Value) -> Result<Vec<Value>, BuiltinError> {
    match path {
        Value::String(p) => {
            if p.is_empty() {
                return Ok(Vec::new());
            }
            let s = p.trim_start_matches('/');
            Ok(s.split('/')
                .map(|part| Value::string(part.replace("~1", "/").replace("~0", "~")))
                .collect())
        }
        Value::Array(a) => Ok(a.iter().cloned().collect()),
        other => Err(BuiltinError::operand(
            2,
            format!(
                "must be one of {{set, array}} containing string paths or array of path segments but got {}",
                other.type_name()
            ),
        )),
    }
}

/// getJSONPaths: the paths of an array or a set.
pub fn json_paths(operand: &Value) -> Result<Vec<Vec<Value>>, BuiltinError> {
    match operand {
        Value::Array(a) => a.iter().map(parse_path).collect(),
        Value::Set(s) => s.iter().map(parse_path).collect(),
        other => Err(BuiltinError::operand_type(2, other, &["set", "array"])),
    }
}

/// pathsToObject.
pub fn paths_to_object(paths: &[Vec<Value>]) -> BTreeMap<Value, PathNode> {
    let mut root = BTreeMap::new();
    for path in paths {
        insert_path(&mut root, path);
    }
    root
}

fn insert_path(root: &mut BTreeMap<Value, PathNode>, path: &[Value]) {
    let Some((last, prefix)) = path.split_last() else {
        return;
    };
    let mut node = root;
    for k in prefix {
        match node
            .entry(k.clone())
            .or_insert_with(|| PathNode::Obj(BTreeMap::new()))
        {
            PathNode::Obj(m) => node = m,
            // A path already ending here holds everything below it.
            PathNode::Null => return,
        }
    }
    node.insert(last.clone(), PathNode::Null);
}

fn index_key(i: usize) -> Value {
    Value::string(i.to_string())
}

/// filterObject.
pub fn filter(o: &Value, f: &PathNode) -> Value {
    let PathNode::Obj(fo) = f else {
        return o.clone();
    };
    match o {
        Value::Array(a) => Value::array(
            a.iter()
                .enumerate()
                .filter_map(|(i, v)| fo.get(&index_key(i)).map(|sub| filter(v, sub)))
                .collect(),
        ),
        Value::Set(s) => Value::set(
            s.iter()
                .filter_map(|t| fo.get(t).map(|sub| filter(t, sub)))
                .collect(),
        ),
        Value::Object(m) => Value::object(
            m.iter()
                .filter_map(|(k, v)| fo.get(k).map(|sub| (k.clone(), filter(v, sub))))
                .collect(),
        ),
        _ => o.clone(),
    }
}

/// jsonRemove: None where the value is removed.
pub fn remove(a: &Value, b: Option<&PathNode>) -> Option<Value> {
    let bo = match b {
        None => return Some(a.clone()),
        Some(PathNode::Null) => return None,
        Some(PathNode::Obj(bo)) => bo,
    };
    Some(match a {
        Value::Object(m) => Value::object(
            m.iter()
                .filter_map(|(k, v)| remove(v, bo.get(k)).map(|d| (k.clone(), d)))
                .collect(),
        ),
        Value::Set(s) => Value::set(s.iter().filter_map(|v| remove(v, bo.get(v))).collect()),
        Value::Array(arr) => Value::array(
            arr.iter()
                .enumerate()
                .filter_map(|(i, v)| remove(v, bo.get(&index_key(i))))
                .collect(),
        ),
        _ => a.clone(),
    })
}

// ---- EditTree ----

fn is_composite(v: &Value) -> bool {
    matches!(v, Value::Object(_) | Value::Set(_) | Value::Array(_))
}

/// %T of an ast.Value.
fn go_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "ast.Null",
        Value::Bool(_) => "ast.Boolean",
        Value::Number(_) => "ast.Number",
        Value::String(_) => "ast.String",
        Value::Array(_) => "*ast.Array",
        Value::Object(_) => "*ast.object",
        Value::Set(_) => "*ast.set",
    }
}

fn not_composite(v: &Value) -> String {
    format!(
        "expected composite type, found value: {} (type: {})",
        rego_string(v),
        go_type(v)
    )
}

/// strconv.ParseInt(s, 10, 64).
fn parse_int10(s: &str) -> Option<i64> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// toIndex.
fn to_index(len: usize, term: &Value) -> Result<i64, String> {
    match term {
        Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| "invalid number type for indexing".to_string()),
        Value::String(s) => {
            if &**s == "-" {
                return Ok(i64::try_from(len).unwrap_or(i64::MAX));
            }
            let i = parse_int10(s).ok_or_else(|| "invalid string for indexing".to_string())?;
            if &**s != "0" && s.starts_with('0') {
                return Err("leading zeros are not allowed in JSON paths".to_string());
            }
            Ok(i)
        }
        _ => Err("invalid type for indexing".to_string()),
    }
}

/// ast.Value.Find with a one-element path.
fn find1(v: &Value, key: &Value) -> Option<Value> {
    match v {
        Value::Object(m) => m.get(key).cloned(),
        Value::Set(s) => s.contains(key).then(|| key.clone()),
        Value::Array(a) => {
            let Value::Number(n) = key else { return None };
            let i = usize::try_from(n.as_i64()?).ok()?;
            a.get(i).cloned()
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Default)]
struct EditTree {
    value: Option<Value>,
    keys: BTreeSet<Value>,
    scalars: BTreeMap<Value, Option<Value>>,
    composites: BTreeMap<Value, Option<EditTree>>,
    ascalars: BTreeMap<i64, Value>,
    acomposites: BTreeMap<i64, EditTree>,
    eliminated: Vec<bool>,
    insertions: Vec<bool>,
}

/// A tree unfolded as deep as its value drops one node after another.
impl Drop for EditTree {
    fn drop(&mut self) {
        let mut rest = Vec::new();
        take_children(self, &mut rest);
        while let Some(mut t) = rest.pop() {
            take_children(&mut t, &mut rest);
        }
    }
}

fn take_children(t: &mut EditTree, out: &mut Vec<EditTree>) {
    out.extend(std::mem::take(&mut t.composites).into_values().flatten());
    out.extend(std::mem::take(&mut t.acomposites).into_values());
}

/// Unfold's destination: a node in the tree, or a scalar standing alone.
enum Dest<'a> {
    Node(&'a mut EditTree),
    Alone(EditTree),
}

impl Dest<'_> {
    fn tree(&mut self) -> &mut EditTree {
        match self {
            Dest::Node(t) => t,
            Dest::Alone(t) => t,
        }
    }
}

impl EditTree {
    fn new(v: Value) -> EditTree {
        let mut t = EditTree::default();
        if let Value::Array(a) = &v {
            t.eliminated = vec![false; a.len()];
            t.insertions = vec![false; a.len()];
        }
        t.value = Some(v);
        t
    }

    fn clear(&mut self) {
        self.keys.clear();
        self.scalars.clear();
        self.composites.clear();
        self.ascalars.clear();
        self.acomposites.clear();
    }

    fn len(&self) -> usize {
        self.insertions.len()
    }

    fn delete_child_value_obj(&mut self, key: &Value) {
        self.scalars.remove(key);
        self.composites.remove(key);
    }

    fn delete_child_value_arr(&mut self, i: i64) {
        self.ascalars.remove(&i);
        self.acomposites.remove(&i);
    }

    fn insert_keyed(&mut self, key: Value, value: Value) -> Dest<'_> {
        if self.keys.contains(&key) {
            self.delete_child_value_obj(&key);
        }
        self.keys.insert(key.clone());
        if is_composite(&value) {
            let slot = self.composites.entry(key).or_insert(None);
            *slot = Some(EditTree::new(value));
            match slot {
                Some(child) => Dest::Node(child),
                None => Dest::Alone(EditTree::default()),
            }
        } else {
            self.scalars.insert(key, Some(value.clone()));
            Dest::Alone(EditTree::new(value))
        }
    }

    fn insert(&mut self, key: Value, value: Value) -> Result<Dest<'_>, String> {
        let Some(v) = self.value.clone() else {
            return Err("deleted node encountered during insert operation".to_string());
        };
        match v {
            Value::Object(_) => Ok(self.insert_keyed(key, value)),
            Value::Set(_) => {
                if key != value {
                    return Err(format!(
                        "set key {} does not equal value to be inserted {}",
                        rego_string(&key),
                        rego_string(&value)
                    ));
                }
                if is_composite(&key) {
                    self.value = self.render();
                    self.clear();
                }
                Ok(self.insert_keyed(key, value))
            }
            Value::Array(_) => {
                let len = self.len();
                let idx = to_index(len, &key)?;
                if idx < 0 || idx > i64::try_from(len).unwrap_or(i64::MAX) {
                    return Err("index for array insertion out of bounds".to_string());
                }
                self.insert_array(idx, value)
            }
            other => Err(not_composite(&other)),
        }
    }

    fn insert_array(&mut self, idx: i64, value: Value) -> Result<Dest<'_>, String> {
        let len = i64::try_from(self.len()).unwrap_or(i64::MAX);
        let mut rs = Vec::new();
        let mut rc = Vec::new();
        for i in idx..len {
            if self.insertions.get(usize::try_from(i).unwrap_or(usize::MAX)) == Some(&true) {
                if self.ascalars.contains_key(&i) {
                    rs.push(i);
                } else if self.acomposites.contains_key(&i) {
                    rc.push(i);
                } else {
                    return Err(format!("invalid index {i} during Insert operation"));
                }
            }
        }
        for &i in rs.iter().rev() {
            let v = self.ascalars.get(&i).cloned();
            self.delete_child_value_arr(i);
            if let Some(v) = v {
                self.ascalars.insert(i + 1, v);
            }
        }
        for &i in rc.iter().rev() {
            let v = self.acomposites.remove(&i);
            self.delete_child_value_arr(i);
            if let Some(v) = v {
                self.acomposites.insert(i + 1, v);
            }
        }
        let at = usize::try_from(idx).unwrap_or(0);
        if at >= self.insertions.len() {
            self.insertions.push(true);
        } else {
            self.insertions.insert(at, true);
        }
        if is_composite(&value) {
            self.acomposites.insert(idx, EditTree::new(value));
            match self.acomposites.get_mut(&idx) {
                Some(child) => Ok(Dest::Node(child)),
                None => Ok(Dest::Alone(EditTree::default())),
            }
        } else {
            self.ascalars.insert(idx, value.clone());
            Ok(Dest::Alone(EditTree::new(value)))
        }
    }

    fn fallback_delete(&mut self, key: &Value) -> Result<(), String> {
        let found = self.value.as_ref().and_then(|v| find1(v, key));
        let Some(value) = found else {
            return Err(format!(
                "cannot delete child key {} that does not exist",
                rego_string(key)
            ));
        };
        self.keys.insert(key.clone());
        if is_composite(&value) {
            self.composites.insert(key.clone(), None);
        } else {
            self.scalars.insert(key.clone(), None);
        }
        Ok(())
    }

    fn delete(&mut self, key: &Value) -> Result<(), String> {
        let Some(v) = self.value.clone() else {
            return Err("deleted node encountered during delete operation".to_string());
        };
        match v {
            Value::Object(_) => {
                if self.keys.contains(key) {
                    if let Some(child) = self.scalars.get(key) {
                        if child.is_none() {
                            return Err(format!(
                                "cannot delete the already deleted scalar node for key {}",
                                rego_string(key)
                            ));
                        }
                        self.scalars.insert(key.clone(), None);
                        return Ok(());
                    }
                    if let Some(child) = self.composites.get(key) {
                        if child.is_none() {
                            return Err(format!(
                                "cannot delete the already deleted composite node for key {}",
                                rego_string(key)
                            ));
                        }
                        self.composites.insert(key.clone(), None);
                        return Ok(());
                    }
                    return Err("hash value not found in scalar or composite child maps".to_string());
                }
                self.fallback_delete(key)
            }
            Value::Set(_) => {
                if is_composite(key) {
                    self.value = self.render();
                    self.clear();
                } else if self.keys.contains(key)
                    && let Some(child) = self.scalars.get(key)
                {
                    match child {
                        None => {
                            return Err(format!(
                                "cannot delete the already deleted scalar node for key {}",
                                rego_string(key)
                            ));
                        }
                        Some(c) if c == key => {
                            self.scalars.insert(key.clone(), None);
                            return Ok(());
                        }
                        Some(_) => {}
                    }
                }
                self.fallback_delete(key)
            }
            Value::Array(_) => {
                let len = self.len();
                let idx = to_index(len, key)?;
                let ilen = i64::try_from(len).unwrap_or(i64::MAX);
                if idx < 0 || idx > ilen - 1 {
                    return Err("index for array delete out of bounds".to_string());
                }
                let mut rs = Vec::new();
                let mut rc = Vec::new();
                for i in idx + 1..ilen {
                    if self.insertions.get(usize::try_from(i).unwrap_or(usize::MAX)) == Some(&true) {
                        if self.ascalars.contains_key(&i) {
                            rs.push(i);
                        } else if self.acomposites.contains_key(&i) {
                            rc.push(i);
                        } else {
                            return Err(format!("invalid index {i} during Insert operation"));
                        }
                    }
                }
                self.delete_child_value_arr(idx);
                for &i in &rs {
                    let v = self.ascalars.get(&i).cloned();
                    self.delete_child_value_arr(i);
                    if let Some(v) = v {
                        self.ascalars.insert(i - 1, v);
                    }
                }
                for &i in &rc {
                    let v = self.acomposites.remove(&i);
                    self.delete_child_value_arr(i);
                    if let Some(v) = v {
                        self.acomposites.insert(i - 1, v);
                    }
                }
                let at = usize::try_from(idx).unwrap_or(0);
                if self.insertions.get(at) == Some(&false) {
                    let zeroes_seen = 1 + self.insertions.iter().take(at).filter(|b| !**b).count();
                    let mut seen = 0;
                    let mut elim = None;
                    for (i, b) in self.eliminated.iter().enumerate() {
                        if !*b {
                            seen += 1;
                        }
                        if seen == zeroes_seen {
                            elim = Some(i);
                            break;
                        }
                    }
                    let Some(e) = elim else {
                        return Err(format!("could not successfully eliminate index {idx} from array"));
                    };
                    if let Some(slot) = self.eliminated.get_mut(e) {
                        *slot = true;
                    }
                }
                if at < self.insertions.len() {
                    self.insertions.remove(at);
                }
                Ok(())
            }
            other => Err(not_composite(&other)),
        }
    }

    /// Unfold: the node at `path`, each node on the way unfolded in turn (OPA recurses a
    /// step a node; a path as long as the value is deep walks here without a frame a
    /// step).
    fn unfold(&mut self, path: &[Value]) -> Result<Dest<'_>, String> {
        let mut node = self;
        let mut path = path;
        loop {
            let Some((key, rest)) = path.split_first() else {
                return Ok(Dest::Node(node));
            };
            path = rest;
            let Some(v) = node.value.clone() else {
                return Err("nil value encountered where composite value was expected".to_string());
            };
            let next = match &v {
                Value::Object(_) => {
                    if node.keys.contains(key) {
                        if let Some(term) = node.scalars.get(key) {
                            let Some(term) = term.clone() else {
                                return Err(format!(
                                    "cannot unfold the already deleted scalar node for key {}",
                                    rego_string(key)
                                ));
                            };
                            return alone(EditTree::new(term), rest);
                        }
                        match node.composites.get_mut(key) {
                            Some(Some(child)) => Dest::Node(child),
                            Some(None) => {
                                return Err(format!(
                                    "cannot unfold the already deleted composite node for key {}",
                                    rego_string(key)
                                ));
                            }
                            None => {
                                return Err(
                                    "hash value not found in scalar or composite child maps".to_string()
                                );
                            }
                        }
                    } else if let Some(found) = find1(&v, key) {
                        node.insert(key.clone(), found)?
                    } else {
                        return Err(format!(
                            "path {} does not exist in object term {}",
                            rego_string(key),
                            rego_string(&v)
                        ));
                    }
                }
                Value::Set(_) => {
                    if is_composite(key) {
                        node.value = node.render();
                        node.clear();
                    } else if node.keys.contains(key)
                        && let Some(term) = node.scalars.get(key)
                    {
                        let Some(term) = term.clone() else {
                            return Err(
                                "nil value encountered where composite value was expected".to_string()
                            );
                        };
                        return alone(EditTree::new(term), rest);
                    }
                    let current = node.value.clone().unwrap_or(Value::Null);
                    let Some(found) = find1(&current, key) else {
                        let ref_text = match key {
                            Value::String(s) => s.to_string(),
                            other => rego_string(other),
                        };
                        return Err(format!(
                            "path {} does not exist in set term {}",
                            ref_text,
                            rego_string(&current)
                        ));
                    };
                    node.insert(key.clone(), found)?
                }
                Value::Array(_) => {
                    let idx = to_index(node.len(), key)?;
                    if let Some(term) = node.ascalars.get(&idx) {
                        return alone(EditTree::new(term.clone()), rest);
                    }
                    if node.acomposites.contains_key(&idx) {
                        match node.acomposites.get_mut(&idx) {
                            Some(child) => Dest::Node(child),
                            None => return Err("invalid index".to_string()),
                        }
                    } else {
                        let idxt = Value::Number(Number::from_i64(idx));
                        let Some(found) = find1(&v, &idxt) else {
                            return Err(format!(
                                "path {idx} does not exist in array term {}",
                                rego_string(&v)
                            ));
                        };
                        node.delete(&idxt)?;
                        node.insert(idxt, found)?
                    }
                }
                other => {
                    return Err(format!(
                        "expected composite type for path {}, found value: {} (type: {})",
                        rego_string(key),
                        rego_string(other),
                        go_type(other)
                    ));
                }
            };
            match next {
                Dest::Node(child) => node = child,
                Dest::Alone(child) => return alone(child, rest),
            }
        }
    }

    /// Render: the value the tree stands for now, each node rendered after the nodes
    /// below it (OPA recurses a node a frame; a tree unfolded as deep as its value is
    /// renders here without).
    fn render(&self) -> Option<Value> {
        let addr = |t: &EditTree| std::ptr::from_ref(t) as usize;
        let mut order: Vec<&EditTree> = Vec::new();
        let mut stack = vec![self];
        while let Some(t) = stack.pop() {
            order.push(t);
            stack.extend(t.composites.values().flatten());
            stack.extend(t.acomposites.values());
        }
        let mut done: HashMap<usize, Option<Value>> = HashMap::with_capacity(order.len());
        for t in order.into_iter().rev() {
            let r = t.render_one(&|c| done.get(&addr(c)).cloned().flatten());
            done.insert(addr(t), r);
        }
        done.remove(&addr(self)).flatten()
    }

    /// One node rendered, its children's renders from `child`.
    fn render_one(&self, child: &dyn Fn(&EditTree) -> Option<Value>) -> Option<Value> {
        let v = self.value.as_ref()?;
        Some(match v {
            Value::Object(m) => {
                if self.keys.is_empty() {
                    return Some(v.clone());
                }
                let mut out = BTreeMap::new();
                let mut skip = BTreeSet::new();
                for (k, t) in &self.scalars {
                    skip.insert(k.clone());
                    if let Some(t) = t {
                        out.insert(k.clone(), t.clone());
                    }
                }
                for (k, c) in &self.composites {
                    skip.insert(k.clone());
                    if let Some(r) = c.as_ref().and_then(child) {
                        out.insert(k.clone(), r);
                    }
                }
                for (k, v) in m.iter() {
                    if !skip.contains(k) {
                        out.insert(k.clone(), v.clone());
                    }
                }
                Value::object(out)
            }
            Value::Set(s) => {
                if self.keys.is_empty() {
                    return Some(v.clone());
                }
                let mut out = BTreeSet::new();
                let mut skip = BTreeSet::new();
                for (k, t) in &self.scalars {
                    skip.insert(k.clone());
                    if let Some(t) = t {
                        out.insert(t.clone());
                    }
                }
                for (k, c) in &self.composites {
                    skip.insert(k.clone());
                    if let Some(r) = c.as_ref().and_then(child) {
                        out.insert(r);
                    }
                }
                for k in s.iter() {
                    if !skip.contains(k) {
                        out.insert(k.clone());
                    }
                }
                Value::set(out)
            }
            Value::Array(a) => {
                let mut out = Vec::with_capacity(self.len());
                let mut e_idx = 0usize;
                for (i, ins) in self.insertions.iter().enumerate() {
                    let ii = i64::try_from(i).unwrap_or(i64::MAX);
                    if !*ins {
                        let mut found = None;
                        let mut j = e_idx;
                        while j < self.eliminated.len() {
                            if self.eliminated.get(j) == Some(&false) {
                                found = Some(j);
                                break;
                            }
                            j += 1;
                        }
                        let j = found?;
                        e_idx = j;
                        out.push(a.get(e_idx)?.clone());
                        e_idx += 1;
                    } else if let Some(t) = self.ascalars.get(&ii) {
                        out.push(t.clone());
                    } else {
                        out.push(child(self.acomposites.get(&ii)?)?);
                    }
                }
                Value::array(out)
            }
            _ => v.clone(),
        })
    }

    fn insert_at_path(&mut self, path: &[Value], value: Option<Value>) -> Result<(), String> {
        let Some(value) = value else {
            return Err("cannot insert nil value into EditTree".to_string());
        };
        let Some((last, prefix)) = path.split_last() else {
            self.clear();
            if let Value::Array(a) = &value {
                self.eliminated = vec![false; a.len()];
                self.insertions = vec![false; a.len()];
            }
            self.value = Some(value);
            return Ok(());
        };
        let mut dest = self.unfold(prefix)?;
        dest.tree().insert(last.clone(), value)?;
        Ok(())
    }

    fn delete_at_path(&mut self, path: &[Value]) -> Result<(), String> {
        let Some((last, prefix)) = path.split_last() else {
            if self.value.is_none() {
                return Err("deleted node encountered during delete operation".to_string());
            }
            *self = EditTree::default();
            return Ok(());
        };
        let mut dest = self.unfold(prefix)?;
        dest.tree().delete(last)
    }

    fn render_at_path(&mut self, path: &[Value]) -> Result<Option<Value>, String> {
        let mut dest = self.unfold(path)?;
        Ok(dest.tree().render())
    }
}

/// Unfold of a scalar standing alone: only an empty path reaches it.
fn alone<'a>(t: EditTree, rest: &[Value]) -> Result<Dest<'a>, String> {
    let Some(key) = rest.first() else {
        return Ok(Dest::Alone(t));
    };
    let v = t.value.clone().unwrap_or(Value::Null);
    if t.value.is_none() {
        return Err("nil value encountered where composite value was expected".to_string());
    }
    Err(format!(
        "expected composite type for path {}, found value: {} (type: {})",
        rego_string(key),
        rego_string(&v),
        go_type(&v)
    ))
}

/// What a patch step failed on: a plain error, or an operand error from parsePath.
pub enum PatchError {
    Plain(String),
    Builtin(BuiltinError),
}

impl From<String> for PatchError {
    fn from(s: String) -> PatchError {
        PatchError::Plain(s)
    }
}

impl From<BuiltinError> for PatchError {
    fn from(e: BuiltinError) -> PatchError {
        PatchError::Builtin(e)
    }
}

/// applyPatches.
pub fn apply_patches(source: &Value, operations: &[Value]) -> Result<Option<Value>, PatchError> {
    let mut et = EditTree::new(source.clone());
    let attr = |o: &BTreeMap<Value, Value>, name: &str| o.get(&Value::string(name)).cloned();
    for op in operations {
        let Value::Object(object) = op else {
            return Err(
                "must be an array of JSON-Patch objects, but at least one element is not an object"
                    .to_string()
                    .into(),
            );
        };
        let Some(path_v) = attr(object, "path") else {
            return Err("missing required attribute 'path'".to_string().into());
        };
        let Some(op_term) = attr(object, "op") else {
            return Err("missing required attribute 'op'".to_string().into());
        };
        let Value::String(op_str) = &op_term else {
            return Err(format!(
                "attribute 'op' must be a string but found: {}",
                op_term.type_name()
            )
            .into());
        };
        let path = parse_path(&path_v)?;
        let value_attr =
            || attr(object, "value").ok_or_else(|| "missing required attribute 'value'".to_string());
        match &**op_str {
            "add" => {
                let value = value_attr()?;
                et.insert_at_path(&path, Some(value))?;
            }
            "remove" => et.delete_at_path(&path)?,
            "replace" => {
                et.delete_at_path(&path)?;
                let value = value_attr()?;
                et.insert_at_path(&path, Some(value))?;
            }
            "move" | "copy" => {
                let Some(from_v) = attr(object, "from") else {
                    return Err("missing required attribute 'from'".to_string().into());
                };
                let from = parse_path(&from_v)?;
                let chunk = et.render_at_path(&from)?;
                if &**op_str == "move" {
                    et.delete_at_path(&from)?;
                }
                et.insert_at_path(&path, chunk)?;
            }
            "test" => {
                let chunk = et.render_at_path(&path)?;
                let value = value_attr()?;
                let chunk = chunk.unwrap_or(Value::Null);
                if chunk != value {
                    return Err(format!(
                        "value from EditTree != patch value.\n\nExpected: {}\n\nFound: {}",
                        rego_string(&value),
                        rego_string(&chunk)
                    )
                    .into());
                }
            }
            other => return Err(format!("unrecognized op: '{other}'").into()),
        }
    }
    Ok(et.render())
}
