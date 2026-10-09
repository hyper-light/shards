//! OPA's safety analysis (ast/compile.go `reorderBodyForSafety`, `outputVarsFor*`,
//! ast/unify.go): which variables an expression binds, the order a body's expressions
//! must run in for every variable to be bound before it is read, and the unsafe ones.

use std::collections::{HashMap, HashSet};

use super::vars::{self, OUTPUT, Params, SAFETY, Var, VarSet, VarVisitor};
use super::{CompileError, UNSAFE_VAR_ERR};
use crate::ast::{Body, Expr, ExprTerms, Location, Term, TermValue};

/// The arity of a function by its ref, None for no function.
pub type Arity<'a> = &'a dyn Fn(&[Term]) -> Option<usize>;

fn walk_vars(t: &Term, params: Params) -> VarSet {
    let mut v = VarVisitor::new(params);
    v.term(t);
    v.vars
}

fn diff_count(a: &VarSet, b: &VarSet) -> usize {
    a.difference(b).count()
}

/// isRefSafe.
pub fn is_ref_safe(r: &[Term], safe: &VarSet) -> bool {
    let Some(head) = r.first() else { return true };
    match &head.value {
        TermValue::Var(v) => safe.contains(v),
        TermValue::Call(_) => diff_count(&walk_vars(head, SAFETY), safe) == 0,
        _ => diff_count(&walk_vars(head, SAFETY), safe) == 0,
    }
}

fn is_call_safe(t: &Term, safe: &VarSet) -> bool {
    diff_count(&walk_vars(t, SAFETY), safe) == 0
}

/// OPA's unifier (ast/unify.go): the variables unifying a and b binds.
struct Unifier<'a> {
    safe: &'a VarSet,
    unified: VarSet,
    unknown: HashMap<Var, VarSet>,
}

impl Unifier<'_> {
    fn is_safe(&self, x: &Var) -> bool {
        self.safe.contains(x) || self.unified.contains(x)
    }

    fn mark_safe(&mut self, x: &Var) {
        self.unified.insert(x.clone());
        if let Some(vs) = self.unknown.remove(x) {
            for v in vs {
                self.mark_safe(&v);
            }
        }
        let keys: Vec<Var> = self.unknown.keys().cloned().collect();
        for v in keys {
            let empty = match self.unknown.get_mut(&v) {
                Some(deps) => deps.remove(x) && deps.is_empty(),
                None => false,
            };
            if empty {
                self.mark_safe(&v);
            }
        }
    }

    fn mark_unknown(&mut self, a: &Var, b: &Var) {
        self.unknown.entry(a.clone()).or_default().insert(b.clone());
    }

    fn mark_all_safe(&mut self, t: &Term) {
        let p = Params {
            skip_ref_head: true,
            skip_object_keys: true,
            skip_closures: true,
            ..Params::default()
        };
        for v in walk_vars(t, p) {
            self.mark_safe(&v);
        }
    }

    fn unify_all(&mut self, a: &Var, b: &Term) {
        if self.is_safe(a) {
            self.mark_all_safe(b);
        } else {
            let p = Params {
                skip_ref_head: true,
                skip_object_keys: true,
                skip_closures: true,
                ..Params::default()
            };
            let vs = walk_vars(b, p);
            let unsafe_count = vs
                .iter()
                .filter(|v| !self.safe.contains(*v) && !self.unified.contains(*v))
                .count();
            if unsafe_count == 0 {
                self.mark_safe(a);
            } else {
                let unsafe_vars: Vec<Var> = vs
                    .iter()
                    .filter(|v| !self.safe.contains(*v) && !self.unified.contains(*v))
                    .cloned()
                    .collect();
                for v in &unsafe_vars {
                    self.mark_unknown(a, v);
                }
            }
        }
    }

    fn unify(&mut self, a: &Term, b: &Term) {
        match &a.value {
            TermValue::Var(av) => match &b.value {
                TermValue::Var(bv) => {
                    if self.is_safe(bv) {
                        self.mark_safe(av);
                    } else if self.is_safe(av) {
                        self.mark_safe(bv);
                    } else {
                        self.mark_unknown(av, bv);
                        self.mark_unknown(bv, av);
                    }
                }
                TermValue::Array(_) | TermValue::Object(_) => self.unify_all(av, b),
                TermValue::Ref(r) => {
                    if is_ref_safe(r, self.safe) {
                        self.mark_safe(av);
                    }
                }
                TermValue::Call(_) => {
                    if is_call_safe(b, self.safe) {
                        self.mark_safe(av);
                    }
                }
                _ => self.mark_safe(av),
            },
            TermValue::Ref(r) => {
                if is_ref_safe(r, self.safe) {
                    match &b.value {
                        TermValue::Var(bv) => self.mark_safe(bv),
                        TermValue::Array(_) | TermValue::Object(_) => self.mark_all_safe(b),
                        _ => {}
                    }
                }
            }
            TermValue::Call(_) => {
                if is_call_safe(a, self.safe) {
                    match &b.value {
                        TermValue::Var(bv) => self.mark_safe(bv),
                        TermValue::Array(_) | TermValue::Object(_) => self.mark_all_safe(b),
                        _ => {}
                    }
                }
            }
            TermValue::ArrayCompr(..) => match &b.value {
                TermValue::Var(bv) => self.mark_safe(bv),
                TermValue::Array(_) => self.mark_all_safe(b),
                _ => {}
            },
            TermValue::ObjectCompr(..) => match &b.value {
                TermValue::Var(bv) => self.mark_safe(bv),
                TermValue::Object(_) => self.mark_all_safe(b),
                _ => {}
            },
            TermValue::SetCompr(..) => {
                if let TermValue::Var(bv) = &b.value {
                    self.mark_safe(bv);
                }
            }
            TermValue::Array(aa) => match &b.value {
                TermValue::Var(bv) => self.unify_all(bv, a),
                TermValue::ArrayCompr(..) | TermValue::ObjectCompr(..) | TermValue::SetCompr(..) => {
                    self.mark_all_safe(a)
                }
                TermValue::Ref(r) => {
                    if is_ref_safe(r, self.safe) {
                        self.mark_all_safe(a);
                    }
                }
                TermValue::Call(_) => {
                    if is_call_safe(b, self.safe) {
                        self.mark_all_safe(a);
                    }
                }
                TermValue::Array(ba) if aa.len() == ba.len() => {
                    for (x, y) in aa.iter().zip(ba) {
                        self.unify(x, y);
                    }
                }
                _ => {}
            },
            TermValue::Object(ao) => match &b.value {
                TermValue::Var(bv) => self.unify_all(bv, a),
                TermValue::Ref(r) => {
                    if is_ref_safe(r, self.safe) {
                        self.mark_all_safe(a);
                    }
                }
                TermValue::Call(_) => {
                    if is_call_safe(b, self.safe) {
                        self.mark_all_safe(a);
                    }
                }
                TermValue::Object(bo) if ao.len() == bo.len() => {
                    for (k, v) in vars::sorted_pairs(ao) {
                        if let Some((_, v2)) = bo.iter().find(|(k2, _)| k2.equal(k)) {
                            self.unify(v, v2);
                        }
                    }
                }
                _ => {}
            },
            _ => {
                if let TermValue::Var(bv) = &b.value {
                    self.mark_safe(bv);
                }
            }
        }
    }
}

/// ast.Unify.
pub fn unify(safe: &VarSet, a: &Term, b: &Term) -> VarSet {
    let mut u = Unifier {
        safe,
        unified: VarSet::new(),
        unknown: HashMap::new(),
    };
    u.unify(a, b);
    u.unified
}

/// outputVarsForTerms: the variables of refs that are safe to evaluate.
fn output_vars_for_terms(e: &Expr, safe: &VarSet) -> VarSet {
    let mut out = VarSet::new();
    let mut f = |t: &Term| -> bool {
        match &t.value {
            TermValue::SetCompr(..)
            | TermValue::ArrayCompr(..)
            | TermValue::ObjectCompr(..)
            | TermValue::TemplateString { .. } => true,
            TermValue::Ref(r) => {
                if !is_ref_safe(r, safe) {
                    return true;
                }
                if !t.is_ground() {
                    let mut v = VarVisitor::new(Params {
                        skip_ref_head: true,
                        ..Params::default()
                    });
                    v.reference(r);
                    out.extend(v.vars);
                }
                false
            }
            _ => false,
        }
    };
    walk_terms_expr(e, &mut f);
    out
}

fn output_vars_for_term(t: &Term, safe: &VarSet) -> VarSet {
    output_vars_for_terms(&Expr::term(t.clone()), safe)
}

/// WalkTerms over an expression (GenericVisitor order, read-only).
pub fn walk_terms_expr(e: &Expr, f: &mut dyn FnMut(&Term) -> bool) {
    match &e.terms {
        ExprTerms::Term(t) => walk_terms(t, f),
        ExprTerms::Call(c) => c.iter().for_each(|t| walk_terms(t, f)),
        ExprTerms::Some(d) => d.symbols.iter().for_each(|t| walk_terms(t, f)),
        ExprTerms::Every(ev) => {
            if let Some(k) = &ev.key {
                walk_terms(k, f);
            }
            walk_terms(&ev.value, f);
            walk_terms(&ev.domain, f);
            ev.body.iter().for_each(|x| walk_terms_expr(x, f));
        }
    }
    for w in &e.with {
        walk_terms(&w.target, f);
        walk_terms(&w.value, f);
    }
}

pub fn walk_terms(t: &Term, f: &mut dyn FnMut(&Term) -> bool) {
    if f(t) {
        return;
    }
    match &t.value {
        TermValue::Ref(r) | TermValue::Array(r) | TermValue::Call(r) => {
            r.iter().for_each(|x| walk_terms(x, f))
        }
        TermValue::Object(o) => {
            for (k, v) in vars::sorted_pairs(o) {
                walk_terms(k, f);
                walk_terms(v, f);
            }
        }
        TermValue::Set(s) => vars::sorted_items(s).into_iter().for_each(|x| walk_terms(x, f)),
        TermValue::ArrayCompr(x, b) | TermValue::SetCompr(x, b) => {
            walk_terms(x, f);
            b.iter().for_each(|e| walk_terms_expr(e, f));
        }
        TermValue::ObjectCompr(k, v, b) => {
            walk_terms(k, f);
            walk_terms(v, f);
            b.iter().for_each(|e| walk_terms_expr(e, f));
        }
        TermValue::TemplateString { parts, .. } => {
            for p in parts {
                match p {
                    crate::ast::TemplatePart::Term(t) => walk_terms(t, f),
                    crate::ast::TemplatePart::Expr(e) => walk_terms_expr(e, f),
                }
            }
        }
        _ => {}
    }
}

/// outputVarsForExprEq.
pub fn output_vars_for_expr_eq(e: &Expr, safe: &VarSet) -> VarSet {
    let (Some(a), Some(b)) = (e.operand(0), e.operand(1)) else {
        return safe.clone();
    };
    if e.operand(2).is_some() {
        return safe.clone();
    }
    let mut output = output_vars_for_terms(e, safe);
    output.extend(safe.iter().cloned());
    let u = unify(&output, a, b);
    output.extend(u);
    output.difference(safe).cloned().collect()
}

fn output_vars_for_expr_call(e: &Expr, arity: usize, safe: &VarSet, terms: &[Term]) -> VarSet {
    let mut output = output_vars_for_terms(e, safe);
    let inputs = arity + 1;
    if inputs >= terms.len() {
        return output;
    }
    let mut v = VarVisitor::new(OUTPUT);
    v.args(terms.get(..inputs).unwrap_or_default());
    let unsafe_count = v
        .vars
        .iter()
        .filter(|x| !output.contains(*x) && !safe.contains(*x))
        .count();
    if unsafe_count > 0 {
        return VarSet::new();
    }
    let mut v = VarVisitor::new(OUTPUT);
    v.args(terms.get(inputs..).unwrap_or_default());
    output.extend(v.vars);
    output
}

/// outputVarsForExpr.
pub fn output_vars_for_expr(e: &Expr, arity: Arity<'_>, safe: &VarSet) -> VarSet {
    if e.negated {
        return VarSet::new();
    }
    for w in &e.with {
        let mut v = VarVisitor::new(SAFETY);
        v.with(w);
        if diff_count(&v.vars, safe) > 0 {
            return VarSet::new();
        }
    }
    match &e.terms {
        ExprTerms::Term(t) => {
            if matches!(t.value, TermValue::TemplateString { .. }) {
                return VarSet::new();
            }
            output_vars_for_terms(e, safe)
        }
        ExprTerms::Call(terms) => {
            if e.is_equality() {
                return output_vars_for_expr_eq(e, safe);
            }
            let Some(op) = terms.first().and_then(Term::as_ref) else {
                return VarSet::new();
            };
            let Some(a) = arity(op) else { return VarSet::new() };
            output_vars_for_expr_call(e, a, safe, terms)
        }
        ExprTerms::Every(ev) => output_vars_for_term(&ev.domain, safe),
        ExprTerms::Some(_) => VarSet::new(),
    }
}

/// outputVarsForBody.
pub fn output_vars_for_body(body: &[Expr], arity: Arity<'_>, safe: &VarSet) -> VarSet {
    let mut o = safe.clone();
    for e in body {
        let out = output_vars_for_expr(e, arity, &o);
        o.extend(out);
    }
    o.difference(safe).cloned().collect()
}

fn unsafe_add(u: &mut HashMap<usize, VarSet>, i: usize, v: &Var) {
    u.entry(i).or_default().insert(v.clone());
}

/// unsafeVarsInClosures: the variables the closures of an expression read, an `every`'s
/// body included.
fn vars_in_closures(e: &Expr) -> VarSet {
    if let ExprTerms::Every(ev) = &e.terms {
        return vars::body_vars(&ev.body, Params::default());
    }
    let mut out = VarSet::new();
    walk_terms_expr(e, &mut |t: &Term| match &t.value {
        TermValue::ArrayCompr(..) | TermValue::SetCompr(..) | TermValue::ObjectCompr(..) => {
            out.extend(vars::term_vars(t));
            true
        }
        _ => false,
    });
    out
}

/// reorderBodyForSafety, without the closure transform (applied by the caller).
pub fn reorder(arity: Arity<'_>, globals: &VarSet, body: &Body) -> (Vec<usize>, HashMap<usize, VarSet>) {
    let mut vis = VarVisitor::new(SAFETY);
    vis.body(body);
    let body_vars = vis.vars;
    let mut safe: VarSet = body_vars.intersection(globals).cloned().collect();
    let mut unsafe_vars: HashMap<usize, VarSet> = HashMap::new();
    for (i, e) in body.iter().enumerate() {
        for v in vars::expr_vars(e, SAFETY) {
            if !safe.contains(&v) {
                unsafe_add(&mut unsafe_vars, i, &v);
            }
        }
    }
    let mut order: Vec<usize> = Vec::new();
    let mut placed: HashSet<usize> = HashSet::new();
    loop {
        let n = order.len();
        for (i, e) in body.iter().enumerate() {
            if placed.contains(&i) {
                continue;
            }
            let ovs = output_vars_for_expr(e, arity, &safe);
            let cv: VarSet = vars_in_closures(e)
                .intersection(&body_vars)
                .filter(|v| !globals.contains(*v))
                .cloned()
                .collect();
            let reordered: Body = order.iter().filter_map(|&j| body.get(j).cloned()).collect();
            let ob = output_vars_for_body(&reordered, arity, &safe);
            if diff_count(&cv, &ob) > 0 {
                let uv: VarSet = cv.difference(&ob).cloned().collect();
                if uv == ovs {
                    continue;
                }
                unsafe_vars.insert(i, uv);
            }
            if let Some(u) = unsafe_vars.get_mut(&i) {
                u.retain(|v| !ovs.contains(v) && !safe.contains(v));
            }
            if unsafe_vars.get(&i).is_none_or(VarSet::is_empty) {
                unsafe_vars.remove(&i);
                order.push(i);
                placed.insert(i);
                safe.extend(ovs);
            }
        }
        if order.len() == n {
            break;
        }
    }
    (order, unsafe_vars)
}

/// Unsafe variables, each with the location of the expression they were found in.
pub type Unsafe = Vec<(Option<Location>, VarSet)>;

/// safetyErrorSlice: the unsafe variables' errors, by their first locations.
pub fn errors(unsafe_vars: &Unsafe, rewritten: &HashMap<Var, Var>) -> Vec<CompileError> {
    let unsafe_vars: Vec<&(Option<Location>, VarSet)> =
        unsafe_vars.iter().filter(|(_, v)| !v.is_empty()).collect();
    if unsafe_vars.is_empty() {
        return Vec::new();
    }
    // Each variable at its earliest location.
    let mut locs: HashMap<Var, Option<Location>> = HashMap::new();
    for (l, vs) in &unsafe_vars {
        for v in vs {
            match locs.get(v) {
                Some(prev) if compare_loc(prev, l) <= 0 => {}
                _ => {
                    locs.insert(v.clone(), l.clone());
                }
            }
        }
    }
    let mut pairs: Vec<(Var, Option<Location>)> = locs.into_iter().collect();
    pairs.sort_by(|a, b| compare_loc(&a.1, &b.1).cmp(&0).then_with(|| a.0.cmp(&b.0)));
    let mut out = Vec::new();
    for (v, loc) in pairs {
        let v = rewritten.get(&v).cloned().unwrap_or(v);
        if vars::is_generated(&v) {
            continue;
        }
        if matches!(&*v, "in" | "every" | "contains" | "if") {
            out.push(CompileError::new(
                UNSAFE_VAR_ERR,
                loc,
                format!("var {v} is unsafe (hint: `import future.keywords.{v}` to import a future keyword)"),
            ));
            continue;
        }
        out.push(CompileError::new(
            UNSAFE_VAR_ERR,
            loc,
            format!("var {v} is unsafe"),
        ));
    }
    if !out.is_empty() {
        return out;
    }
    let mut exprs = unsafe_vars.clone();
    exprs.sort_by(|a, b| compare_loc(&a.0, &b.0).cmp(&0));
    let mut seen = VarSet::new();
    for (l, vs) in exprs {
        let before = seen.len();
        seen.extend(vs.iter().filter(|v| vars::is_generated(v)).cloned());
        if seen.len() > before {
            out.push(CompileError::new(
                UNSAFE_VAR_ERR,
                l.clone(),
                "expression is unsafe".into(),
            ));
        }
    }
    out
}

/// Location.Compare: by file, row and column, nil last.
pub fn compare_loc(a: &Option<Location>, b: &Option<Location>) -> i32 {
    match (a, b) {
        (None, None) => 0,
        (None, Some(_)) => 1,
        (Some(_), None) => -1,
        (Some(a), Some(b)) => {
            let c = a
                .file
                .cmp(&b.file)
                .then(a.row.cmp(&b.row))
                .then(a.col.cmp(&b.col));
            match c {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }
        }
    }
}
