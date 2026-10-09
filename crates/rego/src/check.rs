//! OPA's type checker (ast/check.go and ast/env.go, v1.14.1): the type environment
//! (TypeEnv and its typeTreeNode), the types it infers for rules, bodies and terms, and
//! the type errors it reports, written as OPA writes them. Held to OPA by
//! `tests/check.rs`, against what `scripts/rego/generate` records.
//!
//! Where OPA walks a Go map (a node's children, the leaves Insert merges), this walks the
//! keys in OPA's term order; the answers that differ by that order are recorded as such.

use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::{Expr, ExprTerms, Location, RuleKind, TemplatePart, Term, TermValue};
use crate::compare::term_compare;
use crate::compile::safety::walk_terms;
use crate::compile::vars::{Var, sorted_items, sorted_pairs};
use crate::compile::{CompileError, TYPE_ERR, ground_prefix, text_of_ref};
use crate::types::{self, A, FuncArgs, Key, Type};

/// typeTreeNode: a type, or none (Nil), and the nodes below by key.
#[derive(Debug, Clone)]
pub struct Node {
    value: Type,
    children: Vec<(Term, Node)>,
}

impl Default for Node {
    fn default() -> Node {
        Node {
            value: Type::Nil,
            children: Vec::new(),
        }
    }
}

fn same(a: &Term, b: &Term) -> bool {
    term_compare(a, b) == std::cmp::Ordering::Equal
}

impl Node {
    fn child(&self, key: &Term) -> Option<&Node> {
        self.children.iter().find(|(k, _)| same(k, key)).map(|(_, n)| n)
    }

    fn leaf(&self) -> bool {
        !matches!(self.value, Type::Nil)
    }

    fn sorted_keys(&self) -> Vec<Term> {
        let mut keys: Vec<Term> = self.children.iter().map(|(k, _)| k.clone()).collect();
        keys.sort_by(term_compare);
        keys
    }

    fn at(&self, path: &[Term]) -> Option<&Node> {
        let mut n = self;
        for k in path {
            n = n.child(k)?;
        }
        Some(n)
    }

    fn child_mut(&mut self, key: &Term) -> Option<&mut Node> {
        let pos = match self.children.iter().position(|(k, _)| same(k, key)) {
            Some(p) => p,
            None => {
                self.children.push((key.clone(), Node::default()));
                self.children.len().saturating_sub(1)
            }
        };
        self.children.get_mut(pos).map(|(_, n)| n)
    }

    /// PutOne.
    pub fn put_one(&mut self, key: &Term, tpe: Type) {
        if let Some(c) = self.child_mut(key) {
            c.value = tpe;
        }
    }

    /// Put.
    pub fn put(&mut self, path: &[Term], tpe: Type) {
        let mut curr = self;
        for k in path {
            let Some(c) = curr.child_mut(k) else { return };
            curr = c;
        }
        curr.value = tpe;
    }

    /// Insert: tpe at path, merged into the object types along the path, and the leaves
    /// below merged into an object inserted.
    pub fn insert(&mut self, path: &[Term], tpe: Type) {
        let mut curr = self;
        for (i, k) in path.iter().enumerate() {
            let existed = curr.child(k).is_some();
            let Some(child) = curr.child_mut(k) else { return };
            if existed
                && matches!(child.value, Type::Object { .. })
                && let Some(rest) = path.get(i + 1..)
                && !rest.is_empty()
            {
                child.value = insert_into_object(&child.value, rest, &tpe);
            }
            curr = child;
        }
        curr.value = merge_types(&curr.value, &tpe);
        if matches!(tpe, Type::Object { .. }) && !curr.children.is_empty() {
            for (p, t) in curr.leafs() {
                // OPA asserts the merged value is an object here.
                if matches!(curr.value, Type::Object { .. }) {
                    curr.value = insert_into_object(&curr.value, &p, &t);
                }
            }
        }
    }

    /// Leafs: each leaf below, by its path from here.
    fn leafs(&self) -> Vec<(Vec<Term>, Type)> {
        fn collect(n: &Node, path: Vec<Term>, out: &mut Vec<(Vec<Term>, Type)>) {
            if n.leaf() {
                out.push((path, n.value.clone()));
                return;
            }
            for k in n.sorted_keys() {
                if let Some(c) = n.child(&k) {
                    let mut p = path.clone();
                    p.push(k);
                    collect(c, p, out);
                }
            }
        }
        let mut out = Vec::new();
        for k in self.sorted_keys() {
            if let Some(c) = self.child(&k) {
                collect(c, vec![k], &mut out);
            }
        }
        out
    }

    /// getRefRecExtent: a leaf's type, else an object of the children's.
    fn extent(&self) -> Type {
        if self.leaf() {
            return self.value.clone();
        }
        let mut fixed = Vec::new();
        for (k, c) in &self.children {
            if matches!(
                k.value,
                TermValue::String(_) | TermValue::Number(_) | TermValue::Bool(_)
            ) && let Some(key) = to_key(k)
            {
                fixed.push((key, c.extent()));
            }
        }
        Type::object(fixed, Some((Type::String, A)))
    }
}

fn dyn_parts(d: &Option<(Box<Type>, Box<Type>)>) -> (Type, Type) {
    match d {
        Some((k, v)) => ((**k).clone(), (**v).clone()),
        None => (Type::Nil, Type::Nil),
    }
}

/// mergeTypes.
fn merge_types(a: &Type, b: &Type) -> Type {
    if matches!(a, Type::Nil) {
        return b.clone();
    }
    if matches!(b, Type::Nil) {
        return a.clone();
    }
    match a {
        Type::Object {
            fixed: fa,
            dynamic: da,
        } => {
            if let Type::Object {
                fixed: fb,
                dynamic: db,
            } = b
                && fa.is_empty()
                && fb.is_empty()
            {
                let ((ak, av), (bk, bv)) = (dyn_parts(da), dyn_parts(db));
                return Type::object(Vec::new(), Some((types::or(&ak, &bk), merge_types(&av, &bv))));
            } else if let Type::Any(of) = b
                && fa.is_empty()
            {
                // OPA updates the first such object of b in place.
                for (i, t) in of.iter().enumerate() {
                    if let Type::Object {
                        fixed: ft,
                        dynamic: dt,
                    } = t
                        && ft.is_empty()
                    {
                        let ((tk, tv), (ak, av)) = (dyn_parts(dt), dyn_parts(da));
                        let mut out = of.clone();
                        if let Some(slot) = out.get_mut(i) {
                            *slot = Type::Object {
                                fixed: Vec::new(),
                                dynamic: Some((Box::new(types::or(&tk, &ak)), Box::new(types::or(&tv, &av)))),
                            };
                        }
                        return Type::Any(out);
                    }
                }
            }
        }
        Type::Set(x) => {
            if let Type::Set(y) = b {
                let of = |s: &Option<Box<Type>>| s.as_deref().cloned().unwrap_or(Type::Nil);
                return Type::set(types::or(&of(x), &of(y)));
            }
        }
        Type::Any(_) if !matches!(b, Type::Any(_)) => return merge_types(b, a),
        _ => {}
    }
    types::or(a, b)
}

/// insertIntoObject.
fn insert_into_object(o: &Type, path: &[Term], tpe: &Type) -> Type {
    let Some((first, rest)) = path.split_first() else {
        return o.clone();
    };
    let Type::Object { fixed, dynamic } = o else {
        return o.clone();
    };
    let key = ground_type(first);
    let value = if rest.is_empty() {
        tpe.clone()
    } else {
        insert_into_object(&Type::object(Vec::new(), None), rest, tpe)
    };
    let d = match dynamic {
        Some((k, v)) => (types::or(k, &key), types::or(v, &value)),
        None => (key, value),
    };
    Type::object(fixed.clone(), Some(d))
}

/// The type of a ground term, as GetByValue gives it.
fn ground_type(t: &Term) -> Type {
    match &t.value {
        TermValue::Null => Type::Null,
        TermValue::Bool(_) => Type::Boolean,
        TermValue::Number(_) => Type::Number,
        TermValue::String(_) | TermValue::TemplateString { .. } => Type::String,
        TermValue::Array(a) => {
            let fixed: Vec<Type> = a.iter().map(ground_type).collect();
            let d = if fixed.is_empty() { A } else { Type::Nil };
            Type::array(fixed, d)
        }
        TermValue::Object(o) => {
            let mut fixed = Vec::new();
            let mut dynamic = None;
            for (k, v) in sorted_pairs(o) {
                match to_key(k).filter(|_| is_constant(k)) {
                    Some(key) => fixed.push((key, ground_type(v))),
                    None => dynamic = Some((ground_type(k), ground_type(v))),
                }
            }
            if fixed.is_empty() && dynamic.is_none() {
                dynamic = Some((A, A));
            }
            Type::object(fixed, dynamic)
        }
        TermValue::Set(s) => {
            let mut tpe = Type::Nil;
            for x in sorted_items(s) {
                tpe = types::or(&tpe, &ground_type(x));
            }
            Type::set(if tpe.is_nil() { A } else { tpe })
        }
        _ => Type::Nil,
    }
}

/// IsConstant: no variables, refs, calls or comprehensions anywhere.
pub fn is_constant(t: &Term) -> bool {
    match &t.value {
        TermValue::Null | TermValue::Bool(_) | TermValue::Number(_) | TermValue::String(_) => true,
        TermValue::Array(a) | TermValue::Set(a) => a.iter().all(is_constant),
        TermValue::Object(o) => o.iter().all(|(k, v)| is_constant(k) && is_constant(v)),
        TermValue::TemplateString { parts, .. } => parts.iter().all(|p| match p {
            TemplatePart::Term(t) => is_constant(t),
            TemplatePart::Expr(e) => match &e.terms {
                ExprTerms::Term(t) => is_constant(t),
                _ => false,
            },
        }),
        _ => false,
    }
}

/// ast.JSON: a constant term as the Go value OPA makes of it.
pub fn to_key(t: &Term) -> Option<Key> {
    Some(match &t.value {
        TermValue::Null => Key::Null,
        TermValue::Bool(b) => Key::Bool(*b),
        TermValue::Number(n) => Key::Number(n.text().to_string()),
        TermValue::String(s) => Key::String(s.to_string()),
        TermValue::Array(a) => Key::Array(a.iter().map(to_key).collect::<Option<_>>()?),
        TermValue::Set(s) => Key::Array(sorted_items(s).into_iter().map(to_key).collect::<Option<_>>()?),
        TermValue::Object(o) => {
            let mut m = Vec::new();
            for (k, v) in sorted_pairs(o) {
                let name = match &k.value {
                    TermValue::String(s) => s.to_string(),
                    TermValue::Number(n) => n.text().to_string(),
                    TermValue::Bool(b) => b.to_string(),
                    TermValue::Null => "null".to_string(),
                    _ => return None,
                };
                m.push((name, to_key(v)?));
            }
            m.sort_by(|a, b| a.0.cmp(&b.0));
            Key::Object(m)
        }
        _ => return None,
    })
}

/// InterfaceToValue of a key.
fn key_term(k: &Key) -> Term {
    let v = match k {
        Key::Null => TermValue::Null,
        Key::Bool(b) => TermValue::Bool(*b),
        Key::Number(n) => TermValue::Number(crate::value::Number(n.as_str().into())),
        Key::String(s) => TermValue::String(s.as_str().into()),
        Key::Array(a) => TermValue::Array(a.iter().map(key_term).collect()),
        Key::Object(o) => TermValue::Object(
            o.iter()
                .map(|(k, v)| (Term::string(k, None), key_term(v)))
                .collect(),
        ),
    };
    Term::new(v, None)
}

/// RootDocumentNames.Contains.
fn is_root_doc(t: Option<&Term>) -> bool {
    matches!(t.and_then(Term::as_var), Some("data" | "input"))
}

/// The ref of a builtin's name (Builtin.Ref).
pub fn builtin_ref(name: &str) -> Vec<Term> {
    let mut parts = name.split('.');
    let mut out = Vec::new();
    if let Some(first) = parts.next() {
        out.push(Term::var(first, None));
    }
    out.extend(parts.map(|p| Term::string(p, None)));
    out
}

/// TypeEnv: the types known, in frames that each wrap the one below.
#[derive(Debug, Clone)]
pub struct TypeEnv {
    below: Vec<Rc<Node>>,
    top: Rc<Node>,
}

impl Default for TypeEnv {
    fn default() -> TypeEnv {
        TypeEnv {
            below: Vec::new(),
            top: Rc::new(Node::default()),
        }
    }
}

impl TypeEnv {
    /// typeChecker.Env: a frame of the builtins' declarations.
    pub fn with_builtins<'a>(decls: impl Iterator<Item = (&'a str, &'a Type)>) -> TypeEnv {
        let mut env = TypeEnv::default();
        for (name, decl) in decls {
            env.top_mut().put(&builtin_ref(name), decl.clone());
        }
        env
    }

    /// wrap.
    pub fn wrap(&mut self) {
        let old = std::mem::replace(&mut self.top, Rc::new(Node::default()));
        self.below.push(old);
    }

    /// env.next.
    pub fn pop(&mut self) {
        if let Some(n) = self.below.pop() {
            self.top = n;
        }
    }

    fn top_level(&self) -> usize {
        self.below.len()
    }

    fn top_mut(&mut self) -> &mut Node {
        Rc::make_mut(&mut self.top)
    }

    fn frame(&self, level: usize) -> Option<&Node> {
        if level == self.below.len() {
            return Some(&self.top);
        }
        self.below.get(level).map(|n| &**n)
    }

    fn node_at(&self, level: usize, path: &[Term]) -> Option<&Node> {
        self.frame(level)?.at(path)
    }

    /// GetByValue of a variable, from the given frame down.
    fn get_var(&self, level: usize, v: &Term) -> Type {
        if let Some(n) = self.frame(level).and_then(|f| f.child(v)) {
            return n.value.clone();
        }
        match level.checked_sub(1) {
            Some(l) => self.get_var(l, v),
            None => Type::Nil,
        }
    }

    /// GetByRef, from the given frame down.
    fn get_ref(&self, level: usize, r: &[Term]) -> Type {
        let Some(head) = r.first() else { return Type::Nil };
        match self.frame(level).and_then(|f| f.child(head)) {
            None => self.ref_fallback(level, r),
            Some(node) => self.ref_rec(level, node, r, r.get(1..).unwrap_or_default()),
        }
    }

    /// getRefFallback.
    fn ref_fallback(&self, level: usize, r: &[Term]) -> Type {
        if let Some(l) = level.checked_sub(1) {
            return self.get_ref(l, r);
        }
        if is_root_doc(r.first()) {
            return A;
        }
        Type::Nil
    }

    /// getRefRec.
    fn ref_rec(&self, level: usize, node: &Node, r: &[Term], tail: &[Term]) -> Type {
        let Some((first, rest)) = tail.split_first() else {
            return node.extent();
        };
        if node.leaf() {
            if let Some(c) = node.child(first) {
                return self.ref_rec(level, c, r, rest);
            }
            return select_ref(&node.value, tail);
        }
        if !is_constant(first) {
            return select_ref(&node.extent(), tail);
        }
        match node.child(first) {
            None => self.ref_fallback(level, r),
            Some(c) => self.ref_rec(level, c, r, rest),
        }
    }

    /// GetByRef from the top frame.
    pub fn get_by_ref(&self, r: &[Term]) -> Type {
        self.get_ref(self.top_level(), r)
    }
}

/// selectConstant.
fn select_constant(tpe: &Type, t: &Term) -> Type {
    match to_key(t) {
        Some(k) => types::select(tpe, &k),
        None => Type::Nil,
    }
}

/// selectRef.
fn select_ref(tpe: &Type, r: &[Term]) -> Type {
    let Some((head, tail)) = r.split_first() else {
        return tpe.clone();
    };
    if tpe.is_nil() {
        return Type::Nil;
    }
    match head.value {
        TermValue::Var(_)
        | TermValue::Ref(_)
        | TermValue::Array(_)
        | TermValue::Object(_)
        | TermValue::Set(_) => select_ref(&types::values(tpe), tail),
        _ => select_ref(&select_constant(tpe, head), tail),
    }
}

/// GetByValue: the type of a term in the env's top frame.
pub fn get_value(env: &mut TypeEnv, t: &Term) -> Type {
    match &t.value {
        TermValue::Null => Type::Null,
        TermValue::Bool(_) => Type::Boolean,
        TermValue::Number(_) => Type::Number,
        TermValue::String(_) | TermValue::TemplateString { .. } => Type::String,
        TermValue::Array(a) => {
            let fixed: Vec<Type> = a.iter().map(|x| get_value(env, x)).collect();
            let d = if fixed.is_empty() { A } else { Type::Nil };
            Type::array(fixed, d)
        }
        TermValue::Object(o) => {
            let mut fixed = Vec::new();
            let mut dynamic = None;
            for (k, v) in sorted_pairs(o) {
                match to_key(k).filter(|_| is_constant(k)) {
                    Some(key) => {
                        let vt = get_value(env, v);
                        fixed.push((key, vt));
                    }
                    None => {
                        let kt = get_value(env, k);
                        dynamic = Some((kt, get_value(env, v)));
                    }
                }
            }
            if fixed.is_empty() && dynamic.is_none() {
                dynamic = Some((A, A));
            }
            Type::object(fixed, dynamic)
        }
        TermValue::Set(s) => {
            let mut tpe = Type::Nil;
            for x in sorted_items(s) {
                let xt = get_value(env, x);
                tpe = types::or(&tpe, &xt);
            }
            Type::set(if tpe.is_nil() { A } else { tpe })
        }
        TermValue::ArrayCompr(term, body) => {
            let ok = Checker::new(None).check_body(env, body).is_empty();
            let out = if ok {
                Type::array(Vec::new(), get_value(env, term))
            } else {
                Type::Nil
            };
            env.pop();
            out
        }
        TermValue::ObjectCompr(k, v, body) => {
            let ok = Checker::new(None).check_body(env, body).is_empty();
            let out = if ok {
                let kt = get_value(env, k);
                Type::object(Vec::new(), Some((kt, get_value(env, v))))
            } else {
                Type::Nil
            };
            env.pop();
            out
        }
        TermValue::SetCompr(term, body) => {
            let ok = Checker::new(None).check_body(env, body).is_empty();
            let out = if ok {
                Type::set(get_value(env, term))
            } else {
                Type::Nil
            };
            env.pop();
            out
        }
        TermValue::Ref(r) => env.get_by_ref(r),
        TermValue::Var(_) => env.get_var(env.top_level(), t),
        TermValue::Call(_) => Type::Nil,
    }
}

fn type_err(loc: Option<Location>, msg: String) -> CompileError {
    CompileError::new(TYPE_ERR, loc, msg)
}

fn format_args(args: &[Type]) -> String {
    let buf: Vec<String> = args.iter().map(ToString::to_string).collect();
    format!("({})", buf.join(", "))
}

/// newArgError, and whether a type it has is not known (ArgErrDetail.nilType).
fn arg_error(
    loc: Option<Location>,
    name: &[Term],
    msg: &str,
    have: &[Type],
    want: &FuncArgs,
) -> (CompileError, bool) {
    let mut e = type_err(loc, format!("{}: {msg}", text_of_ref(name)));
    e.lines = vec![format!("have: {}", format_args(have)), format!("want: {want}")];
    (e, have.iter().any(types::has_nil))
}

/// arityMismatchError's richer form, for a declared function.
pub fn arity_error(env: &TypeEnv, f: &[Term], expr: &Expr) -> Option<CompileError> {
    let want = env.get_by_ref(f);
    if !matches!(want, Type::Function { .. }) {
        return None;
    }
    let mut env = env.clone();
    let ops = match &expr.terms {
        ExprTerms::Call(c) => c.get(1..).unwrap_or_default(),
        _ => &[],
    };
    let have: Vec<Type> = ops.iter().map(|o| get_value(&mut env, o)).collect();
    Some(
        arg_error(
            expr.loc.clone(),
            f,
            "arity mismatch",
            &have,
            &want.named_func_args(),
        )
        .0,
    )
}

/// newRefErrInvalid.
fn ref_err_invalid(
    loc: Option<Location>,
    r: &[Term],
    pos: usize,
    have: &Type,
    want: &Type,
    one_of: &[Term],
) -> CompileError {
    let text = text_of_ref(r);
    let mut e = type_err(loc, format!("undefined ref: {text}"));
    let offset = text_of_ref(r.get(..pos).unwrap_or_default()).len() + 1;
    let pad = " ".repeat(offset);
    let mut lines = vec![text, format!("{pad}^")];
    if have.is_nil() {
        let at = r.get(pos).map(ToString::to_string).unwrap_or_default();
        lines.push(format!("{pad}have: {at}"));
    } else {
        lines.push(format!("{pad}have (type): {have}"));
    }
    if one_of.is_empty() {
        lines.push(format!("{pad}want (type): {}", types::go_value(want)));
    } else {
        let vals: Vec<String> = one_of.iter().map(ToString::to_string).collect();
        lines.push(format!("{pad}want (one of): [{}]", vals.join(" ")));
    }
    e.lines = lines;
    e
}

/// Whether a term's text holds a generated name (`__local[0-9]+__`).
fn has_local_name(s: &str) -> bool {
    let mut rest = s;
    while let Some(i) = rest.find("__local") {
        let after = rest.get(i + 7..).unwrap_or_default();
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && after.get(digits..).is_some_and(|x| x.starts_with("__")) {
            return true;
        }
        rest = after;
    }
    false
}

/// newRefErrUnsupported.
fn ref_err_unsupported(loc: Option<Location>, r: &[Term], pos: isize, have: &Type) -> CompileError {
    let text = text_of_ref(r);
    let mut e = if matches!(have, Type::Function { .. }) {
        let function = match r.split_last() {
            Some((last, init)) if has_local_name(&last.to_string()) => text_of_ref(init),
            _ => text.clone(),
        };
        type_err(loc, format!("function {function} used as reference, not called"))
    } else {
        type_err(loc, format!("undefined ref: {text}"))
    };
    let upto = usize::try_from(pos.saturating_add(1)).unwrap_or(0);
    let carets = text_of_ref(r.get(..upto.min(r.len())).unwrap_or_default()).len();
    e.lines = vec![
        text,
        "^".repeat(carets),
        format!("have: {}", types::go_value(have)),
    ];
    e
}

/// getOneOfForType: the keys of an object type, or of the objects in a union.
fn one_of_type(tpe: &Type) -> Vec<Term> {
    let mut out: Vec<Term> = Vec::new();
    match tpe {
        Type::Object { fixed, .. } => out.extend(fixed.iter().map(|(k, _)| key_term(k))),
        Type::Any(of) => {
            for t in of {
                out.extend(one_of_type(t));
            }
        }
        _ => {}
    }
    let mut uniq: Vec<Term> = Vec::new();
    for t in out {
        if !uniq.iter().any(|u| same(u, &t)) {
            uniq.push(t);
        }
    }
    uniq.sort_by(term_compare);
    uniq
}

/// unifies.
pub fn unifies(a: &Type, b: &Type) -> bool {
    let (a, b) = (a.unwrap(), b.unwrap());
    if a.is_nil() || b.is_nil() {
        return false;
    }
    let any_a = if let Type::Any(x) = a { Some(x) } else { None };
    let any_b = if let Type::Any(y) = b { Some(y) } else { None };
    if let Some(x) = any_a
        && unifies_any(x, b)
    {
        return true;
    }
    if let Some(y) = any_b
        && unifies_any(y, a)
    {
        return true;
    }
    if any_a.is_some() || any_b.is_some() {
        return false;
    }
    match (a, b) {
        (Type::Null, Type::Null)
        | (Type::Boolean, Type::Boolean)
        | (Type::Number, Type::Number)
        | (Type::String, Type::String) => true,
        (Type::Array { .. }, Type::Array { .. }) => unifies_arrays(a, b),
        (Type::Object { .. }, Type::Object { .. }) => unifies_objects(a, b),
        (Type::Set(_), Type::Set(_)) => unifies(&types::values(a), &types::values(b)),
        (Type::Function { .. }, Type::Function { .. }) => {
            if a.arity() != b.arity() {
                return false;
            }
            let (fa, fb) = (a.func_args(), b.func_args());
            fa.args.iter().enumerate().all(|(i, x)| unifies(x, fb.arg(i)))
        }
        _ => false,
    }
}

/// unifiesAny.
fn unifies_any(a: &[Type], b: &Type) -> bool {
    if matches!(b, Type::Function { .. }) {
        return false;
    }
    a.iter().any(|x| unifies(x, b)) || a.is_empty()
}

fn array_parts(t: &Type) -> (usize, Type) {
    match t {
        Type::Array { fixed, dynamic } => (fixed.len(), dynamic.as_deref().cloned().unwrap_or(Type::Nil)),
        _ => (0, Type::Nil),
    }
}

/// unifiesArrays.
fn unifies_arrays(a: &Type, b: &Type) -> bool {
    let statics = |x: &Type, y: &Type| {
        (0..array_parts(x).0).all(|i| unifies(&types::array_elem(x, i), &types::array_elem(y, i)))
    };
    if !statics(a, b) || !statics(b, a) {
        return false;
    }
    let (da, db) = (array_parts(a).1, array_parts(b).1);
    da.is_nil() || db.is_nil() || unifies(&da, &db)
}

/// unifiesObjects.
fn unifies_objects(a: &Type, b: &Type) -> bool {
    let statics = |x: &Type, y: &Type| match x {
        Type::Object { fixed, .. } => fixed
            .iter()
            .all(|(k, _)| unifies(&types::object_select(x, k), &types::object_select(y, k))),
        _ => true,
    };
    if !statics(a, b) || !statics(b, a) {
        return false;
    }
    let dv = |x: &Type| match x {
        Type::Object {
            dynamic: Some((_, v)),
            ..
        } => (**v).clone(),
        _ => Type::Nil,
    };
    let (da, db) = (dv(a), dv(b));
    da.is_nil() || db.is_nil() || unifies(&da, &db)
}

/// unify1: whether a term can have the type, binding its variables' types as it goes.
fn unify1(env: &mut TypeEnv, term: &Term, tpe: &Type, union: bool) -> bool {
    let tpe = tpe.unwrap();
    match &term.value {
        TermValue::Array(items) => match tpe {
            Type::Array { fixed, dynamic } => {
                if items.len() != fixed.len() && dynamic.is_none() {
                    return false;
                }
                items
                    .iter()
                    .enumerate()
                    .all(|(i, x)| unify1(env, x, &types::array_elem(tpe, i), union))
            }
            Type::Any(of) => unify1_any(env, term, of, &mut |env| {
                for x in items {
                    unify1(env, x, &A, true);
                }
            }),
            _ => false,
        },
        TermValue::Object(pairs) => match tpe {
            Type::Object { fixed, dynamic } => {
                if pairs.len() != fixed.len() && dynamic.is_none() {
                    return false;
                }
                for (k, v) in sorted_pairs(pairs) {
                    if is_constant(k) {
                        let child = select_constant(tpe, k);
                        if child.is_nil() || !unify1(env, v, &child, union) {
                            return false;
                        }
                    } else {
                        unify1(env, v, &A, union);
                    }
                }
                true
            }
            Type::Any(of) => unify1_any(env, term, of, &mut |env| {
                for (k, v) in sorted_pairs(pairs) {
                    unify1(env, k, &A, true);
                    unify1(env, v, &A, true);
                }
            }),
            _ => false,
        },
        TermValue::Set(items) => match tpe {
            Type::Set(_) => {
                let of = types::values(tpe);
                sorted_items(items)
                    .into_iter()
                    .all(|x| unify1(env, x, &of, union))
            }
            Type::Any(of) => unify1_any(env, term, of, &mut |env| {
                for x in sorted_items(items) {
                    unify1(env, x, &A, true);
                }
            }),
            _ => false,
        },
        TermValue::Ref(_)
        | TermValue::ArrayCompr(..)
        | TermValue::ObjectCompr(..)
        | TermValue::SetCompr(..) => {
            let t = get_value(env, term);
            unifies(&t, tpe)
        }
        TermValue::Var(_) => {
            if !union {
                let exist = get_value(env, term);
                if !exist.is_nil() {
                    return unifies(&exist, tpe);
                }
                env.top_mut().put_one(term, tpe.clone());
            } else {
                let exist = get_value(env, term);
                env.top_mut().put_one(term, types::or(&exist, tpe));
            }
            true
        }
        // OPA reaches only constants here (a call would be its panic).
        _ => {
            let t = get_value(env, term);
            unifies(&t, tpe)
        }
    }
}

/// unify1 of a composite against a union: every member of A, else any member.
fn unify1_any(env: &mut TypeEnv, term: &Term, of: &[Type], all: &mut dyn FnMut(&mut TypeEnv)) -> bool {
    if of.is_empty() {
        all(env);
        return true;
    }
    let mut ok = false;
    for t in of {
        ok = unify1(env, term, t, true) || ok;
    }
    ok
}

/// unify2.
fn unify2(env: &mut TypeEnv, a: &Term, ta: &Type, b: &Term, tb: &Type) -> bool {
    let (nil_a, nil_b) = (types::has_nil(ta), types::has_nil(tb));
    if nil_a && !nil_b {
        return unify1(env, a, tb, false);
    } else if nil_b && !nil_a {
        return unify1(env, b, ta, false);
    } else if !nil_a && !nil_b {
        return unifies(ta, tb);
    }
    match (&a.value, &b.value) {
        (TermValue::Array(_), _) => unify2_array(env, a, b),
        (TermValue::Object(_), _) => unify2_object(env, a, b),
        (TermValue::Var(_), TermValue::Var(_)) => unify_vars(env, a, b),
        (TermValue::Var(_), TermValue::Array(_)) => unify2_array(env, b, a),
        (TermValue::Var(_), TermValue::Object(_)) => unify2_object(env, b, a),
        _ => false,
    }
}

fn unify_vars(env: &mut TypeEnv, a: &Term, b: &Term) -> bool {
    if !unify1(env, a, &A, false) {
        return false;
    }
    let ta = get_value(env, a);
    unify1(env, b, &ta, false)
}

/// unify2Array.
fn unify2_array(env: &mut TypeEnv, a: &Term, b: &Term) -> bool {
    let TermValue::Array(arr) = &a.value else {
        return false;
    };
    match &b.value {
        TermValue::Array(bv) => {
            if arr.len() != bv.len() {
                return false;
            }
            for (x, y) in arr.iter().zip(bv) {
                let (tx, ty) = (get_value(env, x), get_value(env, y));
                if !unify2(env, x, &tx, y, &ty) {
                    return false;
                }
            }
            true
        }
        TermValue::Var(_) => unify_vars(env, a, b),
        _ => false,
    }
}

/// unify2Object.
fn unify2_object(env: &mut TypeEnv, a: &Term, b: &Term) -> bool {
    let TermValue::Object(obj) = &a.value else {
        return false;
    };
    match &b.value {
        TermValue::Object(bv) => {
            // Intersect: a's pairs, in key order, whose key b has.
            let mut cv: Vec<(&Term, &Term)> = Vec::new();
            for (k, v) in sorted_pairs(obj) {
                if let Some((_, v2)) = bv.iter().find(|(k2, _)| same(k, k2)) {
                    cv.push((v, v2));
                }
            }
            if obj.len() != bv.len() || bv.len() != cv.len() {
                return false;
            }
            for (x, y) in cv {
                let (tx, ty) = (get_value(env, x), get_value(env, y));
                if !unify2(env, x, &tx, y, &ty) {
                    return false;
                }
            }
            true
        }
        TermValue::Var(_) => unify_vars(env, a, b),
        _ => false,
    }
}

/// typeChecker: the type errors of rules and bodies, with the rewriter that writes the
/// variables of refs in errors as they were written.
#[derive(Debug)]
pub struct Checker<'a> {
    rewritten: Option<&'a HashMap<Var, Var>>,
    errs: Vec<CompileError>,
}

impl<'a> Checker<'a> {
    pub fn new(rewritten: Option<&'a HashMap<Var, Var>>) -> Checker<'a> {
        Checker {
            rewritten,
            errs: Vec::new(),
        }
    }

    fn copy(&self) -> Checker<'a> {
        Checker::new(self.rewritten)
    }

    /// CheckTypes: each rule in turn, in dependency order, by its ref; the errors sorted
    /// by location, then by text.
    pub fn check_types(
        &mut self,
        env: &mut TypeEnv,
        rules: &[(Vec<Term>, &crate::ast::Rule)],
    ) -> Vec<CompileError> {
        for (path, rule) in rules {
            self.check_rule(env, path.clone(), rule);
        }
        let mut errs = std::mem::take(&mut self.errs);
        errs.sort_by(|a, b| {
            crate::compile::safety::compare_loc(&a.loc, &b.loc)
                .cmp(&0)
                .then_with(|| a.to_string().cmp(&b.to_string()))
        });
        errs
    }

    /// checkRule: the rule's type, inserted at its ref; any for a rule with errors.
    fn check_rule(&mut self, env: &mut TypeEnv, mut path: Vec<Term>, rule: &crate::ast::Rule) {
        env.wrap();
        let errs = self.check_body(env, &rule.body);
        if !errs.is_empty() {
            env.pop();
            env.pop();
            env.top_mut().put(&path, A);
            return;
        }
        let tpe = if !rule.head.args.is_empty() {
            for arg in &rule.head.args {
                let mut vars = Vec::new();
                walk_terms(arg, &mut |t: &Term| {
                    if matches!(t.value, TermValue::Var(_)) {
                        vars.push(t.clone());
                    }
                    false
                });
                for v in vars {
                    if get_value(env, &v).is_nil() {
                        env.top_mut().put_one(&v, A);
                    }
                }
            }
            let args: Vec<Type> = rule.head.args.iter().map(|a| get_value(env, a)).collect();
            let result = rule
                .head
                .value
                .as_ref()
                .map(|v| get_value(env, v))
                .unwrap_or(Type::Nil);
            Type::function(args, result)
        } else {
            match rule.head.kind() {
                RuleKind::SingleValue => {
                    let tv = rule
                        .head
                        .value
                        .as_ref()
                        .map(|v| get_value(env, v))
                        .unwrap_or(Type::Nil);
                    if !path.iter().skip(1).all(Term::is_ground) {
                        // Ref.Dynamic: from the first part past the head that is not constant.
                        let at = path
                            .iter()
                            .skip(1)
                            .position(|t| !is_constant(t))
                            .map_or(path.len(), |i| i + 1);
                        let suffix = path.get(at..).unwrap_or_default().to_vec();
                        path = ground_prefix(&path);
                        nested_object(env, &suffix, tv)
                    } else {
                        tv
                    }
                }
                RuleKind::MultiValue => {
                    let tk = rule
                        .head
                        .key
                        .as_ref()
                        .map(|k| get_value(env, k))
                        .unwrap_or(Type::Nil);
                    if tk.is_nil() { Type::Nil } else { Type::set(tk) }
                }
            }
        };
        env.pop();
        env.pop();
        if !tpe.is_nil() {
            env.top_mut().insert(&path, tpe);
        }
    }

    /// CheckBody: each expression's closures, refs and call; the body's frame is left
    /// on the env for the caller to read and pop.
    pub fn check_body(&mut self, env: &mut TypeEnv, body: &[Expr]) -> Vec<CompileError> {
        env.wrap();
        let mut errors = Vec::new();
        for expr in body {
            let closure = self.check_closures(env, expr);
            let has_closure = !closure.is_empty();
            errors.extend(closure);
            let mut rc = RefChecker {
                rewritten: self.rewritten,
                errs: Vec::new(),
            };
            rc.visit_expr(env, expr);
            let has_ref = !rc.errs.is_empty();
            errors.extend(rc.errs);
            if let Some((err, nil)) = self.check_expr(env, expr) {
                // A more actionable error explains an unknown type.
                if !((has_closure || has_ref) && nil) {
                    errors.push(err);
                }
            }
        }
        self.errs.extend(errors.iter().cloned());
        errors
    }

    /// checkClosures: the errors of the last comprehension that has any.
    fn check_closures(&mut self, env: &mut TypeEnv, expr: &Expr) -> Vec<CompileError> {
        let mut result = Vec::new();
        self.closures_expr(env, expr, &mut result);
        result
    }

    fn closures_expr(&mut self, env: &mut TypeEnv, e: &Expr, out: &mut Vec<CompileError>) {
        match &e.terms {
            ExprTerms::Term(t) => self.closures_term(env, t, out),
            ExprTerms::Call(c) => c.iter().for_each(|t| self.closures_term(env, t, out)),
            ExprTerms::Some(d) => d.symbols.iter().for_each(|t| self.closures_term(env, t, out)),
            ExprTerms::Every(ev) => {
                if let Some(k) = &ev.key {
                    self.closures_term(env, k, out);
                }
                self.closures_term(env, &ev.value, out);
                self.closures_term(env, &ev.domain, out);
                ev.body.iter().for_each(|x| self.closures_expr(env, x, out));
            }
        }
        for w in &e.with {
            self.closures_term(env, &w.target, out);
            self.closures_term(env, &w.value, out);
        }
    }

    fn closures_term(&mut self, env: &mut TypeEnv, t: &Term, out: &mut Vec<CompileError>) {
        match &t.value {
            TermValue::ArrayCompr(x, body) | TermValue::SetCompr(x, body) => {
                if self.closure(env, body, out) {
                    self.closures_term(env, x, out);
                    body.iter().for_each(|e| self.closures_expr(env, e, out));
                }
            }
            TermValue::ObjectCompr(k, v, body) => {
                if self.closure(env, body, out) {
                    self.closures_term(env, k, out);
                    self.closures_term(env, v, out);
                    body.iter().for_each(|e| self.closures_expr(env, e, out));
                }
            }
            TermValue::Ref(r) | TermValue::Array(r) | TermValue::Call(r) => {
                r.iter().for_each(|x| self.closures_term(env, x, out));
            }
            TermValue::Object(o) => {
                for (k, v) in sorted_pairs(o) {
                    self.closures_term(env, k, out);
                    self.closures_term(env, v, out);
                }
            }
            TermValue::Set(s) => sorted_items(s)
                .into_iter()
                .for_each(|x| self.closures_term(env, x, out)),
            TermValue::TemplateString { parts, .. } => {
                for p in parts {
                    match p {
                        TemplatePart::Term(x) => self.closures_term(env, x, out),
                        TemplatePart::Expr(e) => self.closures_expr(env, e, out),
                    }
                }
            }
            _ => {}
        }
    }

    /// A comprehension's body checked; true to walk on into it (it has no errors).
    fn closure(&mut self, env: &mut TypeEnv, body: &[Expr], out: &mut Vec<CompileError>) -> bool {
        let errs = self.copy().check_body(env, body);
        env.pop();
        if errs.is_empty() {
            return true;
        }
        *out = errs;
        false
    }

    /// checkExpr: an error, and whether an unknown type caused it.
    fn check_expr(&mut self, env: &mut TypeEnv, expr: &Expr) -> Option<(CompileError, bool)> {
        if let Some(e) = check_expr_with(env, expr) {
            return Some(e);
        }
        let ExprTerms::Call(terms) = &expr.terms else {
            return None;
        };
        let op = terms.first().and_then(Term::as_ref)?;
        let args = terms.get(1..).unwrap_or_default();
        if text_of_ref(op) == "eq" {
            return check_expr_eq(env, expr, op, args);
        }
        check_expr_builtin(env, expr, op, args)
    }
}

/// checkExprWith: a function replaced by one it does not unify with.
fn check_expr_with(env: &mut TypeEnv, expr: &Expr) -> Option<(CompileError, bool)> {
    for w in &expr.with {
        let tt = get_value(env, &w.target);
        let vt = get_value(env, &w.value);
        if matches!(tt, Type::Function { .. }) && matches!(vt, Type::Function { .. }) && !unifies(&tt, &vt) {
            let target = w.target.as_ref().unwrap_or_default();
            return Some(arg_error(
                w.loc.clone(),
                target,
                "arity mismatch",
                &vt.func_args().args,
                &tt.named_func_args(),
            ));
        }
    }
    None
}

/// checkExprEq.
fn check_expr_eq(env: &mut TypeEnv, expr: &Expr, op: &[Term], args: &[Term]) -> Option<(CompileError, bool)> {
    let pre: Vec<Type> = args.iter().map(|a| get_value(env, a)).collect();
    let want = FuncArgs {
        args: vec![A, A],
        variadic: Type::Nil,
    };
    if pre.len() < 2 {
        return Some(arg_error(expr.loc.clone(), op, "too few arguments", &pre, &want));
    }
    if pre.len() > 2 {
        return Some(arg_error(expr.loc.clone(), op, "too many arguments", &pre, &want));
    }
    let (Some(a), Some(b)) = (args.first(), args.get(1)) else {
        return None;
    };
    let (ta, tb) = (get_value(env, a), get_value(env, b));
    if unify2(env, a, &ta, b, &tb) {
        return None;
    }
    let mut e = type_err(expr.loc.clone(), "match error".into());
    e.lines = vec![format!("left  : {ta}"), format!("right : {tb}")];
    Some((e, types::has_nil(&ta) || types::has_nil(&tb)))
}

/// checkExprBuiltin: a call's arguments against its declaration, the result last.
fn check_expr_builtin(
    env: &mut TypeEnv,
    expr: &Expr,
    name: &[Term],
    args: &[Term],
) -> Option<(CompileError, bool)> {
    let tpe = env.get_by_ref(name);
    let undefined = || {
        Some((
            type_err(
                expr.loc.clone(),
                format!("undefined function {}", text_of_ref(name)),
            ),
            false,
        ))
    };
    match &tpe {
        Type::Nil => return undefined(),
        Type::Any(of) if of.is_empty() => return None,
        Type::Function { .. } => {}
        _ => return undefined(),
    }
    let mut fargs = tpe.func_args();
    let mut named = tpe.named_func_args();
    let result = tpe.result();
    if !result.is_nil() {
        fargs.args.push(result);
        named.args.push(tpe.named_result());
    }
    if args.len() > fargs.args.len() && fargs.variadic.is_nil() {
        let have: Vec<Type> = args.iter().map(|a| get_value(env, a)).collect();
        return Some(arg_error(
            expr.loc.clone(),
            name,
            "too many arguments",
            &have,
            &named,
        ));
    }
    if args.len() < tpe.func_args().args.len() {
        let have: Vec<Type> = args.iter().map(|a| get_value(env, a)).collect();
        return Some(arg_error(
            expr.loc.clone(),
            name,
            "too few arguments",
            &have,
            &named,
        ));
    }
    for (i, a) in args.iter().enumerate() {
        let want = fargs.arg(i).clone();
        if !unify1(env, a, &want, false) {
            let post: Vec<Type> = args.iter().map(|a| get_value(env, a)).collect();
            return Some(arg_error(
                expr.loc.clone(),
                name,
                "invalid argument(s)",
                &post,
                &named,
            ));
        }
    }
    None
}

/// nestedObject: objects nested by the dynamic parts of a rule's ref.
fn nested_object(env: &mut TypeEnv, path: &[Term], tpe: Type) -> Type {
    let Some((k, rest)) = path.split_first() else {
        return tpe;
    };
    let tv = nested_object(env, rest, tpe);
    if tv.is_nil() {
        return Type::Nil;
    }
    let tk = get_value(env, k);
    if tk.is_nil() {
        return Type::Nil;
    }
    Type::object(Vec::new(), Some((tk, tv)))
}

/// refChecker: each ref of an expression against the types known.
#[derive(Debug)]
struct RefChecker<'a> {
    rewritten: Option<&'a HashMap<Var, Var>>,
    errs: Vec<CompileError>,
}

impl RefChecker<'_> {
    fn rewrite(&self, r: &[Term]) -> Vec<Term> {
        match self.rewritten {
            Some(m) => crate::compile::rewrite_ref(m, r),
            None => r.to_vec(),
        }
    }

    /// Visit of an expression: a call's operands, else everything under it.
    fn visit_expr(&mut self, env: &mut TypeEnv, e: &Expr) {
        match &e.terms {
            ExprTerms::Call(ts) => {
                ts.iter().skip(1).for_each(|t| self.walk(env, t));
                return;
            }
            ExprTerms::Term(t) => {
                self.walk(env, t);
                return;
            }
            ExprTerms::Some(d) => d.symbols.iter().for_each(|t| self.walk(env, t)),
            ExprTerms::Every(ev) => {
                if let Some(k) = &ev.key {
                    self.walk(env, k);
                }
                self.walk(env, &ev.value);
                self.walk(env, &ev.domain);
                ev.body.iter().for_each(|x| self.visit_expr(env, x));
            }
        }
        for w in &e.with {
            self.walk(env, &w.target);
            self.walk(env, &w.value);
        }
    }

    fn walk(&mut self, env: &mut TypeEnv, t: &Term) {
        match &t.value {
            TermValue::ArrayCompr(..) | TermValue::SetCompr(..) | TermValue::ObjectCompr(..) => {}
            TermValue::Ref(r) => {
                if let Some(e) = self.check_apply(env, r) {
                    self.errs.push(e);
                    return;
                }
                let top = env.top_level();
                if let Some(e) = self.check_ref(env, top, &[], r, 0) {
                    self.errs.push(e);
                }
                r.iter().for_each(|x| self.walk(env, x));
            }
            TermValue::Array(a) | TermValue::Call(a) => a.iter().for_each(|x| self.walk(env, x)),
            TermValue::Object(o) => {
                for (k, v) in sorted_pairs(o) {
                    self.walk(env, k);
                    self.walk(env, v);
                }
            }
            TermValue::Set(s) => sorted_items(s).into_iter().for_each(|x| self.walk(env, x)),
            TermValue::TemplateString { parts, .. } => {
                for p in parts {
                    match p {
                        TemplatePart::Term(x) => self.walk(env, x),
                        TemplatePart::Expr(e) => self.visit_expr(env, e),
                    }
                }
            }
            _ => {}
        }
    }

    /// checkApply: a function named without a call.
    fn check_apply(&self, env: &TypeEnv, r: &[Term]) -> Option<CompileError> {
        let tpe = env.get_by_ref(r);
        if !matches!(tpe, Type::Function { .. }) {
            return None;
        }
        let loc = r.first().and_then(|t| t.loc.clone());
        let pos = isize::try_from(r.len()).unwrap_or(0).saturating_sub(1);
        Some(ref_err_unsupported(loc, &self.rewrite(r), pos, &tpe))
    }

    /// getOneOfForNode: a node's keys, in order.
    fn one_of_node(env: &TypeEnv, level: usize, node: &[Term]) -> Vec<Term> {
        env.node_at(level, node)
            .map(Node::sorted_keys)
            .unwrap_or_default()
    }

    /// checkRef: a ref through the type tree of the frame at `level`, from the node
    /// at `node` there.
    fn check_ref(
        &mut self,
        env: &mut TypeEnv,
        level: usize,
        node: &[Term],
        r: &[Term],
        idx: usize,
    ) -> Option<CompileError> {
        let head = r.get(idx)?;
        let loc = r.first().and_then(|t| t.loc.clone());
        if (idx == 1 || idx == 2) && !matches!(head.value, TermValue::Var(_) | TermValue::String(_)) {
            let have = get_value(env, head);
            let one_of = Self::one_of_node(env, level, node);
            return Some(ref_err_invalid(
                loc,
                &self.rewrite(r),
                idx,
                &have,
                &Type::String,
                &one_of,
            ));
        }
        if matches!(head.value, TermValue::Var(_)) && idx != 0 {
            let tpe = types::keys(&env.node_at(level, node).map(Node::extent).unwrap_or(Type::Nil));
            let exist = get_value(env, head);
            if !exist.is_nil() {
                if !unifies(&tpe, &exist) {
                    let one_of = Self::one_of_node(env, level, node);
                    return Some(ref_err_invalid(loc, &self.rewrite(r), idx, &exist, &tpe, &one_of));
                }
            } else {
                env.top_mut().put_one(head, tpe);
            }
        }
        let child = env
            .node_at(level, node)
            .and_then(|n| n.child(head))
            .map(|c| (c.leaf(), c.value.clone()));
        match child {
            None => {
                if let Some(l) = level.checked_sub(1) {
                    return self.check_ref(env, l, &[], r, 0);
                }
                if is_root_doc(r.first()) {
                    if idx != 0 {
                        for k in Self::one_of_node(env, level, node) {
                            let mut p = node.to_vec();
                            p.push(k);
                            let _ = self.check_ref(env, level, &p, r, idx + 1);
                        }
                        return None;
                    }
                    return self.check_ref_leaf(env, &A, r, 1);
                }
                self.check_ref_leaf(env, &A, r, 0)
            }
            Some((true, v)) => self.check_ref_leaf(env, &v, r, idx + 1),
            Some((false, _)) => {
                let mut p = node.to_vec();
                p.push(head.clone());
                self.check_ref(env, level, &p, r, idx + 1)
            }
        }
    }

    /// checkRefLeaf: the rest of a ref through a type.
    fn check_ref_leaf(
        &mut self,
        env: &mut TypeEnv,
        tpe: &Type,
        r: &[Term],
        idx: usize,
    ) -> Option<CompileError> {
        let head = r.get(idx)?;
        let loc = r.first().and_then(|t| t.loc.clone());
        let keys = types::keys(tpe);
        if keys.is_nil() {
            let pos = isize::try_from(idx).unwrap_or(0).saturating_sub(1);
            return Some(ref_err_unsupported(loc, &self.rewrite(r), pos, tpe));
        }
        match &head.value {
            TermValue::Var(_) => {
                let exist = get_value(env, head);
                if !exist.is_nil() {
                    if !unifies(&exist, &keys) {
                        return Some(ref_err_invalid(
                            loc,
                            &self.rewrite(r),
                            idx,
                            &exist,
                            &keys,
                            &one_of_type(tpe),
                        ));
                    }
                } else {
                    env.top_mut().put_one(head, keys.clone());
                }
            }
            TermValue::Ref(x) => {
                let exist = env.get_by_ref(x);
                if !exist.is_nil() && !unifies(&exist, &keys) {
                    return Some(ref_err_invalid(
                        loc,
                        &self.rewrite(r),
                        idx,
                        &exist,
                        &keys,
                        &one_of_type(tpe),
                    ));
                }
            }
            TermValue::Array(_) | TermValue::Object(_) | TermValue::Set(_) => {
                if !unify1(env, head, &keys, false) {
                    let have = get_value(env, head);
                    return Some(ref_err_invalid(loc, &self.rewrite(r), idx, &have, &keys, &[]));
                }
            }
            _ => {
                let child = select_constant(tpe, head);
                if child.is_nil() {
                    return Some(ref_err_invalid(
                        loc,
                        &self.rewrite(r),
                        idx,
                        &Type::Nil,
                        &keys,
                        &one_of_type(tpe),
                    ));
                }
                return self.check_ref_leaf(env, &child, r, idx + 1);
            }
        }
        self.check_ref_leaf(env, &types::values(tpe), r, idx + 1)
    }
}
