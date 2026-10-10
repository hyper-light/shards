//! OPA's rewriting of local variables (ast/compile.go `rewriteLocalVars` and its helpers):
//! every variable a rule declares (`:=`, `some`, `every`, arguments) is renamed to a
//! fresh `__localN__`, numbered as OPA numbers them, and misuses are reported in OPA's
//! words.

use std::collections::HashMap;
use std::rc::Rc;

use super::vars::{self, Var, VarSet, sorted_items, sorted_pairs};
use super::{COMPILE_ERR, CompileError};
use crate::ast::{Body, Expr, ExprTerms, Location, Rule, TemplatePart, Term, TermValue};

/// localVarGenerator: `__local<n>__`, skipping names the modules use.
#[derive(Debug, Clone)]
pub struct LocalVarGen {
    exclude: VarSet,
    suffix: String,
    next: usize,
}

impl LocalVarGen {
    pub fn new(exclude: VarSet, suffix: &str) -> LocalVarGen {
        LocalVarGen {
            exclude,
            suffix: format!("__local{suffix}"),
            next: 0,
        }
    }

    pub fn generate(&mut self) -> Var {
        loop {
            let v: Var = format!("{}{}__", self.suffix, self.next).into();
            self.next += 1;
            if !self.exclude.contains(&v) {
                return v;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Occurrence {
    New,
    Arg,
    Seen,
    Assigned,
    Declared,
}

#[derive(Debug, Clone, Default)]
struct DeclaredVarSet {
    vs: HashMap<Var, Var>,
    occurrence: HashMap<Var, Occurrence>,
    count: HashMap<Var, usize>,
    /// How many terms each name's uses were rewritten to here: checkUnusedDeclaredVars
    /// asks whether a declared variable is among the body's variables, which these say
    /// without a walk of the body and every body within it.
    uses: HashMap<Var, usize>,
    /// How many names this scope has declared (anything but seen).
    decls: usize,
}

/// localDeclaredVars. OPA looks a name up scope by scope, from the innermost out; each
/// name here keeps the scopes it is in, so a lookup costs the same however deep the
/// scopes nest (an `every` in an `every`, thousands deep, each a scope).
#[derive(Debug, Clone)]
pub struct Stack {
    vars: Vec<DeclaredVarSet>,
    /// Each name's scopes with it declared (`vs` and `occurrence`), innermost last.
    declared_at: HashMap<Var, Vec<usize>>,
    /// Each name's scopes with it counted, innermost last.
    counted_at: HashMap<Var, Vec<usize>>,
    pub rewritten: HashMap<Var, Var>,
    pub assignment: bool,
}

impl Default for Stack {
    fn default() -> Stack {
        Stack {
            vars: vec![DeclaredVarSet::default()],
            declared_at: HashMap::new(),
            counted_at: HashMap::new(),
            rewritten: HashMap::new(),
            assignment: false,
        }
    }
}

impl Stack {
    /// Copy: every level merged into one, as OPA's Copy does.
    fn copy(&self) -> Stack {
        let mut merged = DeclaredVarSet::default();
        for s in &self.vars {
            merged.vs.extend(s.vs.iter().map(|(k, v)| (k.clone(), v.clone())));
            merged
                .occurrence
                .extend(s.occurrence.iter().map(|(k, v)| (k.clone(), *v)));
            merged.count.extend(s.count.iter().map(|(k, v)| (k.clone(), *v)));
            merged.uses.extend(s.uses.iter().map(|(k, v)| (k.clone(), *v)));
        }
        let declared_at = merged.vs.keys().map(|k| (k.clone(), vec![0])).collect();
        let counted_at = merged.count.keys().map(|k| (k.clone(), vec![0])).collect();
        let mut vars = vec![merged];
        for _ in 1..self.vars.len() {
            vars.push(DeclaredVarSet::default());
        }
        Stack {
            vars,
            declared_at,
            counted_at,
            rewritten: self.rewritten.clone(),
            assignment: false,
        }
    }

    fn push(&mut self) {
        self.vars.push(DeclaredVarSet::default());
    }

    fn pop(&mut self) {
        if self.vars.len() > 1
            && let Some(top) = self.vars.pop()
        {
            let at = self.vars.len();
            let unscope = |index: &mut HashMap<Var, Vec<usize>>, k: &Var| {
                if let Some(frames) = index.get_mut(k) {
                    if frames.last() == Some(&at) {
                        frames.pop();
                    }
                    if frames.is_empty() {
                        index.remove(k);
                    }
                }
            };
            for k in top.vs.keys() {
                unscope(&mut self.declared_at, k);
            }
            for k in top.count.keys() {
                unscope(&mut self.counted_at, k);
            }
        }
    }

    /// The innermost scope's place, made when there is none.
    fn top_at(&mut self) -> usize {
        if self.vars.is_empty() {
            self.vars.push(DeclaredVarSet::default());
        }
        self.vars.len() - 1
    }

    fn peek(&self) -> Option<&DeclaredVarSet> {
        self.vars.last()
    }

    fn insert(&mut self, x: &Var, y: &Var, occ: Occurrence) {
        let at = self.top_at();
        if let Some(top) = self.vars.get_mut(at) {
            if top.vs.insert(x.clone(), y.clone()).is_none() {
                self.declared_at.entry(x.clone()).or_default().push(at);
            }
            top.occurrence.insert(x.clone(), occ);
            if top.count.insert(x.clone(), 1).is_none() {
                self.counted_at.entry(x.clone()).or_default().push(at);
            }
            if occ != Occurrence::Seen {
                top.decls += 1;
            }
        }
        if x != y {
            self.rewritten.insert(y.clone(), x.clone());
        }
    }

    /// How many names the innermost scope has declared.
    fn decls(&self) -> usize {
        self.peek().map_or(0, |s| s.decls)
    }

    /// The innermost scope a name is declared in.
    fn declaring(&self, x: &str) -> Option<&DeclaredVarSet> {
        let at = *self.declared_at.get(x)?.last()?;
        self.vars.get(at)
    }

    fn declared(&self, x: &str) -> Option<Var> {
        self.declaring(x)?.vs.get(x).cloned()
    }

    /// What a use of a name is rewritten to, counted as a use where it is declared.
    fn use_var(&mut self, x: &str) -> Option<Var> {
        let at = *self.declared_at.get(x)?.last()?;
        let s = self.vars.get_mut(at)?;
        let (k, gv) = s.vs.get_key_value(x)?;
        let (k, gv) = (k.clone(), gv.clone());
        *s.uses.entry(k).or_default() += 1;
        Some(gv)
    }

    /// A use of a name taken back: a term rewritten to its declaration and then back.
    fn unuse(&mut self, x: &str) {
        if let Some(&at) = self.declared_at.get(x).and_then(|f| f.last())
            && let Some(n) = self.vars.get_mut(at).and_then(|s| s.uses.get_mut(x))
        {
            *n = n.saturating_sub(1);
        }
    }

    fn occurrence(&self, x: &str) -> Occurrence {
        self.peek()
            .and_then(|s| s.occurrence.get(x).copied())
            .unwrap_or(Occurrence::New)
    }

    fn global_occurrence(&self, x: &str) -> Option<Occurrence> {
        self.declaring(x)?.occurrence.get(x).copied()
    }

    fn seen(&mut self, x: &Var) {
        if let Some(&at) = self.counted_at.get(&**x).and_then(|f| f.last())
            && let Some(c) = self.vars.get_mut(at).and_then(|s| s.count.get_mut(x))
        {
            *c += 1;
            return;
        }
        let at = self.top_at();
        if let Some(top) = self.vars.get_mut(at)
            && top.count.insert(x.clone(), 1).is_none()
        {
            self.counted_at.entry(x.clone()).or_default().push(at);
        }
    }

    fn count(&self, x: &str) -> usize {
        self.counted_at
            .get(x)
            .and_then(|f| f.last())
            .and_then(|&at| self.vars.get(at))
            .and_then(|s| s.count.get(x).copied())
            .unwrap_or(0)
    }
}

/// The state a rewrite threads through: the generator and the errors.
#[derive(Debug)]
pub struct Rewriter<'a> {
    pub vargen: &'a mut LocalVarGen,
    pub errs: Vec<CompileError>,
}

fn err(loc: &Option<Location>, msg: String) -> CompileError {
    CompileError::new(COMPILE_ERR, loc.clone(), msg)
}

/// rewriteDeclaredVar.
fn declare(rw: &mut Rewriter<'_>, stack: &mut Stack, v: &Var, occ: Occurrence) -> Result<Var, String> {
    match stack.occurrence(v) {
        Occurrence::Seen => return Err(format!("var {v} referenced above")),
        Occurrence::Assigned => return Err(format!("var {v} assigned above")),
        Occurrence::Declared => return Err(format!("var {v} declared above")),
        Occurrence::Arg => return Err(format!("arg {v} redeclared")),
        Occurrence::New => {}
    }
    let gv = rw.vargen.generate();
    stack.insert(v, &gv, occ);
    Ok(gv)
}

/// rewriteLocalArgVars: each argument variable becomes a fresh local.
pub fn rewrite_arg_vars(rw: &mut Rewriter<'_>, stack: &mut Stack, rule: &mut Rule) {
    let mut args = std::mem::take(&mut rule.head.args);
    for a in args.iter_mut() {
        arg_term(rw, stack, a);
    }
    rule.head.args = args;
}

fn arg_term(rw: &mut Rewriter<'_>, stack: &mut Stack, t: &mut Term) {
    match &mut t.value {
        TermValue::Var(v) => {
            let v = v.clone();
            let gv = match stack.declared(&v) {
                Some(gv) => {
                    stack.seen(&v);
                    gv
                }
                None => {
                    let gv = rw.vargen.generate();
                    stack.insert(&v, &gv, Occurrence::Arg);
                    gv
                }
            };
            t.value = TermValue::Var(gv);
        }
        TermValue::Object(o) => {
            let mut pairs = sorted_pairs(o).into_iter().cloned().collect::<Vec<_>>();
            for (_, v) in pairs.iter_mut() {
                arg_term(rw, stack, v);
            }
            *o = pairs.into();
        }
        TermValue::Null
        | TermValue::Bool(_)
        | TermValue::Number(_)
        | TermValue::String(_)
        | TermValue::ArrayCompr(..)
        | TermValue::SetCompr(..)
        | TermValue::ObjectCompr(..)
        | TermValue::Set(_)
        | TermValue::TemplateString { .. } => {}
        TermValue::Call(_) => rw
            .errs
            .push(err(&t.loc, "rule arguments cannot contain calls".into())),
        TermValue::Ref(r) | TermValue::Array(r) => r.iter_mut().for_each(|x| arg_term(rw, stack, x)),
    }
}

/// IsScalar.
pub fn is_scalar(t: &Term) -> bool {
    matches!(
        t.value,
        TermValue::Null | TermValue::Bool(_) | TermValue::Number(_) | TermValue::String(_)
    )
}

/// headMayHaveVars.
fn head_may_have_vars(rule: &Rule) -> bool {
    let h = &rule.head;
    h.args.iter().any(|a| !is_scalar(a))
        || h.key.as_ref().is_some_and(|k| !is_scalar(k))
        || h.value.as_ref().is_some_and(|v| !is_scalar(v))
        || h.ref_path().iter().skip(1).any(|t| !is_scalar(t))
}

/// rewriteLocalVarsInRule. Returns the stack it used, for counting arguments' uses.
pub fn rewrite_rule(
    rw: &mut Rewriter<'_>,
    rewritten: &mut HashMap<Var, Var>,
    rule: &mut Rule,
    args_stack: &Stack,
) -> Stack {
    let only_scalars = !head_may_have_vars(rule);
    let mut used = VarSet::new();
    if !only_scalars {
        // rewriteNestedHeadVarLocalTransform: closures in the head get their own locals.
        nested_head(rw, rewritten, rule);
        let mut vis = vars::VarVisitor::default();
        for t in rule.head.ref_path().iter().skip(1) {
            if !is_scalar(t) {
                vis.term(t);
            }
        }
        if let Some(k) = &rule.head.key
            && !is_scalar(k)
        {
            vis.term(k);
        }
        if let Some(v) = &rule.head.value
            && !is_scalar(v)
        {
            vis.vars.extend(vars::term_vars(v));
        }
        used = vis.vars;
    }
    let mut stack = args_stack.copy();
    let body = std::mem::take(&mut rule.body);
    rule.body = rewrite_body(rw, &mut stack, &used, body);
    rewritten.extend(stack.rewritten.iter().map(|(k, v)| (k.clone(), v.clone())));
    if only_scalars {
        return stack;
    }
    let declared: HashMap<Var, Var> = stack.peek().map(|s| s.vs.clone()).unwrap_or_default();
    let xform = |t: &mut Term| head_var_local(&declared, t);
    rule.head.args.iter_mut().for_each(xform);
    if rule.head.reference.is_empty() {
        rule.head.reference = rule.head.ref_path();
    }
    rule.head.reference.iter_mut().skip(1).for_each(xform);
    if let Some(k) = rule.head.key.as_mut() {
        xform(k);
    }
    if let Some(v) = rule.head.value.as_mut() {
        xform(v);
    }
    stack
}

/// rewriteHeadVarLocalTransform: every variable declared in the body is renamed.
fn head_var_local(declared: &HashMap<Var, Var>, t: &mut Term) {
    super::transform_vars(t, &mut |v: &Var| declared.get(v).cloned());
}

fn nested_head(rw: &mut Rewriter<'_>, rewritten: &mut HashMap<Var, Var>, rule: &mut Rule) {
    let mut f = |t: &mut Term, rw: &mut Rewriter<'_>| -> bool {
        let mut stack = Stack::default();
        let stop = match &mut t.value {
            TermValue::ArrayCompr(term, body) | TermValue::SetCompr(term, body) => {
                comprehension(rw, &mut stack, &mut [&mut **term], body);
                true
            }
            TermValue::ObjectCompr(k, v, body) => {
                comprehension(rw, &mut stack, &mut [&mut **k, &mut **v], body);
                true
            }
            TermValue::TemplateString { parts, .. } => {
                template_string(rw, &mut stack, parts);
                true
            }
            _ => false,
        };
        rewritten.extend(stack.rewritten);
        stop
    };
    let h = &mut rule.head;
    for a in h.args.iter_mut() {
        walk_terms_mut(a, &mut |t| f(t, rw));
    }
    if let Some(k) = h.key.as_mut() {
        walk_terms_mut(k, &mut |t| f(t, rw));
    }
    if let Some(v) = h.value.as_mut() {
        walk_terms_mut(v, &mut |t| f(t, rw));
    }
}

/// A GenericVisitor walk over the terms under a term, objects and sets copied and
/// walked in sorted order; `f` returning true stops the descent.
pub fn walk_terms_mut(t: &mut Term, f: &mut dyn FnMut(&mut Term) -> bool) {
    if f(t) {
        return;
    }
    match &mut t.value {
        TermValue::Ref(r) | TermValue::Array(r) | TermValue::Call(r) => {
            r.iter_mut().for_each(|x| walk_terms_mut(x, f))
        }
        TermValue::Object(o) => {
            let mut pairs: Vec<(Term, Term)> = sorted_pairs(o).into_iter().cloned().collect();
            for (k, v) in pairs.iter_mut() {
                walk_terms_mut(k, f);
                walk_terms_mut(v, f);
            }
            *o = pairs.into();
        }
        TermValue::Set(s) => {
            let mut items: Vec<Term> = sorted_items(s).into_iter().cloned().collect();
            items.iter_mut().for_each(|x| walk_terms_mut(x, f));
            *s = items.into();
        }
        TermValue::ArrayCompr(term, body) | TermValue::SetCompr(term, body) => {
            walk_terms_mut(term, f);
            body.iter_mut().for_each(|e| walk_expr_terms_mut(e, f));
        }
        TermValue::ObjectCompr(k, v, body) => {
            walk_terms_mut(k, f);
            walk_terms_mut(v, f);
            body.iter_mut().for_each(|e| walk_expr_terms_mut(e, f));
        }
        TermValue::TemplateString { parts, .. } => {
            for p in parts.iter_mut() {
                match p {
                    TemplatePart::Term(t) => walk_terms_mut(t, f),
                    TemplatePart::Expr(e) => walk_expr_terms_mut(e, f),
                }
            }
        }
        _ => {}
    }
}

/// The terms of an expression, in GenericVisitor order.
pub fn walk_expr_terms_mut(e: &mut Expr, f: &mut dyn FnMut(&mut Term) -> bool) {
    match &mut e.terms {
        ExprTerms::Term(t) => walk_terms_mut(t, f),
        ExprTerms::Call(c) => c.iter_mut().for_each(|t| walk_terms_mut(t, f)),
        ExprTerms::Some(d) => d.symbols.iter_mut().for_each(|t| walk_terms_mut(t, f)),
        ExprTerms::Every(ev) => {
            if let Some(k) = ev.key.as_mut() {
                walk_terms_mut(k, f);
            }
            walk_terms_mut(&mut ev.value, f);
            walk_terms_mut(&mut ev.domain, f);
            ev.body.iter_mut().for_each(|x| walk_expr_terms_mut(x, f));
        }
    }
    for w in e.with.iter_mut() {
        walk_terms_mut(&mut w.target, f);
        walk_terms_mut(&mut w.value, f);
    }
}

/// What a later walk of a body may skip of an `every` statement in it, which
/// rewriteEveryStatement walks again whole after its body, at every level of nesting:
/// one whose terms hold no comprehension, template string or `with`, walked when the
/// scope it is in had declared as many names as it has now, would be walked to the same
/// terms, its scope and the scopes around it declaring nothing it holds.
#[derive(Debug, Clone, Copy)]
struct Settled {
    plain: bool,
    decls: usize,
}

impl Settled {
    fn holds(s: Option<&Option<Settled>>, stack: &Stack) -> bool {
        s.copied()
            .flatten()
            .is_some_and(|s| s.plain && s.decls == stack.decls())
    }
}

/// A body being rewritten, and the `every` statement it is the body of.
struct BodyFrame {
    rest: std::vec::IntoIter<Expr>,
    cpy: Body,
    settled: Vec<Option<Settled>>,
    /// What checkUnusedDeclaredVars reads of the body as written: each expression's
    /// declarations and place.
    original: Vec<(VarSet, Option<Location>)>,
    used: VarSet,
    every: Option<Expr>,
}

impl BodyFrame {
    fn new(body: Body, used: VarSet, every: Option<Expr>) -> BodyFrame {
        let original = body
            .iter()
            .map(|e| (declared_vars_expr(e), e.loc.clone()))
            .collect();
        BodyFrame {
            rest: body.into_iter(),
            cpy: Vec::new(),
            settled: Vec::new(),
            original,
            used,
            every,
        }
    }

    fn push(&mut self, e: Expr, s: Option<Settled>) {
        push(&mut self.cpy, e);
        self.settled.push(s);
    }
}

/// rewriteDeclaredVarsInBody.
pub fn rewrite_body(rw: &mut Rewriter<'_>, stack: &mut Stack, used: &VarSet, body: Body) -> Body {
    rewrite_body_settled(rw, stack, used, body).0
}

/// rewriteDeclaredVarsInBody, with what a walk of the body may skip of each expression.
/// An `every` statement's body is rewritten on a stack of bodies, not in a call within
/// the call rewriting the body it is in: policies nest them 99990 deep.
fn rewrite_body_settled(
    rw: &mut Rewriter<'_>,
    stack: &mut Stack,
    used: &VarSet,
    body: Body,
) -> (Body, Vec<Option<Settled>>) {
    let mut frames = vec![BodyFrame::new(body, used.clone(), None)];
    loop {
        if let Some(expr) = frames.last_mut().and_then(|f| f.rest.next()) {
            if expr.is_every() {
                match every_open(rw, stack, expr) {
                    Opened::Body(every, body) => {
                        frames.push(BodyFrame::new(body, VarSet::new(), Some(every)))
                    }
                    Opened::Expr(e) => {
                        if let Some(top) = frames.last_mut() {
                            top.push(e, None);
                        }
                    }
                    Opened::Failed => {}
                }
                continue;
            }
            let out = if expr.is_assignment() {
                stack.assignment = true;
                Some(assignment(rw, stack, expr))
            } else if expr.is_some() {
                some_decl(rw, stack, expr)
            } else {
                Some(rewrite_expr(rw, stack, expr))
            };
            if let (Some(e), Some(top)) = (out, frames.last_mut()) {
                top.push(e, None);
            }
            continue;
        }
        let Some(mut done) = frames.pop() else {
            return (Vec::new(), Vec::new());
        };
        if done.cpy.is_empty() {
            done.push(Expr::term(Term::boolean(true, None)), None);
        }
        check_unused_declared(rw, stack, &done.used, &done.original);
        let Some(every) = done.every else {
            return (done.cpy, done.settled);
        };
        let (out, settled) = every_close(rw, stack, every, done.cpy, &done.settled);
        if let Some(top) = frames.last_mut() {
            top.push(out, Some(settled));
        }
    }
}

/// Body.Append.
pub fn push(body: &mut Body, mut e: Expr) {
    e.index = body.len();
    body.push(e);
}

/// checkUnusedDeclaredVars. Whether a declared variable is among the variables of the
/// body as rewritten is whether a use of it was rewritten to it: the body holds it no
/// other way (its declaration is not in the body).
fn check_unused_declared(
    rw: &mut Rewriter<'_>,
    stack: &Stack,
    used: &VarSet,
    body: &[(VarSet, Option<Location>)],
) {
    if !rw.errs.is_empty() {
        return;
    }
    let Some(dvs) = stack.peek() else { return };
    if !dvs.occurrence.values().any(|o| *o == Occurrence::Declared) {
        return;
    }
    let declared: VarSet = dvs
        .occurrence
        .iter()
        .filter(|(_, o)| **o == Occurrence::Declared)
        .filter_map(|(v, _)| dvs.vs.get(v).cloned())
        .collect();
    let mut bodyvars: VarSet = dvs
        .occurrence
        .iter()
        .filter(|(v, o)| **o == Occurrence::Declared && dvs.uses.get(*v).is_some_and(|n| *n > 0))
        .filter_map(|(v, _)| dvs.vs.get(v).cloned())
        .collect();
    for v in used {
        bodyvars.insert(stack.declared(v).unwrap_or_else(|| v.clone()));
    }
    let dbv: VarSet = declared.difference(&bodyvars).cloned().collect();
    if dbv.difference(used).count() == 0 {
        return;
    }
    let reversed: HashMap<Var, Var> = dvs.vs.iter().map(|(k, v)| (v.clone(), k.clone())).collect();
    for gv in dbv.difference(used) {
        let Some(rv) = reversed.get(gv) else { continue };
        if vars::is_generated(rv) {
            continue;
        }
        let at = body.iter().find(|(d, _)| d.contains(rv)).or(body.first());
        rw.errs.push(err(
            &at.and_then(|(_, loc)| loc.clone()),
            format!("declared var {rv} unused"),
        ));
    }
}

/// declaredVars of one expression: what `some` and `:=` declare in it.
pub fn declared_vars_expr(e: &Expr) -> VarSet {
    let mut out = VarSet::new();
    match &e.terms {
        ExprTerms::Some(d) => {
            for s in &d.symbols {
                match &s.value {
                    TermValue::Var(v) => {
                        out.insert(v.clone());
                    }
                    TermValue::Call(c) => {
                        // `some k, v in xs` declares its key and value variables.
                        for t in c.iter().skip(1).take(c.len().saturating_sub(2)) {
                            out.extend(vars::term_vars(t));
                        }
                    }
                    _ => {}
                }
            }
        }
        ExprTerms::Call(_) if e.is_assignment() => {
            if let Some(lhs) = e.operand(0) {
                out.extend(vars::term_vars(lhs));
            }
        }
        _ => {}
    }
    out
}

/// What [`every_open`] leaves.
enum Opened {
    /// The statement, and its body to rewrite in the statement's scope.
    Body(Expr, Body),
    /// What takes the statement's place: itself, not being one.
    Expr(Expr),
    /// Nothing: a declaration failed.
    Failed,
}

/// rewriteEveryStatement up to its body: the domain rewritten, then the key and value
/// declared in a scope of their own.
fn every_open(rw: &mut Rewriter<'_>, stack: &mut Stack, mut expr: Expr) -> Opened {
    let loc = expr.loc.clone();
    let ExprTerms::Every(ev) = &mut expr.terms else {
        return Opened::Expr(expr);
    };
    term_recursive(rw, stack, &mut ev.domain);
    stack.push();
    let fail = |rw: &mut Rewriter<'_>, m: String| rw.errs.push(err(&ev.loc.clone().or(loc.clone()), m));
    if let Some(k) = ev.key.as_mut() {
        if let Some(v) = k.as_var().map(Rc::from)
            && !vars::is_wildcard(&v)
        {
            match declare(rw, stack, &v, Occurrence::Declared) {
                Ok(gv) => k.value = TermValue::Var(gv),
                Err(m) => {
                    fail(rw, m);
                    stack.pop();
                    return Opened::Failed;
                }
            }
        }
    } else {
        ev.key = Some(Term::new(TermValue::Var(rw.vargen.generate()), None));
    }
    if let Some(v) = ev.value.as_var().map(Rc::from)
        && !vars::is_wildcard(&v)
    {
        match declare(rw, stack, &v, Occurrence::Declared) {
            Ok(gv) => ev.value.value = TermValue::Var(gv),
            Err(m) => {
                fail(rw, m);
                stack.pop();
                return Opened::Failed;
            }
        }
    }
    let body = std::mem::take(&mut ev.body).into_inner();
    Opened::Body(expr, body)
}

/// The rest of rewriteEveryStatement, its body rewritten: the whole statement walked
/// again in its scope (but for what `settled` says the walk would leave as it is), and
/// the scope ended. Gives back what a walk of the body it is in may skip of it.
fn every_close(
    rw: &mut Rewriter<'_>,
    stack: &mut Stack,
    mut expr: Expr,
    body: Body,
    settled: &[Option<Settled>],
) -> (Expr, Settled) {
    if let ExprTerms::Every(ev) = &mut expr.terms {
        ev.body = body.into();
    }
    let out = rewrite_expr_settled(rw, stack, expr, settled);
    stack.pop();
    let plain = out.with.is_empty()
        && match &out.terms {
            ExprTerms::Every(ev) => {
                ev.key.as_ref().is_none_or(plain_term)
                    && plain_term(&ev.value)
                    && plain_term(&ev.domain)
                    && ev.body.iter().enumerate().all(|(i, e)| match settled.get(i) {
                        Some(Some(s)) => s.plain,
                        _ => plain_expr(e),
                    })
            }
            _ => false,
        };
    let decls = stack.decls();
    (out, Settled { plain, decls })
}

/// An expression with no `with`, whose terms hold no comprehension or template string,
/// and is no `every` statement.
fn plain_expr(e: &Expr) -> bool {
    e.with.is_empty()
        && match &e.terms {
            ExprTerms::Term(t) => plain_term(t),
            ExprTerms::Call(c) => c.iter().all(plain_term),
            ExprTerms::Some(d) => d.symbols.iter().all(plain_term),
            ExprTerms::Every(_) => false,
        }
}

/// A term that holds no comprehension or template string, at any depth.
fn plain_term(t: &Term) -> bool {
    let mut todo = vec![t];
    while let Some(t) = todo.pop() {
        match &t.value {
            TermValue::ArrayCompr(..)
            | TermValue::SetCompr(..)
            | TermValue::ObjectCompr(..)
            | TermValue::TemplateString { .. } => return false,
            TermValue::Ref(r) | TermValue::Array(r) | TermValue::Call(r) => todo.extend(r.iter()),
            TermValue::Object(o) => todo.extend(o.iter().flat_map(|(k, v)| [k, v])),
            TermValue::Set(s) => todo.extend(s.iter()),
            TermValue::Null
            | TermValue::Bool(_)
            | TermValue::Number(_)
            | TermValue::String(_)
            | TermValue::Var(_) => {}
        }
    }
    true
}

/// rewriteSomeDeclStatement.
fn some_decl(rw: &mut Rewriter<'_>, stack: &mut Stack, mut expr: Expr) -> Option<Expr> {
    let ExprTerms::Some(decl) = &expr.terms else {
        return Some(expr);
    };
    let decl = decl.clone();
    for s in &decl.symbols {
        match &s.value {
            TermValue::Var(v) => {
                if let Err(m) = declare(rw, stack, v, Occurrence::Declared) {
                    rw.errs.push(err(&decl.loc, m));
                    return None;
                }
            }
            TermValue::Call(c) => {
                let (key, val, container) = match c.as_slice() {
                    [_, k, v, cont] => (k.clone(), v.clone(), cont.clone()),
                    [_, v, cont] => (
                        Term::new(TermValue::Var(rw.vargen.generate()), None),
                        v.clone(),
                        cont.clone(),
                    ),
                    _ => return None,
                };
                let rhs = match &container.value {
                    TermValue::Ref(r) => {
                        let mut r = r.to_vec();
                        r.push(key);
                        Term::reference(r, None)
                    }
                    _ => Term::reference(vec![container.clone(), key], None),
                };
                expr.terms = ExprTerms::Call(vec![op("eq"), val, rhs]);
                let container_vars = vars::term_vars(&container);
                for v0 in super::safety::output_vars_for_expr_eq(&expr, &container_vars) {
                    if let Err(m) = declare(rw, stack, &v0, Occurrence::Declared) {
                        rw.errs.push(err(&decl.loc, m));
                        return None;
                    }
                }
                return Some(rewrite_expr(rw, stack, expr));
            }
            _ => {}
        }
    }
    None
}

/// A reference to an operator: `eq`, `assign`, `equal`.
pub fn op(name: &str) -> Term {
    Term::reference(vec![Term::var(name, None)], None)
}

/// rewriteDeclaredAssignment.
fn assignment(rw: &mut Rewriter<'_>, stack: &mut Stack, mut expr: Expr) -> Expr {
    if expr.negated {
        rw.errs.push(err(
            &expr.loc,
            "cannot assign vars inside negated expression".into(),
        ));
        return expr;
    }
    let before = rw.errs.len();
    let ExprTerms::Call(terms) = &mut expr.terms else {
        return expr;
    };
    if terms.len() != 3 {
        return expr;
    }
    if let Some(rhs) = terms.get_mut(2) {
        term_recursive(rw, stack, rhs);
    }
    for w in expr.with.iter_mut() {
        term_recursive(rw, stack, &mut w.value);
    }
    let ExprTerms::Call(terms) = &mut expr.terms else {
        return expr;
    };
    if let Some(lhs) = terms.get_mut(1) {
        assign_target(rw, stack, lhs);
    }
    if rw.errs.len() == before
        && let Some(o) = terms.first_mut()
    {
        let loc = o.loc.clone();
        *o = Term::reference(vec![Term::var("eq", loc.clone())], loc);
    }
    expr
}

fn assign_target(rw: &mut Rewriter<'_>, stack: &mut Stack, t: &mut Term) {
    match &mut t.value {
        TermValue::Var(v) => {
            let v = v.clone();
            match declare(rw, stack, &v, Occurrence::Assigned) {
                Ok(gv) => t.value = TermValue::Var(gv),
                Err(m) => rw.errs.push(err(&t.loc, m)),
            }
        }
        TermValue::Array(a) => a.iter_mut().for_each(|x| assign_target(rw, stack, x)),
        TermValue::Object(o) => {
            for (_, v) in o.iter_mut() {
                assign_target(rw, stack, v);
            }
        }
        TermValue::Ref(r)
            if r.len() == 1 && matches!(r.first().and_then(Term::as_var), Some("data" | "input")) =>
        {
            let v: Var = r.first().and_then(Term::as_var).unwrap_or_default().into();
            match declare(rw, stack, &v, Occurrence::Assigned) {
                Ok(gv) => t.value = TermValue::Var(gv),
                Err(m) => rw.errs.push(err(&t.loc, m)),
            }
        }
        _ => {
            let name = t.value_name();
            rw.errs.push(err(&t.loc, format!("cannot assign to {name}")));
        }
    }
}

/// rewriteDeclaredVarsInExpr.
fn rewrite_expr(rw: &mut Rewriter<'_>, stack: &mut Stack, expr: Expr) -> Expr {
    rewrite_expr_settled(rw, stack, expr, &[])
}

/// rewriteDeclaredVarsInExpr, skipping the expressions of an `every` statement's own
/// body that `settled` says the walk would leave as they are. A statement within
/// another's body is walked on a stack of statements, not in a call within a call.
fn rewrite_expr_settled(
    rw: &mut Rewriter<'_>,
    stack: &mut Stack,
    expr: Expr,
    settled: &[Option<Settled>],
) -> Expr {
    // The statements being walked, outermost first, each with the place of its body's
    // next expression.
    let mut open: Vec<(Expr, usize)> = Vec::new();
    let mut next = Some(expr);
    loop {
        if let Some(mut e) = next.take() {
            match &mut e.terms {
                ExprTerms::Term(t) => walk_decl_terms(rw, stack, t),
                ExprTerms::Call(c) => c.iter_mut().for_each(|t| walk_decl_terms(rw, stack, t)),
                ExprTerms::Some(d) => d.symbols.iter_mut().for_each(|t| walk_decl_terms(rw, stack, t)),
                ExprTerms::Every(ev) => {
                    if let Some(k) = ev.key.as_mut() {
                        walk_decl_terms(rw, stack, k);
                    }
                    walk_decl_terms(rw, stack, &mut ev.value);
                    walk_decl_terms(rw, stack, &mut ev.domain);
                    open.push((e, 0));
                    continue;
                }
            }
            rewrite_with(rw, stack, &mut e);
            match open.last_mut() {
                Some((parent, i)) => put_back(parent, i.saturating_sub(1), e),
                None => return e,
            }
            continue;
        }
        let outermost = open.len() == 1;
        let Some((parent, i)) = open.last_mut() else {
            return Expr::term(Term::boolean(true, None));
        };
        if let ExprTerms::Every(ev) = &mut parent.terms
            && let Some(x) = ev.body.get_mut(*i)
        {
            *i += 1;
            if !(outermost && Settled::holds(settled.get(*i - 1), stack)) {
                next = Some(std::mem::replace(x, Expr::term(Term::boolean(true, None))));
            }
            continue;
        }
        let Some((mut done, _)) = open.pop() else {
            return Expr::term(Term::boolean(true, None));
        };
        rewrite_with(rw, stack, &mut done);
        match open.last_mut() {
            Some((parent, i)) => put_back(parent, i.saturating_sub(1), done),
            None => return done,
        }
    }
}

/// An expression walked, back in its place in the `every` statement's body it was
/// taken from.
fn put_back(parent: &mut Expr, at: usize, e: Expr) {
    if let ExprTerms::Every(ev) = &mut parent.terms
        && let Some(slot) = ev.body.get_mut(at)
    {
        *slot = e;
    }
}

/// rewriteDeclaredVarsInWithRecursive, for each of an expression's `with` modifiers.
fn rewrite_with(rw: &mut Rewriter<'_>, stack: &mut Stack, expr: &mut Expr) {
    for w in expr.with.iter_mut() {
        term_recursive(rw, stack, &mut w.target);
        if let Some(sdw) = stack.declared("input") {
            match &mut w.target.value {
                TermValue::Var(v) if *v == sdw => {
                    w.target.value = TermValue::Ref(vec![Term::var("input", None)].into());
                    stack.unuse("input");
                }
                TermValue::Ref(r) => {
                    if let Some(first) = r.first_mut()
                        && first.as_var() == Some(&*sdw)
                    {
                        first.value = TermValue::Var("input".into());
                        stack.unuse("input");
                    }
                }
                _ => {}
            }
        }
        term_recursive(rw, stack, &mut w.value);
    }
}

/// A GenericVisitor walk calling rewriteDeclaredVarsInTerm.
fn walk_decl_terms(rw: &mut Rewriter<'_>, stack: &mut Stack, t: &mut Term) {
    if decl_term(rw, stack, t) {
        return;
    }
    match &mut t.value {
        TermValue::Ref(r) | TermValue::Array(r) | TermValue::Call(r) => {
            r.iter_mut().for_each(|x| walk_decl_terms(rw, stack, x))
        }
        TermValue::TemplateString { parts, .. } => {
            for p in parts.iter_mut() {
                if let TemplatePart::Term(t) = p {
                    walk_decl_terms(rw, stack, t);
                } else if let TemplatePart::Expr(e) = p {
                    let x = std::mem::replace(e.as_mut(), Expr::term(Term::boolean(true, None)));
                    **e = rewrite_expr(rw, stack, x);
                }
            }
        }
        _ => {}
    }
}

/// rewriteDeclaredVarsInTermRecursive.
pub fn term_recursive(rw: &mut Rewriter<'_>, stack: &mut Stack, t: &mut Term) {
    walk_decl_terms(rw, stack, t);
}

/// rewriteDeclaredVarsInTerm: true stops the walk below the term.
fn decl_term(rw: &mut Rewriter<'_>, stack: &mut Stack, t: &mut Term) -> bool {
    match &mut t.value {
        TermValue::Var(v) => {
            let v = v.clone();
            if let Some(gv) = stack.use_var(&v) {
                t.value = TermValue::Var(gv);
                stack.seen(&v);
            } else if stack.occurrence(&v) == Occurrence::New {
                stack.insert(&v, &v, Occurrence::Seen);
            }
            true
        }
        TermValue::Ref(r) => {
            if r.len() == 1
                && let Some(x) = r
                    .first()
                    .and_then(Term::as_var)
                    .filter(|x| matches!(*x, "data" | "input"))
            {
                if let Some(occ) = stack.global_occurrence(x)
                    && occ != Occurrence::Seen
                    && let Some(gv) = stack.use_var(x)
                {
                    t.value = TermValue::Var(gv);
                }
                return true;
            }
            false
        }
        TermValue::Call(c) => {
            if let Some(op) = c.first() {
                let mut shadowed = false;
                for v in vars::term_vars(op) {
                    if stack.declared(&v).is_some_and(|gv| gv != v) {
                        shadowed = true;
                        break;
                    }
                }
                if shadowed {
                    rw.errs
                        .push(err(&t.loc, format!("called function {op} shadowed")));
                }
            }
            false
        }
        TermValue::Object(o) => {
            let mut pairs: Vec<(Term, Term)> = sorted_pairs(o).into_iter().cloned().collect();
            for (k, v) in pairs.iter_mut() {
                term_recursive(rw, stack, k);
                term_recursive(rw, stack, v);
            }
            *o = pairs.into();
            true
        }
        TermValue::Set(s) => {
            let mut items: Vec<Term> = sorted_items(s).into_iter().cloned().collect();
            items.iter_mut().for_each(|x| term_recursive(rw, stack, x));
            *s = items.into();
            true
        }
        TermValue::ArrayCompr(term, body) | TermValue::SetCompr(term, body) => {
            comprehension(rw, stack, &mut [&mut **term], body);
            true
        }
        TermValue::ObjectCompr(k, v, body) => {
            comprehension(rw, stack, &mut [&mut **k, &mut **v], body);
            true
        }
        _ => false,
    }
}

/// rewriteDeclaredVarsIn{Array,Set,Object}Comprehension.
fn comprehension(rw: &mut Rewriter<'_>, stack: &mut Stack, terms: &mut [&mut Term], body: &mut Body) {
    let mut used = VarSet::new();
    for t in terms.iter() {
        used.extend(vars::term_vars(t));
    }
    stack.push();
    let b = std::mem::take(body);
    *body = rewrite_body(rw, stack, &used, b);
    for t in terms.iter_mut() {
        term_recursive(rw, stack, t);
    }
    stack.pop();
}

/// rewriteDeclaredVarsInTemplateString.
fn template_string(rw: &mut Rewriter<'_>, stack: &mut Stack, parts: &mut [TemplatePart]) {
    for p in parts.iter_mut() {
        if let TemplatePart::Expr(e) = p {
            stack.push();
            let x = std::mem::replace(e.as_mut(), Expr::term(Term::boolean(true, None)));
            **e = rewrite_expr(rw, stack, x);
            stack.pop();
        }
    }
}

/// How many times the stack saw a variable (for unused-argument counting).
pub fn count(stack: &Stack, v: &str) -> usize {
    stack.count(v)
}
