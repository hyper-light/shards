//! OPA's rewriting stages (ast/compile.go): each turns some construct into the plain
//! unifications and calls the evaluator runs, generating `__localN__` variables in OPA's
//! order.

use super::localvars::{is_scalar, push};
use super::safety::{self, Unsafe};
use super::transform::{self, Transformer};
use super::vars::{self, SAFETY, Var, VarSet, VarVisitor};
use super::{CompileError, Compiler, RuleTree, TYPE_ERR, is_empty_body, text_of_ref};
use crate::ast::{Body, Expr, ExprTerms, Location, Rule, TemplatePart, Term, TermValue};
use crate::types::Type;

/// equalityFactory.Generate: `__localN__ = other`, generated.
pub fn generate(c: &mut Compiler, other: Term) -> Expr {
    let loc = other.loc.clone();
    let v = Term::new(TermValue::Var(c.vargen().generate()), loc.clone());
    let mut e = Expr::new(ExprTerms::Call(vec![super::localvars::op("eq"), v, other]), loc);
    e.generated = true;
    e
}

fn operand0(e: &Expr) -> Term {
    e.operand(0).cloned().unwrap_or_else(|| Term::boolean(true, None))
}

/// appendToBody: a body of only `true` is replaced.
pub fn append_to_body(body: &mut Body, exprs: Vec<Expr>) {
    let mut exprs = exprs.into_iter();
    if is_empty_body(body)
        && let Some(mut first) = exprs.next()
    {
        first.index = 0;
        body.clear();
        body.push(first);
    }
    for mut e in exprs {
        e.index = body.len();
        body.push(e);
    }
}

/// Every rule and its else chain, in module order.
fn for_each_rule(c: &mut Compiler, mut f: impl FnMut(&mut Compiler, &mut Rule)) {
    let names: Vec<String> = c.modules.keys().cloned().collect();
    for name in names {
        // Each rule is rewritten on a copy written back at once, so the rule tree's
        // lookups see the module whole, as OPA's do.
        let count = c.modules.get(&name).map_or(0, |m| m.rules.len());
        for i in 0..count {
            let Some(mut rule) = c.modules.get(&name).and_then(|m| m.rules.get(i)).cloned() else {
                continue;
            };
            let mut r = Some(&mut rule);
            while let Some(x) = r {
                f(c, x);
                r = x.else_.as_deref_mut();
            }
            if let Some(slot) = c.modules.get_mut(&name).and_then(|m| m.rules.get_mut(i)) {
                *slot = rule;
            }
        }
    }
}

/// rewriteRuleHeadRefs: a non-scalar, non-variable part of a head's ref becomes a local.
pub fn rewrite_rule_head_refs(c: &mut Compiler) {
    for_each_rule(c, |c, rule| {
        if rule.head.reference.is_empty() {
            rule.head.reference = rule.head.ref_path();
        }
        let n = rule.head.reference.len();
        for i in 1..n {
            let Some(part) = rule.head.reference.get(i).cloned() else {
                continue;
            };
            if part.as_var().is_some() || is_scalar(&part) {
                continue;
            }
            let e = generate(c, part.clone());
            let v = operand0(&e);
            if i == n - 1 && rule.head.key.as_ref().is_some_and(|k| k.equal(&part)) {
                rule.head.key = Some(v.clone());
            }
            if let Some(slot) = rule.head.reference.get_mut(i) {
                *slot = v;
            }
            append_to_body(&mut rule.body, vec![e]);
        }
    });
}

/// isConstantRule.
fn is_constant_rule(rule: &Rule) -> bool {
    if !is_empty_body(&rule.body) {
        return false;
    }
    match rule.head.value.as_ref().map(|v| &v.value) {
        Some(
            TermValue::String(_)
            | TermValue::Var(_)
            | TermValue::Number(_)
            | TermValue::Bool(_)
            | TermValue::Null,
        ) => true,
        Some(TermValue::Array(_) | TermValue::Object(_) | TermValue::Set(_)) => {
            rule.head.value.as_ref().is_some_and(Term::is_ground)
        }
        _ => false,
    }
}

/// isRefToKnownDefinedRule.
fn is_ref_to_known_defined_rule(c: &Compiler, r: &[Term]) -> bool {
    if r.len() < 2 || r.first().and_then(Term::as_var) != Some("data") {
        return false;
    }
    let ids = c.tree.exact(r);
    let rules: Vec<&Rule> = ids.iter().filter_map(|id| c.rule(id)).collect();
    let Some(first) = rules.first() else { return false };
    if !first.head.args.is_empty() {
        return false;
    }
    if first.default || first.head.kind() == crate::ast::RuleKind::MultiValue {
        return true;
    }
    if rules.len() == 1 {
        return is_constant_rule(first);
    }
    rules.iter().skip(1).any(|r| r.default)
}

fn arity_of(c: &Compiler) -> impl Fn(&[Term]) -> Option<usize> + '_ {
    move |r: &[Term]| c.arity(r)
}

/// rewriteTemplateStrings.
///
/// OPA's walk rewrites a closure's body (an `every` statement's, a comprehension's),
/// then walks on into the body again with the safe variables of the body around it,
/// at each level: twice the work for each level a body nests. What the second walk does
/// is try again the template strings the first left as they were, failing again (it
/// knows fewer variables safe); a body the first walk left none in it skips, and the
/// walks end once the errors fill what OPA's error limit keeps.
pub fn rewrite_template_strings(c: &mut Compiler) {
    let mut errs = Vec::new();
    let room = super::MAX_ERRS.saturating_sub(c.errors.len());
    for_each_rule(c, |c, rule| {
        if errs.len() >= room {
            return;
        }
        let mut safe = VarSet::new();
        for a in &rule.head.args {
            safe.extend(vars::term_vars(a));
        }
        safe.insert("data".into());
        safe.insert("input".into());
        let mut ts = TemplateWalk {
            c,
            safe,
            added: Vec::new(),
            errs: &mut errs,
            room,
            left: false,
        };
        let head = &mut rule.head;
        ts.body(VarSet::new(), &mut rule.body, |ts| {
            for a in head.args.iter_mut() {
                ts.term(a);
            }
            if let Some(k) = head.key.as_mut() {
                ts.term(k);
            }
            if let Some(v) = head.value.as_mut() {
                ts.term(v);
            }
        });
    });
    c.err(errs);
}

struct TemplateWalk<'a> {
    c: &'a mut Compiler,
    /// The variables safe where the walk is: one set, each body adding its own and
    /// taking them back when done. OPA copies the set for each body, and a body as deep
    /// as n others held n copies, each as large as the depth.
    safe: VarSet,
    added: Vec<Var>,
    errs: &'a mut Vec<CompileError>,
    /// How many errors OPA's limit keeps.
    room: usize,
    /// Whether a template string the walk came to is left as it was.
    left: bool,
}

impl TemplateWalk<'_> {
    fn full(&self) -> bool {
        self.errs.len() >= self.room
    }

    fn add(&mut self, vs: VarSet) {
        for v in vs {
            if self.safe.insert(v.clone()) {
                self.added.push(v);
            }
        }
    }

    fn rewrite(&mut self, t: &mut Term) {
        self.left |= rewrite_template_term(self.c, &self.safe, t, self.errs);
    }

    /// rewriteTemplateStrings over a body, `extra` safe in it besides what is safe around
    /// it, then `then` with the variables safe after it (a comprehension's head terms,
    /// a rule's head): whether a template string in the body is left as it was (its
    /// rewrite failed).
    fn body(&mut self, extra: VarSet, b: &mut Body, then: impl FnOnce(&mut Self)) -> bool {
        let mark = self.added.len();
        self.add(extra);
        let outputs = {
            let arity = arity_of(self.c);
            safety::output_vars_for_body_among(b, &arity, &self.safe)
        };
        self.add(outputs);
        let outer = std::mem::replace(&mut self.left, false);
        for e in b.iter_mut() {
            self.expr(e);
        }
        let left = self.left;
        self.left |= outer;
        then(self);
        for v in self.added.drain(mark..) {
            self.safe.remove(&v);
        }
        left
    }

    fn expr(&mut self, e: &mut Expr) {
        if self.full() {
            return;
        }
        match &mut e.terms {
            ExprTerms::Term(t) => self.term(t),
            ExprTerms::Call(cl) => cl.iter_mut().for_each(|t| self.term(t)),
            ExprTerms::Some(d) => d.symbols.iter_mut().for_each(|t| self.term(t)),
            ExprTerms::Every(ev) => {
                self.rewrite(&mut ev.domain);
                let mut kv = VarSet::new();
                if let Some(k) = &ev.key {
                    kv.extend(vars::term_vars(k));
                }
                kv.extend(vars::term_vars(&ev.value));
                let left = self.body(kv, &mut ev.body, |_| {});
                if let Some(k) = ev.key.as_mut() {
                    self.term(k);
                }
                self.term(&mut ev.value);
                self.term(&mut ev.domain);
                if left {
                    for x in ev.body.iter_mut() {
                        self.expr(x);
                    }
                }
            }
        }
        for w in e.with.iter_mut() {
            self.term(&mut w.target);
            self.term(&mut w.value);
        }
    }

    fn term(&mut self, t: &mut Term) {
        if self.full() {
            return;
        }
        let left = match &mut t.value {
            TermValue::TemplateString { .. } => {
                self.rewrite(t);
                false
            }
            TermValue::SetCompr(x, b) | TermValue::ArrayCompr(x, b) => {
                self.body(VarSet::new(), b, |ts| ts.rewrite(x))
            }
            TermValue::ObjectCompr(k, v, b) => self.body(VarSet::new(), b, |ts| {
                ts.rewrite(k);
                ts.rewrite(v);
            }),
            _ => false,
        };
        match &mut t.value {
            TermValue::Ref(r) | TermValue::Array(r) | TermValue::Call(r) => {
                r.iter_mut().for_each(|x| self.term(x))
            }
            TermValue::Object(o) => {
                for (k, v) in o.iter_mut() {
                    self.term(k);
                    self.term(v);
                }
            }
            TermValue::Set(s) => s.iter_mut().for_each(|x| self.term(x)),
            TermValue::SetCompr(x, b) | TermValue::ArrayCompr(x, b) => {
                self.term(x);
                if left {
                    b.iter_mut().for_each(|e| self.expr(e));
                }
            }
            TermValue::ObjectCompr(k, v, b) => {
                self.term(k);
                self.term(v);
                if left {
                    b.iter_mut().for_each(|e| self.expr(e));
                }
            }
            _ => {}
        }
    }
}

/// rewriteTemplateStringTerm and rewriteTemplateString: true when the term is a template
/// string left as it was, its rewrite failing.
fn rewrite_template_term(
    c: &mut Compiler,
    safe: &VarSet,
    t: &mut Term,
    errs: &mut Vec<CompileError>,
) -> bool {
    let TermValue::TemplateString { parts, .. } = &t.value else {
        return false;
    };
    let parts = parts.clone();
    let loc = t.loc.clone();
    let mut terms = Vec::new();
    let mut local_errs = Vec::new();
    if parts.is_empty() {
        terms.push(Term::string("", loc.clone()));
    }
    for p in parts {
        match p {
            TemplatePart::Expr(e) => {
                let term = match &e.terms {
                    ExprTerms::Call(cl) => {
                        let name = cl.first().map(Term::to_string).unwrap_or_default();
                        if crate::builtins::registry().get(&name).is_some_and(|b| b.relation) {
                            local_errs.push(CompileError::compile(
                                None,
                                format!("illegal call to relation built-in '{name}' that may cause multiple outputs"),
                            ));
                            continue;
                        }
                        Term::new(TermValue::Call(cl.clone().into()), e.loc.clone())
                    }
                    ExprTerms::Term(x) => (**x).clone(),
                    _ => {
                        local_errs.push(CompileError::compile(
                            e.loc.clone(),
                            "unexpected template-string expression type".into(),
                        ));
                        continue;
                    }
                };
                if let Some(r) = term.as_ref()
                    && is_ref_to_known_defined_rule(c, r)
                {
                    let l = term.loc.clone();
                    terms.push(crate::ast::set_term(vec![term], l));
                    continue;
                }
                if term.as_var().is_some() {
                    let l = term.loc.clone();
                    terms.push(crate::ast::set_term(vec![term], l));
                    continue;
                }
                let mut vis = VarVisitor::new(SAFETY);
                vis.term(&term);
                for v in vis.vars.difference(safe) {
                    let v = c.rewritten.get(v).cloned().unwrap_or_else(|| v.clone());
                    local_errs.push(CompileError::compile(
                        term.loc.clone(),
                        format!("var {v} is undeclared"),
                    ));
                }
                let l = term.loc.clone();
                let x = Term::new(TermValue::Var(c.vargen().generate()), l.clone());
                let mut capture = Expr::new(
                    ExprTerms::Call(vec![super::localvars::op("eq"), x.clone(), term]),
                    l.clone(),
                );
                capture.with = e.with.clone();
                terms.push(Term::new(TermValue::SetCompr(x.into(), vec![capture].into()), l));
            }
            TemplatePart::Term(x) => terms.push(x),
        }
    }
    if !local_errs.is_empty() {
        errs.extend(local_errs);
        return true;
    }
    let op = Term::reference(
        vec![Term::var("internal", None), Term::string("template_string", None)],
        None,
    );
    t.value = TermValue::Call(vec![op, Term::new(TermValue::Array(terms.into()), loc)].into());
    false
}

/// checkVoidCalls: a call to a function without a result used as a value.
pub fn check_void_calls(c: &mut Compiler) {
    let mut errs = Vec::new();
    let mut check = |t: &Term, c: &Compiler| {
        if let TermValue::Call(cl) = &t.value
            && let Some(op) = cl.first().and_then(Term::as_ref)
            && let Some(Type::Function { result: None, .. }) = c.builtin_decl(&text_of_ref(op))
        {
            errs.push(CompileError::new(
                TYPE_ERR,
                t.loc.clone(),
                format!("{t} used as value"),
            ));
        }
    };
    let rules: Vec<Rule> = c.modules.values().flat_map(|m| m.rules.iter().cloned()).collect();
    for rule in &rules {
        let mut r = Some(rule);
        while let Some(x) = r {
            let mut terms = Vec::new();
            for t in x
                .head
                .args
                .iter()
                .chain(x.head.key.iter())
                .chain(x.head.value.iter())
            {
                safety::walk_terms(t, &mut |t| {
                    terms.push(t.clone());
                    false
                });
            }
            for e in &x.body {
                safety::walk_terms_expr(e, &mut |t| {
                    terms.push(t.clone());
                    false
                });
            }
            for t in &terms {
                check(t, c);
            }
            r = x.else_.as_deref();
        }
    }
    c.err(errs);
}

fn is_print_call(e: &Expr) -> bool {
    matches!(&e.terms, ExprTerms::Call(cl) if cl.first().is_some_and(|op| op.to_string() == "print"))
}

/// ContainsClosures: an `every` statement is one, and comprehensions in its terms.
fn contains_closures(e: &Expr) -> bool {
    if e.is_every() {
        return true;
    }
    let mut found = false;
    safety::walk_terms_expr(e, &mut |t| {
        if matches!(
            t.value,
            TermValue::ArrayCompr(..) | TermValue::SetCompr(..) | TermValue::ObjectCompr(..)
        ) {
            found = true;
        }
        found
    });
    found
}

/// The print calls of a rule, in its head and body at any depth.
fn count_print_calls(rule: &Rule) -> usize {
    enum Node<'a> {
        E(&'a Expr),
        T(&'a Term),
    }
    let mut todo: Vec<Node<'_>> = rule.body.iter().map(Node::E).collect();
    todo.extend(rule.head.args.iter().map(Node::T));
    todo.extend(rule.head.key.iter().map(Node::T));
    todo.extend(rule.head.value.iter().map(Node::T));
    let mut n = 0;
    while let Some(x) = todo.pop() {
        match x {
            Node::E(e) => {
                if is_print_call(e) {
                    n += 1;
                }
                match &e.terms {
                    ExprTerms::Term(t) => todo.push(Node::T(t)),
                    ExprTerms::Call(cl) => todo.extend(cl.iter().map(Node::T)),
                    ExprTerms::Some(d) => todo.extend(d.symbols.iter().map(Node::T)),
                    ExprTerms::Every(ev) => {
                        todo.extend(ev.key.iter().map(Node::T));
                        todo.push(Node::T(&ev.value));
                        todo.push(Node::T(&ev.domain));
                        todo.extend(ev.body.iter().map(Node::E));
                    }
                }
                for w in &e.with {
                    todo.push(Node::T(&w.target));
                    todo.push(Node::T(&w.value));
                }
            }
            Node::T(t) => match &t.value {
                TermValue::Ref(r) | TermValue::Array(r) | TermValue::Call(r) | TermValue::Set(r) => {
                    todo.extend(r.iter().map(Node::T))
                }
                TermValue::Object(o) => todo.extend(o.iter().flat_map(|(k, v)| [Node::T(k), Node::T(v)])),
                TermValue::ArrayCompr(x, b) | TermValue::SetCompr(x, b) => {
                    todo.push(Node::T(x));
                    todo.extend(b.iter().map(Node::E));
                }
                TermValue::ObjectCompr(k, v, b) => {
                    todo.push(Node::T(k));
                    todo.push(Node::T(v));
                    todo.extend(b.iter().map(Node::E));
                }
                TermValue::TemplateString { parts, .. } => {
                    for p in parts.iter() {
                        match p {
                            TemplatePart::Term(t) => todo.push(Node::T(t)),
                            TemplatePart::Expr(e) => todo.push(Node::E(e)),
                        }
                    }
                }
                TermValue::Null
                | TermValue::Bool(_)
                | TermValue::Number(_)
                | TermValue::String(_)
                | TermValue::Var(_) => {}
            },
        }
    }
    n
}

/// Where a rule's print rewrite is: the print calls not yet rewritten, how many errors
/// OPA's limit keeps, and the variables safe in the body being rewritten.
struct Prints {
    left: usize,
    room: usize,
    /// The globals of the body being rewritten: one set, each closure's body adding what
    /// it adds and taking it back when done. OPA copies the set for each closure, and a
    /// body as deep as n closures held n copies, each as large as the depth.
    safe: VarSet,
    added: Vec<Var>,
}

impl Prints {
    /// Whether the walk is done: a body without print calls left to rewrite is left as
    /// it is, and OPA keeps no errors past its limit.
    fn done(&self, errs: &[CompileError]) -> bool {
        self.left == 0 || errs.len() >= self.room
    }

    fn add(&mut self, vs: VarSet) {
        for v in vs {
            if self.safe.insert(v.clone()) {
                self.added.push(v);
            }
        }
    }

    /// Takes back what was added since `mark` (the length of `added` then).
    fn back_to(&mut self, mark: usize) {
        for v in self.added.drain(mark..) {
            self.safe.remove(&v);
        }
    }
}

/// rewritePrintCalls: `print(a, b)` to `internal.print([{x | x = a}, {y | y = b}])`.
///
/// OPA rewrites each body WalkBodies meets, and rewritePrintCalls each closure's body
/// within it, at each level: a body as deep as n closures is rewritten n times, every
/// time after the first finding nothing left to rewrite. A rule's walk ends once its
/// print calls are all rewritten, or the errors fill what OPA's limit keeps.
pub fn rewrite_print_calls(c: &mut Compiler) {
    if !c.print_enabled() {
        return;
    }
    let mut errs = Vec::new();
    let room = super::MAX_ERRS.saturating_sub(c.errors.len());
    for_each_rule(c, |c, rule| {
        let mut safe: VarSet = ["data", "input"].iter().map(|s| Var::from(*s)).collect();
        for a in &rule.head.args {
            safe.extend(vars::term_vars(a));
        }
        let mut p = Prints {
            left: count_print_calls(rule),
            room,
            safe,
            added: Vec::new(),
        };
        if p.done(&errs) {
            return;
        }
        // WalkBodies over the head, then the body: each body met, outermost first.
        let mut head_terms: Vec<&mut Term> = rule.head.args.iter_mut().collect();
        head_terms.extend(rule.head.key.iter_mut());
        head_terms.extend(rule.head.value.iter_mut());
        for t in head_terms {
            print_bodies_in_term(c, t, &mut errs, &mut p);
        }
        print_bodies(c, &mut rule.body, &mut errs, &mut p);
    });
    c.err(errs);
}

/// WalkBodies with rewritePrintCalls at each body, each with the rule's globals.
fn print_bodies(c: &mut Compiler, body: &mut Body, errs: &mut Vec<CompileError>, p: &mut Prints) {
    if p.done(errs) {
        return;
    }
    print_calls(c, body, errs, p);
    for e in body.iter_mut() {
        print_bodies_in_expr(c, e, errs, p);
    }
}

fn print_bodies_in_expr(c: &mut Compiler, e: &mut Expr, errs: &mut Vec<CompileError>, p: &mut Prints) {
    if p.done(errs) {
        return;
    }
    match &mut e.terms {
        ExprTerms::Term(t) => print_bodies_in_term(c, t, errs, p),
        ExprTerms::Call(cl) => cl.iter_mut().for_each(|t| print_bodies_in_term(c, t, errs, p)),
        ExprTerms::Some(d) => d
            .symbols
            .iter_mut()
            .for_each(|t| print_bodies_in_term(c, t, errs, p)),
        ExprTerms::Every(ev) => {
            print_bodies_in_term(c, &mut ev.domain, errs, p);
            print_bodies(c, &mut ev.body, errs, p);
        }
    }
}

fn print_bodies_in_term(c: &mut Compiler, t: &mut Term, errs: &mut Vec<CompileError>, p: &mut Prints) {
    if p.done(errs) {
        return;
    }
    match &mut t.value {
        TermValue::Ref(r) | TermValue::Array(r) | TermValue::Call(r) | TermValue::Set(r) => {
            r.iter_mut().for_each(|x| print_bodies_in_term(c, x, errs, p))
        }
        TermValue::Object(o) => {
            for (k, v) in o.iter_mut() {
                print_bodies_in_term(c, k, errs, p);
                print_bodies_in_term(c, v, errs, p);
            }
        }
        TermValue::ArrayCompr(x, b) | TermValue::SetCompr(x, b) => {
            print_bodies_in_term(c, x, errs, p);
            print_bodies(c, b, errs, p);
        }
        TermValue::ObjectCompr(k, v, b) => {
            print_bodies_in_term(c, k, errs, p);
            print_bodies_in_term(c, v, errs, p);
            print_bodies(c, b, errs, p);
        }
        _ => {}
    }
}

/// rewritePrintCalls on one body, its globals `p.safe`.
fn print_calls(c: &mut Compiler, body: &mut Body, errs: &mut Vec<CompileError>, p: &mut Prints) {
    for i in 0..body.len() {
        if !body.get(i).is_some_and(contains_closures) {
            continue;
        }
        let mark = p.added.len();
        let outputs = {
            let arity = arity_of(c);
            safety::output_vars_for_body_among(body.get(..i).unwrap_or_default(), &arity, &p.safe)
        };
        p.add(outputs);
        let mut local = Vec::new();
        if let Some(e) = body.get_mut(i) {
            print_closures_in_expr(c, e, &mut local, p);
        }
        p.back_to(mark);
        if !local.is_empty() {
            errs.extend(local);
            return;
        }
    }
    for i in 0..body.len() {
        if !body.get(i).is_some_and(is_print_call) {
            continue;
        }
        let before = body.get(..i).unwrap_or_default();
        let outputs = {
            let arity = arity_of(c);
            safety::output_vars_for_body_among(before, &arity, &p.safe)
        };
        let Some(e) = body.get(i) else { continue };
        let loc = e.loc.clone();
        let args: Vec<Term> = match &e.terms {
            ExprTerms::Call(cl) => cl.get(1..).unwrap_or_default().to_vec(),
            _ => Vec::new(),
        };
        let mut local = Vec::new();
        for a in &args {
            let mut vis = VarVisitor::new(SAFETY);
            vis.term(a);
            for v in &vis.vars {
                // Safe: the globals, the outputs of the expressions before, and (OPA
                // issue #7647) the generated variables anywhere in them.
                let safe = p.safe.contains(v)
                    || outputs.contains(v)
                    || (vars::is_generated(v) && occurs_in(before, v));
                if !safe {
                    local.push(CompileError::compile(
                        a.loc.clone(),
                        format!("var {v} is undeclared"),
                    ));
                }
            }
        }
        if !local.is_empty() {
            errs.extend(local);
            return;
        }
        let mut terms = Vec::new();
        for a in args {
            let l = a.loc.clone();
            let x = Term::new(TermValue::Var(c.vargen().generate()), l.clone());
            let capture = Expr::new(
                ExprTerms::Call(vec![super::localvars::op("eq"), x.clone(), a]),
                l.clone(),
            );
            terms.push(Term::new(TermValue::SetCompr(x.into(), vec![capture].into()), l));
        }
        let op = Term::reference(
            vec![
                Term::var("internal", loc.clone()),
                Term::string("print", loc.clone()),
            ],
            loc.clone(),
        );
        let mut new = Expr::new(
            ExprTerms::Call(vec![op, Term::new(TermValue::Array(terms.into()), loc.clone())]),
            loc,
        );
        new.index = i;
        if let Some(slot) = body.get_mut(i) {
            *slot = new;
            p.left = p.left.saturating_sub(1);
        }
    }
}

/// Whether a variable occurs anywhere in the expressions (WalkVars), closures within
/// them too: asked only of a print call's generated variables not otherwise safe.
fn occurs_in(exprs: &[Expr], v: &str) -> bool {
    exprs
        .iter()
        .any(|e| vars::expr_vars(e, vars::Params::default()).contains(v))
}

/// WalkClosures over an expression with rewritePrintCalls at each closure's body, its
/// globals `p.safe` (and an `every` statement's key and value).
fn print_closures_in_expr(c: &mut Compiler, e: &mut Expr, errs: &mut Vec<CompileError>, p: &mut Prints) {
    if let ExprTerms::Every(ev) = &mut e.terms {
        if let Some(k) = &ev.key {
            p.add(vars::term_vars(k));
        }
        p.add(vars::term_vars(&ev.value));
        print_calls(c, &mut ev.body, errs, p);
        return;
    }
    let mut f = |t: &mut Term| -> bool {
        match &mut t.value {
            TermValue::ArrayCompr(_, b) | TermValue::SetCompr(_, b) | TermValue::ObjectCompr(_, _, b) => {
                print_calls(c, b, errs, p);
                true
            }
            _ => false,
        }
    };
    super::localvars::walk_expr_terms_mut(e, &mut f);
}

/// rewriteExprTerms: calls and refs nested in terms become expressions of their own.
pub fn rewrite_expr_terms(c: &mut Compiler) {
    for_each_rule(c, |c, rule| {
        let mut args = std::mem::take(&mut rule.head.args);
        for a in args.iter_mut() {
            let support = expand_term(c, a);
            append_to_body(&mut rule.body, support);
        }
        rule.head.args = args;
        if let Some(mut k) = rule.head.key.take() {
            let support = expand_term(c, &mut k);
            append_to_body(&mut rule.body, support);
            rule.head.key = Some(k);
        }
        if let Some(mut v) = rule.head.value.take() {
            let support = expand_term(c, &mut v);
            append_to_body(&mut rule.body, support);
            rule.head.value = Some(v);
        }
        let body = std::mem::take(&mut rule.body);
        rule.body = expr_terms_in_body(c, body);
    });
}

/// expandExpr over a body. An `every` statement's body is expanded on a stack of bodies,
/// not in a call within the call expanding the body it is in: policies nest them 99990
/// deep.
fn expr_terms_in_body(c: &mut Compiler, body: Body) -> Body {
    // Each body under way, and the `every` statement it is the body of, with the
    // expressions that go before the statement.
    type Level = (std::vec::IntoIter<Expr>, Body, Option<(Expr, Vec<Expr>)>);
    let mut levels: Vec<Level> = vec![(body.into_iter(), Vec::new(), None)];
    loop {
        if let Some(e) = levels.last_mut().and_then(|(rest, _, _)| rest.next()) {
            match expand_expr(c, e) {
                Expanded::Exprs(xs) => {
                    if let Some((_, cpy, _)) = levels.last_mut() {
                        for x in xs {
                            push(cpy, x);
                        }
                    }
                }
                Expanded::Every(e, body, before) => {
                    levels.push((body.into_iter(), Vec::new(), Some((e, before))))
                }
            }
            continue;
        }
        let Some((_, cpy, every)) = levels.pop() else {
            return Vec::new();
        };
        let Some((mut e, before)) = every else {
            return cpy;
        };
        if let ExprTerms::Every(ev) = &mut e.terms {
            ev.body = cpy.into();
        }
        if let Some((_, parent, _)) = levels.last_mut() {
            for x in before {
                push(parent, x);
            }
            push(parent, e);
        }
    }
}

/// What expandExpr makes of an expression: the expressions it becomes; or, for an
/// `every` statement, the statement, its body to expand, and what goes before it.
enum Expanded {
    Exprs(Vec<Expr>),
    Every(Expr, Body, Vec<Expr>),
}

/// expandExpr.
fn expand_expr(c: &mut Compiler, mut e: Expr) -> Expanded {
    let mut result = Vec::new();
    for w in e.with.iter_mut() {
        let extras = expand_term(c, &mut w.value);
        result.extend(extras);
    }
    let with = e.with.clone();
    match &mut e.terms {
        ExprTerms::Term(t) => {
            let mut extras = expand_term(c, t);
            if !with.is_empty() {
                extras.iter_mut().for_each(|x| x.with = with.clone());
            }
            result.extend(extras);
            result.push(e);
        }
        ExprTerms::Call(terms) => {
            for t in terms.iter_mut().skip(1) {
                let mut extras = expand_term(c, t);
                if !with.is_empty() {
                    extras.iter_mut().for_each(|x| x.with = with.clone());
                }
                result.extend(extras);
            }
            result.push(e);
        }
        ExprTerms::Every(ev) => {
            let loc = ev.domain.loc.clone();
            let term = Term::new(TermValue::Var(c.vargen().generate()), loc.clone());
            let domain = std::mem::replace(&mut ev.domain, term.clone());
            let mut eq = Expr::new(
                ExprTerms::Call(vec![super::localvars::op("eq"), term, domain]),
                loc,
            );
            eq.generated = true;
            eq.with = with;
            if let Expanded::Exprs(extras) = expand_expr(c, eq) {
                result.extend(extras);
            }
            let body = std::mem::take(&mut ev.body).into_inner();
            return Expanded::Every(e, body, result);
        }
        ExprTerms::Some(_) => result.push(e),
    }
    Expanded::Exprs(result)
}

/// expandExprTerm: the expressions a term needs first, the term rewritten in place.
fn expand_term(c: &mut Compiler, t: &mut Term) -> Vec<Expr> {
    let loc = t.loc.clone();
    let mut support = Vec::new();
    match &mut t.value {
        TermValue::Call(cl) => {
            for x in cl.iter_mut().skip(1) {
                support.extend(expand_term(c, x));
            }
            let out = Term::new(TermValue::Var(c.vargen().generate()), loc.clone());
            let mut terms = cl.to_vec();
            terms.push(out.clone());
            let mut e = Expr::new(ExprTerms::Call(terms), loc);
            e.generated = true;
            support.push(e);
            *t = out;
        }
        TermValue::Ref(r) => {
            for x in r.iter_mut() {
                support.extend(expand_term(c, x));
            }
            if let Some(subject) = r.first()
                && matches!(
                    subject.value,
                    TermValue::Array(_)
                        | TermValue::Object(_)
                        | TermValue::Set(_)
                        | TermValue::ArrayCompr(..)
                        | TermValue::SetCompr(..)
                        | TermValue::ObjectCompr(..)
                        | TermValue::Call(_)
                )
            {
                let e = generate(c, subject.clone());
                if let Some(first) = r.first_mut() {
                    *first = operand0(&e);
                }
                support.push(e);
            }
        }
        TermValue::Array(a) => {
            for x in a.iter_mut() {
                support.extend(expand_term(c, x));
            }
        }
        TermValue::Object(o) => {
            let mut pairs: Vec<(Term, Term)> = vars::sorted_pairs(o).into_iter().cloned().collect();
            for (k, v) in pairs.iter_mut() {
                support.extend(expand_term(c, k));
                support.extend(expand_term(c, v));
            }
            *o = pairs.into();
        }
        TermValue::Set(s) => {
            let mut items: Vec<Term> = vars::sorted_items(s).into_iter().cloned().collect();
            for x in items.iter_mut() {
                support.extend(expand_term(c, x));
            }
            *s = items.into();
        }
        TermValue::ArrayCompr(x, b) | TermValue::SetCompr(x, b) => {
            let s = expand_term(c, x);
            let mut body = std::mem::take(b).into_inner();
            append_to_body(&mut body, s);
            *b = expr_terms_in_body(c, body).into();
        }
        TermValue::ObjectCompr(k, v, b) => {
            let s = expand_term(c, k);
            append_to_body(b, s);
            let s = expand_term(c, v);
            let mut body = std::mem::take(b).into_inner();
            append_to_body(&mut body, s);
            *b = expr_terms_in_body(c, body).into();
        }
        _ => {}
    }
    support
}

/// requiresEval: a term holding a ref or a comprehension.
fn requires_eval(t: &Term) -> bool {
    let mut found = false;
    safety::walk_terms(t, &mut |x| {
        if matches!(
            x.value,
            TermValue::Ref(_)
                | TermValue::ArrayCompr(..)
                | TermValue::SetCompr(..)
                | TermValue::ObjectCompr(..)
        ) {
            found = true;
        }
        found
    });
    found
}

struct Comprehensions<'a>(&'a mut Compiler);

impl Transformer for Comprehensions<'_> {
    fn term(&mut self, t: &mut Term) {
        match &mut t.value {
            TermValue::ArrayCompr(x, b) | TermValue::SetCompr(x, b) => {
                if requires_eval(x) {
                    let e = generate(self.0, (**x).clone());
                    **x = operand0(&e);
                    append_to_body(b, vec![e]);
                }
            }
            TermValue::ObjectCompr(k, v, b) => {
                if requires_eval(k) {
                    let e = generate(self.0, (**k).clone());
                    **k = operand0(&e);
                    append_to_body(b, vec![e]);
                }
                if requires_eval(v) {
                    let e = generate(self.0, (**v).clone());
                    **v = operand0(&e);
                    append_to_body(b, vec![e]);
                }
            }
            _ => {}
        }
    }
}

/// rewriteComprehensionTerms.
pub fn rewrite_comprehension_terms(c: &mut Compiler) {
    let names: Vec<String> = c.modules.keys().cloned().collect();
    for name in names {
        let count = c.modules.get(&name).map_or(0, |m| m.rules.len());
        for i in 0..count {
            let Some(mut r) = c.modules.get(&name).and_then(|m| m.rules.get(i)).cloned() else {
                continue;
            };
            transform::rule(&mut Comprehensions(c), &mut r);
            if let Some(slot) = c.modules.get_mut(&name).and_then(|m| m.rules.get_mut(i)) {
                *slot = r;
            }
        }
    }
}

/// rewriteRefsInHead.
pub fn rewrite_refs_in_head(c: &mut Compiler) {
    for_each_rule(c, |c, rule| {
        if let Some(k) = rule.head.key.clone()
            && requires_eval(&k)
        {
            let e = generate(c, k);
            rule.head.key = Some(operand0(&e));
            append_to_body(&mut rule.body, vec![e]);
        }
        if let Some(v) = rule.head.value.clone()
            && requires_eval(&v)
        {
            let e = generate(c, v);
            rule.head.value = Some(operand0(&e));
            append_to_body(&mut rule.body, vec![e]);
        }
        for i in 0..rule.head.args.len() {
            if let Some(a) = rule.head.args.get(i).cloned()
                && requires_eval(&a)
            {
                let e = generate(c, a);
                if let Some(slot) = rule.head.args.get_mut(i) {
                    *slot = operand0(&e);
                }
                append_to_body(&mut rule.body, vec![e]);
            }
        }
    });
}

struct Withs<'a> {
    c: &'a mut Compiler,
    errs: Vec<CompileError>,
}

impl Transformer for Withs<'_> {
    fn body(&mut self, b: &mut Body) {
        let mut result = Vec::new();
        for mut e in std::mem::take(b) {
            let mut extra = Vec::new();
            for i in 0..e.with.len() {
                match validate_with(self.c, &mut e, i) {
                    Err(err) => {
                        self.errs.push(*err);
                        continue;
                    }
                    Ok(false) => {}
                    Ok(true) => {
                        let Some(w) = e.with.get_mut(i) else { continue };
                        let eq = generate(self.c, w.value.clone());
                        w.value = operand0(&eq);
                        extra.push(eq);
                    }
                }
            }
            for x in extra {
                push(&mut result, x);
            }
            push(&mut result, e);
        }
        *b = result;
    }
}

fn has_prefix_var(r: &[Term], root: &str) -> bool {
    r.first().and_then(Term::as_var) == Some(root)
}

/// validateWith: whether the with's value needs evaluating first, or why it is wrong.
fn validate_with(c: &Compiler, e: &mut Expr, i: usize) -> Result<bool, Box<CompileError>> {
    let Some(w) = e.with.get_mut(i) else {
        return Ok(false);
    };
    if let Some(v) = w.value.as_var()
        && super::allowed(v).is_some()
    {
        let v = v.to_string();
        w.value.value = TermValue::Ref(vec![Term::var(&v, None)].into());
    }
    let target_name = match &w.target.value {
        TermValue::Ref(r) => Some(text_of_ref(r)),
        TermValue::Var(v) => Some(v.to_string()),
        _ => None,
    };
    let is_builtin = target_name
        .as_deref()
        .is_some_and(|n| c.builtin_decl(n).is_some());
    let tree: &RuleTree = &c.tree;
    match &w.target.value {
        TermValue::Ref(r) if has_prefix_var(r, "data") => {
            for k in 1..r.len() {
                let prefix = r.get(..k).unwrap_or_default();
                if !tree.has_node(prefix) {
                    break;
                }
                if !tree.exact(prefix).is_empty() {
                    return Err(Box::new(CompileError::compile(
                        w.target.loc.clone(),
                        "with keyword cannot partially replace virtual document(s)".into(),
                    )));
                }
            }
            let target_fns = tree
                .exact(r)
                .iter()
                .filter_map(|id| c.rule(id))
                .any(|x| !x.head.args.is_empty());
            if target_fns {
                if let Some(vr) = w.value.as_ref()
                    && tree.has_node(vr)
                {
                    return Ok(false);
                }
                let name = match &w.value.value {
                    TermValue::Ref(vr) => Some(text_of_ref(vr)),
                    TermValue::Var(v) => Some(v.to_string()),
                    _ => None,
                };
                if name.as_deref().is_some_and(|n| c.builtin_decl(n).is_some()) {
                    return Ok(false);
                }
            }
            if let Some(vr) = w.value.as_ref()
                && tree
                    .exact(vr)
                    .iter()
                    .filter_map(|id| c.rule(id))
                    .any(|x| !x.head.args.is_empty())
            {
                return Ok(false);
            }
        }
        TermValue::Ref(r) if has_prefix_var(r, "input") => {}
        _ if is_builtin => {
            if let Some(v) = w.target.as_var().map(str::to_string) {
                w.target.value = TermValue::Ref(vec![Term::var(&v, None)].into());
            }
            let name = target_name.unwrap_or_default();
            let loc = w.target.loc.clone();
            let bi = crate::builtins::registry().get(&name);
            if matches!(name.as_str(), "eq" | "rego.metadata.chain" | "rego.metadata.rule") {
                return Err(Box::new(CompileError::compile(
                    loc,
                    format!("with keyword replacing built-in function: replacement of {name:?} invalid"),
                )));
            }
            if name.starts_with("internal.") {
                return Err(Box::new(CompileError::compile(
                    loc,
                    format!(
                        "with keyword replacing built-in function: replacement of internal function {name:?} invalid"
                    ),
                )));
            }
            if bi.is_some_and(|b| b.relation) {
                return Err(Box::new(CompileError::compile(
                    loc,
                    "with keyword replacing built-in function: target must not be a relation".into(),
                )));
            }
            if matches!(c.builtin_decl(&name), Some(Type::Function { result: None, .. })) {
                return Err(Box::new(CompileError::compile(
                    loc,
                    "with keyword replacing built-in function: target must not be a void function".into(),
                )));
            }
            if let Some(vr) = w.value.as_ref()
                && tree.has_node(vr)
            {
                return Ok(false);
            }
            let vname = match &w.value.value {
                TermValue::Ref(vr) => Some(text_of_ref(vr)),
                TermValue::Var(v) => Some(v.to_string()),
                _ => None,
            };
            if vname.as_deref().is_some_and(|n| c.builtin_decl(n).is_some()) {
                return Ok(false);
            }
        }
        _ => {
            return Err(Box::new(CompileError::new(
                TYPE_ERR,
                w.target.loc.clone(),
                "with keyword target must reference existing input, data, or a function".into(),
            )));
        }
    }
    Ok(requires_eval(&w.value))
}

/// rewriteWithModifiers.
pub fn rewrite_with_modifiers(c: &mut Compiler) {
    let names: Vec<String> = c.modules.keys().cloned().collect();
    let mut errs = Vec::new();
    for name in names {
        let count = c.modules.get(&name).map_or(0, |m| m.rules.len());
        for i in 0..count {
            let Some(mut r) = c.modules.get(&name).and_then(|m| m.rules.get(i)).cloned() else {
                continue;
            };
            let mut t = Withs { c, errs: Vec::new() };
            transform::rule(&mut t, &mut r);
            errs.extend(t.errs);
            if let Some(slot) = c.modules.get_mut(&name).and_then(|m| m.rules.get_mut(i)) {
                *slot = r;
            }
        }
    }
    c.err(errs);
}

/// headMayHaveVars.
pub fn head_may_have_vars(rule: &Rule) -> bool {
    let h = &rule.head;
    h.args.iter().any(|a| !is_scalar(a))
        || h.key.as_ref().is_some_and(|k| !is_scalar(k))
        || h.value.as_ref().is_some_and(|v| !is_scalar(v))
        || h.ref_path().iter().skip(1).any(|t| !is_scalar(t))
}

/// checkBodySafety: the body reordered for safety, or its errors. On errors OPA keeps
/// the body as written (its closures reordered in place), and the compile ends with
/// them: the body is no one's to read, and here is what the reorder placed of it.
pub fn check_body_safety(c: &mut Compiler, safe: &VarSet, mut body: Body) -> Body {
    let mut unsafe_vars: Unsafe = Vec::new();
    let mut s = Safety {
        c,
        globals: safe.clone(),
        added: Vec::new(),
        occ: safety::Occurrences::default(),
    };
    let closures = safety::closures(&mut body, &mut s.occ);
    let reordered = s.reorder(body, closures, &mut unsafe_vars);
    let errs = safety::errors(&unsafe_vars, &c.rewritten);
    if !errs.is_empty() {
        c.err(errs);
    }
    reordered
}

/// A rule body's safety check under way.
struct Safety<'a> {
    c: &'a Compiler,
    /// The variables a closure may read from the bodies around it (bodySafetyTransformer's
    /// globals): one set that each body adds to and takes its additions back from when
    /// done. OPA copies the set for each body, and a body as deep as n others held n
    /// copies, each as large as the depth.
    globals: VarSet,
    added: Vec<Var>,
    /// Where the rule's variables occur, closures by closure.
    occ: safety::Occurrences,
}

/// A body under the safety check: its expressions in the order the reorder placed them,
/// those not yet transformed, and what the check found.
struct Level {
    rest: std::vec::IntoIter<(Expr, Vec<safety::Closure>)>,
    out: Body,
    unplaced: Unsafe,
    extra: Unsafe,
    /// The length of `Safety::added` when the body began.
    mark: usize,
    /// The expression whose closures' bodies are under way.
    current: Option<Current>,
}

/// An expression whose closures' bodies are checked in turn, each taken out of it and
/// put back when done.
struct Current {
    expr: Expr,
    loc: Option<Location>,
    pending: std::vec::IntoIter<Pending>,
    done: Vec<Body>,
    /// The variables a closure's head reads that neither its body nor the globals bind.
    add: VarSet,
}

/// A closure's body yet to check: the variables of its head, and its own closures.
struct Pending {
    tv: VarSet,
    body: Body,
    closures: safety::Closures,
}

impl Current {
    /// The expression, its closures' bodies back in it, in the order they were taken.
    fn finish(mut self) -> (Expr, Option<Location>, VarSet) {
        let mut done = self.done.into_iter();
        if let ExprTerms::Every(ev) = &mut self.expr.terms {
            if let Some(b) = done.next() {
                ev.body = b.into();
            }
        } else {
            super::localvars::walk_expr_terms_mut(&mut self.expr, &mut |t: &mut Term| -> bool {
                match &mut t.value {
                    TermValue::ArrayCompr(_, b)
                    | TermValue::SetCompr(_, b)
                    | TermValue::ObjectCompr(_, _, b) => {
                        if let Some(d) = done.next() {
                            *b = d.into();
                        }
                        true
                    }
                    _ => false,
                }
            });
        }
        (self.expr, self.loc, self.add)
    }
}

impl Safety<'_> {
    fn add(&mut self, vs: VarSet) {
        for v in vs {
            if self.globals.insert(v.clone()) {
                self.added.push(v);
            }
        }
    }

    /// reorderBodyForSafety with its closure transform; unsafe variables are added to
    /// `out`. The expressions move to their places, closures within them, never copied
    /// (a closure as deep as n others would be copied n times), and a closure's body is
    /// checked on a stack of bodies, not in a call within the call checking the body it
    /// is in: policies nest closures 99990 deep.
    fn reorder(&mut self, body: Body, closures: safety::Closures, out: &mut Unsafe) -> Body {
        let mut levels = vec![self.open(body, closures)];
        loop {
            let Some(top) = levels.last_mut() else {
                return Vec::new();
            };
            if let Some(cur) = top.current.as_mut() {
                // reorderComprehensionSafety for the expression's next closure.
                if let Some(p) = cur.pending.next() {
                    let bv = vars::body_vars(&p.body, SAFETY);
                    for v in &p.tv {
                        if !bv.contains(v) && !self.globals.contains(v) {
                            cur.add.insert(v.clone());
                        }
                    }
                    let level = self.open(p.body, p.closures);
                    levels.push(level);
                    continue;
                }
                if let Some(cur) = top.current.take() {
                    let (e, loc, add) = cur.finish();
                    if !add.is_empty() {
                        top.extra.push((loc, add));
                    }
                    push(&mut top.out, e);
                }
                continue;
            }
            if let Some((e, closures)) = top.rest.next() {
                // The closures of each expression, with the variables of those before it.
                if let Some(prev) = top.out.last() {
                    let vs = vars::expr_vars(prev, SAFETY);
                    self.add(vs);
                }
                let cur = self.start(e, closures);
                if let Some(top) = levels.last_mut() {
                    top.current = Some(cur);
                }
                continue;
            }
            let Some(done) = levels.pop() else {
                return Vec::new();
            };
            for v in self.added.drain(done.mark..) {
                self.globals.remove(&v);
            }
            let mut u = done.unplaced;
            u.extend(done.extra);
            let Some(parent) = levels.last_mut() else {
                out.extend(u);
                return done.out;
            };
            if !u.iter().all(|(_, v)| v.is_empty()) {
                parent.extra.extend(u);
            }
            if let Some(cur) = parent.current.as_mut() {
                cur.done.push(done.out);
            }
        }
    }

    /// reorderBodyForSafety's order for a body, its expressions moved into it.
    fn open(&mut self, body: Body, closures: safety::Closures) -> Level {
        let arity = arity_of(self.c);
        let (order, unsafe_map) = safety::reorder(&arity, &self.globals, &body, &closures, &self.occ);
        let unplaced: Unsafe = unsafe_map
            .iter()
            .map(|(i, vs)| (body.get(*i).and_then(|e| e.loc.clone()), vs.clone()))
            .collect();
        let mut slots: Vec<Option<(Expr, Vec<safety::Closure>)>> =
            body.into_iter().zip(closures.exprs).map(Some).collect();
        let mut reordered = Vec::with_capacity(order.len());
        for i in &order {
            if let Some(e) = slots.get_mut(*i).and_then(Option::take) {
                reordered.push(e);
            }
        }
        Level {
            rest: reordered.into_iter(),
            out: Vec::new(),
            unplaced,
            extra: Vec::new(),
            mark: self.added.len(),
            current: None,
        }
    }

    /// bodySafetyTransformer meeting an expression: its closures' bodies taken out to check
    /// in turn, an `every` statement's key and value among the globals from here on.
    fn start(&mut self, mut e: Expr, closures: Vec<safety::Closure>) -> Current {
        let loc = e.loc.clone();
        let mut closures = closures.into_iter();
        let mut pending = Vec::new();
        if let ExprTerms::Every(ev) = &mut e.terms {
            if let Some(k) = &ev.key {
                self.add(vars::term_vars(k));
            }
            self.add(vars::term_vars(&ev.value));
            pending.push(Pending {
                tv: VarSet::new(),
                body: std::mem::take(&mut ev.body).into_inner(),
                closures: closures.next().map(|c| c.body).unwrap_or_default(),
            });
        } else {
            super::localvars::walk_expr_terms_mut(&mut e, &mut |t: &mut Term| -> bool {
                match &mut t.value {
                    TermValue::ArrayCompr(x, b) | TermValue::SetCompr(x, b) => {
                        pending.push(Pending {
                            tv: vars::term_vars(x),
                            body: std::mem::take(b).into_inner(),
                            closures: closures.next().map(|c| c.body).unwrap_or_default(),
                        });
                        true
                    }
                    TermValue::ObjectCompr(k, v, b) => {
                        let mut tv = vars::term_vars(k);
                        tv.extend(vars::term_vars(v));
                        pending.push(Pending {
                            tv,
                            body: std::mem::take(b).into_inner(),
                            closures: closures.next().map(|c| c.body).unwrap_or_default(),
                        });
                        true
                    }
                    _ => false,
                }
            });
        }
        Current {
            expr: e,
            loc,
            pending: pending.into_iter(),
            done: Vec::new(),
            add: VarSet::new(),
        }
    }
}
/// rewriteEquals: `a == b` as an expression is unification.
pub fn rewrite_equals(c: &mut Compiler) {
    struct Equals;
    impl Transformer for Equals {
        fn body(&mut self, b: &mut Body) {
            for e in b.iter_mut() {
                if let ExprTerms::Call(cl) = &mut e.terms
                    && cl.len() == 3
                    && cl.first().is_some_and(|op| op.to_string() == "equal")
                    && let Some(op) = cl.first_mut()
                {
                    *op = super::localvars::op("eq");
                }
            }
        }
    }
    for m in c.modules.values_mut() {
        for r in m.rules.iter_mut() {
            transform::rule(&mut Equals, r);
        }
    }
}

/// rewriteDynamicTerms.
pub fn rewrite_dynamic_terms(c: &mut Compiler) {
    for_each_rule(c, |c, rule| {
        let body = std::mem::take(&mut rule.body);
        rule.body = dynamics(c, body);
    });
}

fn dynamics(c: &mut Compiler, body: Body) -> Body {
    let mut result: Body = Vec::new();
    for mut e in body {
        let with = e.with.clone();
        if e.is_equality() {
            if let ExprTerms::Call(terms) = &mut e.terms
                && terms.len() == 3
            {
                for i in 1..3 {
                    if let Some(t) = terms.get_mut(i) {
                        dynamics_in_term(c, &with, t, &mut result);
                    }
                }
            }
        } else if e.is_call() {
            if let ExprTerms::Call(terms) = &mut e.terms {
                for t in terms.iter_mut().skip(1) {
                    dynamics_one(c, &with, t, &mut result);
                }
            }
        } else if let ExprTerms::Every(ev) = &mut e.terms {
            dynamics_one(c, &with, &mut ev.domain, &mut result);
            let b = std::mem::take(&mut ev.body).into_inner();
            ev.body = dynamics(c, b).into();
        } else if let ExprTerms::Term(t) = &mut e.terms {
            dynamics_in_term(c, &with, t, &mut result);
        }
        push(&mut result, e);
    }
    result
}

fn dynamics_in_term(c: &mut Compiler, with: &[crate::ast::With], t: &mut Term, result: &mut Body) {
    match &mut t.value {
        TermValue::Ref(r) => {
            for x in r.iter_mut().skip(1) {
                dynamics_one(c, with, x, result);
            }
        }
        TermValue::ArrayCompr(_, b) | TermValue::SetCompr(_, b) | TermValue::ObjectCompr(_, _, b) => {
            let body = std::mem::take(b).into_inner();
            *b = dynamics(c, body).into();
        }
        _ => dynamics_one(c, with, t, result),
    }
}

fn dynamics_one(c: &mut Compiler, with: &[crate::ast::With], t: &mut Term, result: &mut Body) {
    match &mut t.value {
        TermValue::Ref(r) => {
            for x in r.iter_mut().skip(1) {
                dynamics_one(c, with, x, result);
            }
            let mut e = generate(c, t.clone());
            e.with = with.to_vec();
            let v = operand0(&e);
            push(result, e);
            *t = v;
        }
        TermValue::Array(a) => {
            for x in a.iter_mut() {
                dynamics_one(c, with, x, result);
            }
        }
        TermValue::Object(o) => {
            let mut pairs: Vec<(Term, Term)> = vars::sorted_pairs(o).into_iter().cloned().collect();
            for (k, v) in pairs.iter_mut() {
                dynamics_one(c, with, k, result);
                dynamics_one(c, with, v, result);
            }
            *o = pairs.into();
        }
        TermValue::Set(s) => {
            let mut items: Vec<Term> = vars::sorted_items(s).into_iter().cloned().collect();
            for x in items.iter_mut() {
                dynamics_one(c, with, x, result);
            }
            *s = items.into();
        }
        TermValue::ArrayCompr(_, b) | TermValue::SetCompr(_, b) | TermValue::ObjectCompr(_, _, b) => {
            let body = std::mem::take(b).into_inner();
            *b = dynamics(c, body).into();
            let mut e = generate(c, t.clone());
            e.with = with.to_vec();
            let v = operand0(&e);
            push(result, e);
            *t = v;
        }
        _ => {}
    }
}
