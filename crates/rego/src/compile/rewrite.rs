//! OPA's rewriting stages (ast/compile.go): each turns some construct into the plain
//! unifications and calls the evaluator runs, generating `__localN__` variables in OPA's
//! order.

use super::localvars::{is_scalar, push};
use super::safety::{self, Unsafe};
use super::transform::{self, Transformer};
use super::vars::{self, Var, VarSet, VarVisitor, SAFETY};
use super::{Compiler, CompileError, RuleTree, TYPE_ERR, is_empty_body, text_of_ref};
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
            let Some(mut rule) = c.modules.get(&name).and_then(|m| m.rules.get(i)).cloned() else { continue };
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
            let Some(part) = rule.head.reference.get(i).cloned() else { continue };
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
        Some(TermValue::String(_) | TermValue::Var(_) | TermValue::Number(_) | TermValue::Bool(_) | TermValue::Null) => true,
        Some(TermValue::Array(_) | TermValue::Object(_) | TermValue::Set(_)) => rule.head.value.as_ref().is_some_and(Term::is_ground),
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
pub fn rewrite_template_strings(c: &mut Compiler) {
    let mut errs = Vec::new();
    for_each_rule(c, |c, rule| {
        let mut safe = VarSet::new();
        for a in &rule.head.args {
            safe.extend(vars::term_vars(a));
        }
        safe.insert("data".into());
        safe.insert("input".into());
        let body_safe = template_strings_in_body(c, &safe, &mut rule.body, &mut errs);
        let mut ts = TemplateWalk { c, safe: body_safe, errs: &mut errs };
        for a in rule.head.args.iter_mut() {
            ts.term(a);
        }
        if let Some(k) = rule.head.key.as_mut() {
            ts.term(k);
        }
        if let Some(v) = rule.head.value.as_mut() {
            ts.term(v);
        }
    });
    c.err(errs);
}

/// rewriteTemplateStrings over a body: returns the variables safe after it.
fn template_strings_in_body(c: &mut Compiler, globals: &VarSet, body: &mut Body, errs: &mut Vec<CompileError>) -> VarSet {
    let mut safe = {
        let arity = arity_of(c);
        safety::output_vars_for_body(body, &arity, globals)
    };
    safe.extend(globals.iter().cloned());
    let mut ts = TemplateWalk { c, safe: safe.clone(), errs };
    for e in body.iter_mut() {
        ts.expr(e);
    }
    safe
}

struct TemplateWalk<'a> {
    c: &'a mut Compiler,
    safe: VarSet,
    errs: &'a mut Vec<CompileError>,
}

impl TemplateWalk<'_> {
    fn expr(&mut self, e: &mut Expr) {
        match &mut e.terms {
            ExprTerms::Term(t) => self.term(t),
            ExprTerms::Call(cl) => cl.iter_mut().for_each(|t| self.term(t)),
            ExprTerms::Some(d) => d.symbols.iter_mut().for_each(|t| self.term(t)),
            ExprTerms::Every(ev) => {
                rewrite_template_term(self.c, &self.safe, &mut ev.domain, self.errs);
                let mut s = self.safe.clone();
                if let Some(k) = &ev.key {
                    s.extend(vars::term_vars(k));
                }
                s.extend(vars::term_vars(&ev.value));
                template_strings_in_body(self.c, &s, &mut ev.body, self.errs);
                if let Some(k) = ev.key.as_mut() {
                    self.term(k);
                }
                self.term(&mut ev.value);
                self.term(&mut ev.domain);
                for x in ev.body.iter_mut() {
                    self.expr(x);
                }
            }
        }
        for w in e.with.iter_mut() {
            self.term(&mut w.target);
            self.term(&mut w.value);
        }
    }

    fn term(&mut self, t: &mut Term) {
        match &mut t.value {
            TermValue::TemplateString { .. } => {
                rewrite_template_term(self.c, &self.safe, t, self.errs);
            }
            TermValue::SetCompr(x, b) | TermValue::ArrayCompr(x, b) => {
                let s = template_strings_in_body(self.c, &self.safe, b, self.errs);
                rewrite_template_term(self.c, &s, x, self.errs);
            }
            TermValue::ObjectCompr(k, v, b) => {
                let s = template_strings_in_body(self.c, &self.safe, b, self.errs);
                rewrite_template_term(self.c, &s, k, self.errs);
                rewrite_template_term(self.c, &s, v, self.errs);
            }
            _ => {}
        }
        match &mut t.value {
            TermValue::Ref(r) | TermValue::Array(r) | TermValue::Call(r) => r.iter_mut().for_each(|x| self.term(x)),
            TermValue::Object(o) => {
                for (k, v) in o.iter_mut() {
                    self.term(k);
                    self.term(v);
                }
            }
            TermValue::Set(s) => s.iter_mut().for_each(|x| self.term(x)),
            TermValue::SetCompr(x, b) | TermValue::ArrayCompr(x, b) => {
                self.term(x);
                b.iter_mut().for_each(|e| self.expr(e));
            }
            TermValue::ObjectCompr(k, v, b) => {
                self.term(k);
                self.term(v);
                b.iter_mut().for_each(|e| self.expr(e));
            }
            _ => {}
        }
    }
}

/// rewriteTemplateStringTerm and rewriteTemplateString.
fn rewrite_template_term(c: &mut Compiler, safe: &VarSet, t: &mut Term, errs: &mut Vec<CompileError>) {
    let TermValue::TemplateString { parts, .. } = &t.value else { return };
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
                        Term::new(TermValue::Call(cl.clone()), e.loc.clone())
                    }
                    ExprTerms::Term(x) => (**x).clone(),
                    _ => {
                        local_errs.push(CompileError::compile(e.loc.clone(), "unexpected template-string expression type".into()));
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
                    local_errs.push(CompileError::compile(term.loc.clone(), format!("var {v} is undeclared")));
                }
                let l = term.loc.clone();
                let x = Term::new(TermValue::Var(c.vargen().generate()), l.clone());
                let mut capture = Expr::new(ExprTerms::Call(vec![super::localvars::op("eq"), x.clone(), term]), l.clone());
                capture.with = e.with.clone();
                terms.push(Term::new(TermValue::SetCompr(Box::new(x), vec![capture]), l));
            }
            TemplatePart::Term(x) => terms.push(x),
        }
    }
    if !local_errs.is_empty() {
        errs.extend(local_errs);
        return;
    }
    let op = Term::reference(vec![Term::var("internal", None), Term::string("template_string", None)], None);
    t.value = TermValue::Call(vec![op, Term::new(TermValue::Array(terms), loc)]);
}

/// checkVoidCalls: a call to a function without a result used as a value.
pub fn check_void_calls(c: &mut Compiler) {
    let mut errs = Vec::new();
    let mut check = |t: &Term, c: &Compiler| {
        if let TermValue::Call(cl) = &t.value
            && let Some(op) = cl.first().and_then(Term::as_ref)
            && let Some(Type::Function { result: None, .. }) = c.builtin_decl(&text_of_ref(op))
        {
            errs.push(CompileError::new(TYPE_ERR, t.loc.clone(), format!("{t} used as value")));
        }
    };
    let rules: Vec<Rule> = c.modules.values().flat_map(|m| m.rules.iter().cloned()).collect();
    for rule in &rules {
        let mut r = Some(rule);
        while let Some(x) = r {
            let mut terms = Vec::new();
            for t in x.head.args.iter().chain(x.head.key.iter()).chain(x.head.value.iter()) {
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

fn contains_closures(e: &Expr) -> bool {
    let mut found = false;
    safety::walk_terms_expr(e, &mut |t| {
        if matches!(t.value, TermValue::ArrayCompr(..) | TermValue::SetCompr(..) | TermValue::ObjectCompr(..)) {
            found = true;
        }
        found
    });
    found || e.is_every()
}

/// rewritePrintCalls: `print(a, b)` to `internal.print([{x | x = a}, {y | y = b}])`.
pub fn rewrite_print_calls(c: &mut Compiler) {
    if !c.print_enabled() {
        return;
    }
    let mut errs = Vec::new();
    for_each_rule(c, |c, rule| {
        let mut globals: VarSet = ["data", "input"].iter().map(|s| Var::from(*s)).collect();
        for a in &rule.head.args {
            globals.extend(vars::term_vars(a));
        }
        // WalkBodies over the head, then the body: each body met, outermost first.
        let mut head_terms: Vec<&mut Term> = rule.head.args.iter_mut().collect();
        head_terms.extend(rule.head.key.iter_mut());
        head_terms.extend(rule.head.value.iter_mut());
        for t in head_terms {
            print_bodies_in_term(c, &globals, t, &mut errs);
        }
        print_bodies(c, &globals, &mut rule.body, &mut errs);
    });
    c.err(errs);
}

/// WalkBodies with rewritePrintCalls at each body.
fn print_bodies(c: &mut Compiler, globals: &VarSet, body: &mut Body, errs: &mut Vec<CompileError>) {
    print_calls(c, globals, body, errs);
    for e in body.iter_mut() {
        print_bodies_in_expr(c, globals, e, errs);
    }
}

fn print_bodies_in_expr(c: &mut Compiler, globals: &VarSet, e: &mut Expr, errs: &mut Vec<CompileError>) {
    match &mut e.terms {
        ExprTerms::Term(t) => print_bodies_in_term(c, globals, t, errs),
        ExprTerms::Call(cl) => cl.iter_mut().for_each(|t| print_bodies_in_term(c, globals, t, errs)),
        ExprTerms::Some(d) => d.symbols.iter_mut().for_each(|t| print_bodies_in_term(c, globals, t, errs)),
        ExprTerms::Every(ev) => {
            print_bodies_in_term(c, globals, &mut ev.domain, errs);
            print_bodies(c, globals, &mut ev.body, errs);
        }
    }
}

fn print_bodies_in_term(c: &mut Compiler, globals: &VarSet, t: &mut Term, errs: &mut Vec<CompileError>) {
    match &mut t.value {
        TermValue::Ref(r) | TermValue::Array(r) | TermValue::Call(r) | TermValue::Set(r) => {
            r.iter_mut().for_each(|x| print_bodies_in_term(c, globals, x, errs))
        }
        TermValue::Object(o) => {
            for (k, v) in o.iter_mut() {
                print_bodies_in_term(c, globals, k, errs);
                print_bodies_in_term(c, globals, v, errs);
            }
        }
        TermValue::ArrayCompr(x, b) | TermValue::SetCompr(x, b) => {
            print_bodies_in_term(c, globals, x, errs);
            print_bodies(c, globals, b, errs);
        }
        TermValue::ObjectCompr(k, v, b) => {
            print_bodies_in_term(c, globals, k, errs);
            print_bodies_in_term(c, globals, v, errs);
            print_bodies(c, globals, b, errs);
        }
        _ => {}
    }
}

/// rewritePrintCalls on one body.
fn print_calls(c: &mut Compiler, globals: &VarSet, body: &mut Body, errs: &mut Vec<CompileError>) {
    for i in 0..body.len() {
        if !body.get(i).is_some_and(contains_closures) {
            continue;
        }
        let mut safe = {
            let arity = arity_of(c);
            safety::output_vars_for_body(body.get(..i).unwrap_or_default(), &arity, globals)
        };
        safe.extend(globals.iter().cloned());
        let Some(e) = body.get_mut(i) else { continue };
        let mut local = Vec::new();
        print_closures_in_expr(c, &mut safe, e, &mut local);
        if !local.is_empty() {
            errs.extend(local);
            return;
        }
    }
    for i in 0..body.len() {
        if !body.get(i).is_some_and(is_print_call) {
            continue;
        }
        let mut safe = {
            let arity = arity_of(c);
            safety::output_vars_for_body(body.get(..i).unwrap_or_default(), &arity, globals)
        };
        safe.extend(globals.iter().cloned());
        for e in body.get(..i).unwrap_or_default() {
            safe.extend(vars::expr_vars(e, vars::Params::default()).into_iter().filter(|v| vars::is_generated(v)));
        }
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
            for v in vis.vars.difference(&safe) {
                local.push(CompileError::compile(a.loc.clone(), format!("var {v} is undeclared")));
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
            let capture = Expr::new(ExprTerms::Call(vec![super::localvars::op("eq"), x.clone(), a]), l.clone());
            terms.push(Term::new(TermValue::SetCompr(Box::new(x), vec![capture]), l));
        }
        let op = Term::reference(vec![Term::var("internal", loc.clone()), Term::string("print", loc.clone())], loc.clone());
        let mut new = Expr::new(ExprTerms::Call(vec![op, Term::new(TermValue::Array(terms), loc.clone())]), loc);
        new.index = i;
        if let Some(slot) = body.get_mut(i) {
            *slot = new;
        }
    }
}

/// WalkClosures over an expression with rewritePrintCalls at each closure's body.
fn print_closures_in_expr(c: &mut Compiler, safe: &mut VarSet, e: &mut Expr, errs: &mut Vec<CompileError>) {
    if let ExprTerms::Every(ev) = &mut e.terms {
        if let Some(k) = &ev.key {
            safe.extend(vars::term_vars(k));
        }
        safe.extend(vars::term_vars(&ev.value));
        print_calls(c, safe, &mut ev.body, errs);
        return;
    }
    let mut f = |t: &mut Term| -> bool {
        match &mut t.value {
            TermValue::ArrayCompr(_, b) | TermValue::SetCompr(_, b) | TermValue::ObjectCompr(_, _, b) => {
                print_calls(c, safe, b, errs);
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

fn expr_terms_in_body(c: &mut Compiler, body: Body) -> Body {
    let mut cpy = Vec::new();
    for e in body {
        for x in expand_expr(c, e) {
            push(&mut cpy, x);
        }
    }
    cpy
}

/// expandExpr.
fn expand_expr(c: &mut Compiler, mut e: Expr) -> Vec<Expr> {
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
            let mut eq = Expr::new(ExprTerms::Call(vec![super::localvars::op("eq"), term, domain]), loc);
            eq.generated = true;
            eq.with = with;
            let extras = expand_expr(c, eq);
            let body = std::mem::take(&mut ev.body);
            ev.body = expr_terms_in_body(c, body);
            result.extend(extras);
            result.push(e);
        }
        ExprTerms::Some(_) => result.push(e),
    }
    result
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
            let mut terms = cl.clone();
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
                    TermValue::Array(_) | TermValue::Object(_) | TermValue::Set(_) | TermValue::ArrayCompr(..) | TermValue::SetCompr(..) | TermValue::ObjectCompr(..) | TermValue::Call(_)
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
            *o = pairs;
        }
        TermValue::Set(s) => {
            let mut items: Vec<Term> = vars::sorted_items(s).into_iter().cloned().collect();
            for x in items.iter_mut() {
                support.extend(expand_term(c, x));
            }
            *s = items;
        }
        TermValue::ArrayCompr(x, b) | TermValue::SetCompr(x, b) => {
            let s = expand_term(c, x);
            let mut body = std::mem::take(b);
            append_to_body(&mut body, s);
            *b = expr_terms_in_body(c, body);
        }
        TermValue::ObjectCompr(k, v, b) => {
            let s = expand_term(c, k);
            append_to_body(b, s);
            let s = expand_term(c, v);
            let mut body = std::mem::take(b);
            append_to_body(&mut body, s);
            *b = expr_terms_in_body(c, body);
        }
        _ => {}
    }
    support
}

/// requiresEval: a term holding a ref or a comprehension.
fn requires_eval(t: &Term) -> bool {
    let mut found = false;
    safety::walk_terms(t, &mut |x| {
        if matches!(x.value, TermValue::Ref(_) | TermValue::ArrayCompr(..) | TermValue::SetCompr(..) | TermValue::ObjectCompr(..)) {
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
            let Some(mut r) = c.modules.get(&name).and_then(|m| m.rules.get(i)).cloned() else { continue };
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
    let Some(w) = e.with.get_mut(i) else { return Ok(false) };
    if let Some(v) = w.value.as_var()
        && super::allowed(v).is_some()
    {
        let v = v.to_string();
        w.value.value = TermValue::Ref(vec![Term::var(&v, None)]);
    }
    let target_name = match &w.target.value {
        TermValue::Ref(r) => Some(text_of_ref(r)),
        TermValue::Var(v) => Some(v.to_string()),
        _ => None,
    };
    let is_builtin = target_name.as_deref().is_some_and(|n| c.builtin_decl(n).is_some());
    let tree: &RuleTree = &c.tree;
    match &w.target.value {
        TermValue::Ref(r) if has_prefix_var(r, "data") => {
            for k in 1..r.len() {
                let prefix = r.get(..k).unwrap_or_default();
                if !tree.has_node(prefix) {
                    break;
                }
                if !tree.exact(prefix).is_empty() {
                    return Err(Box::new(CompileError::compile(w.target.loc.clone(), "with keyword cannot partially replace virtual document(s)".into())));
                }
            }
            let target_fns = tree.exact(r).iter().filter_map(|id| c.rule(id)).any(|x| !x.head.args.is_empty());
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
                && tree.exact(vr).iter().filter_map(|id| c.rule(id)).any(|x| !x.head.args.is_empty())
            {
                return Ok(false);
            }
        }
        TermValue::Ref(r) if has_prefix_var(r, "input") => {}
        _ if is_builtin => {
            if let Some(v) = w.target.as_var().map(str::to_string) {
                w.target.value = TermValue::Ref(vec![Term::var(&v, None)]);
            }
            let name = target_name.unwrap_or_default();
            let loc = w.target.loc.clone();
            let bi = crate::builtins::registry().get(&name);
            if matches!(name.as_str(), "eq" | "rego.metadata.chain" | "rego.metadata.rule") {
                return Err(Box::new(CompileError::compile(loc, format!("with keyword replacing built-in function: replacement of {name:?} invalid"))));
            }
            if name.starts_with("internal.") {
                return Err(Box::new(CompileError::compile(loc, format!("with keyword replacing built-in function: replacement of internal function {name:?} invalid"))));
            }
            if bi.is_some_and(|b| b.relation) {
                return Err(Box::new(CompileError::compile(loc, "with keyword replacing built-in function: target must not be a relation".into())));
            }
            if matches!(c.builtin_decl(&name), Some(Type::Function { result: None, .. })) {
                return Err(Box::new(CompileError::compile(loc, "with keyword replacing built-in function: target must not be a void function".into())));
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
            return Err(Box::new(CompileError::new(TYPE_ERR, w.target.loc.clone(), "with keyword target must reference existing input, data, or a function".into())));
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
            let Some(mut r) = c.modules.get(&name).and_then(|m| m.rules.get(i)).cloned() else { continue };
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

/// checkBodySafety: the body reordered for safety, or its errors.
pub fn check_body_safety(c: &mut Compiler, safe: &VarSet, body: Body) -> Body {
    let mut unsafe_vars: Unsafe = Vec::new();
    let reordered = reorder_for_safety(c, safe, &body, &mut unsafe_vars);
    let errs = safety::errors(&unsafe_vars, &c.rewritten);
    if !errs.is_empty() {
        c.err(errs);
        return body;
    }
    reordered
}

/// reorderBodyForSafety with its closure transform; unsafe variables are added to `out`.
fn reorder_for_safety(c: &Compiler, globals: &VarSet, body: &Body, out: &mut Unsafe) -> Body {
    let arity = arity_of(c);
    let (order, unsafe_map) = safety::reorder(&arity, globals, body);
    let mut reordered: Body = Vec::new();
    for i in &order {
        if let Some(e) = body.get(*i) {
            push(&mut reordered, e.clone());
        }
    }
    // The closures of each expression, with the variables of those before it.
    let mut g = globals.clone();
    let mut extra: Vec<(Option<Location>, VarSet)> = Vec::new();
    for i in 0..reordered.len() {
        if i > 0
            && let Some(prev) = reordered.get(i - 1)
        {
            g.extend(vars::expr_vars(prev, SAFETY));
        }
        let Some(e) = reordered.get_mut(i) else { continue };
        let loc = e.loc.clone();
        let mut add: VarSet = VarSet::new();
        closure_safety_expr(c, &mut g, e, &mut add, &mut extra);
        if !add.is_empty() {
            extra.push((loc, add));
        }
    }
    for (i, vs) in &unsafe_map {
        out.push((body.get(*i).and_then(|e| e.loc.clone()), vs.clone()));
    }
    out.extend(extra);
    reordered
}

/// bodySafetyTransformer over one expression.
fn closure_safety_expr(c: &Compiler, g: &mut VarSet, e: &mut Expr, add: &mut VarSet, nested: &mut Unsafe) {
    if let ExprTerms::Every(ev) = &mut e.terms {
        if let Some(k) = &ev.key {
            g.extend(vars::term_vars(k));
        }
        g.extend(vars::term_vars(&ev.value));
        ev.body = closure_body(c, g, &VarSet::new(), &ev.body, add, nested);
        return;
    }
    let globals = g.clone();
    super::localvars::walk_expr_terms_mut(e, &mut |t: &mut Term| -> bool {
        match &mut t.value {
            TermValue::ArrayCompr(x, b) | TermValue::SetCompr(x, b) => {
                *b = closure_body(c, &globals, &vars::term_vars(x), b, add, nested);
                true
            }
            TermValue::ObjectCompr(k, v, b) => {
                let mut tv = vars::term_vars(k);
                tv.extend(vars::term_vars(v));
                *b = closure_body(c, &globals, &tv, b, add, nested);
                true
            }
            _ => false,
        }
    });
}

/// reorderComprehensionSafety.
fn closure_body(c: &Compiler, globals: &VarSet, tv: &VarSet, body: &Body, add: &mut VarSet, nested: &mut Unsafe) -> Body {
    let mut bv = vars::body_vars(body, SAFETY);
    bv.extend(globals.iter().cloned());
    for v in tv.difference(&bv) {
        add.insert(v.clone());
    }
    let mut u: Unsafe = Vec::new();
    let r = reorder_for_safety(c, globals, body, &mut u);
    if u.iter().all(|(_, v)| v.is_empty()) {
        return r;
    }
    nested.extend(u);
    body.clone()
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
            let b = std::mem::take(&mut ev.body);
            ev.body = dynamics(c, b);
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
            let body = std::mem::take(b);
            *b = dynamics(c, body);
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
            *o = pairs;
        }
        TermValue::Set(s) => {
            let mut items: Vec<Term> = vars::sorted_items(s).into_iter().cloned().collect();
            for x in items.iter_mut() {
                dynamics_one(c, with, x, result);
            }
            *s = items;
        }
        TermValue::ArrayCompr(_, b) | TermValue::SetCompr(_, b) | TermValue::ObjectCompr(_, _, b) => {
            let body = std::mem::take(b);
            *b = dynamics(c, body);
            let mut e = generate(c, t.clone());
            e.with = with.to_vec();
            let v = operand0(&e);
            push(result, e);
            *t = v;
        }
        _ => {}
    }
}
