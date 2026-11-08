//! OPA's rewriting of local variables (ast/compile.go `rewriteLocalVars` and its helpers):
//! every variable a rule declares (`:=`, `some`, `every`, arguments) is renamed to a
//! fresh `__localN__`, numbered as OPA numbers them, and misuses are reported in OPA's
//! words.

use std::collections::HashMap;
use std::rc::Rc;

use super::vars::{self, Var, VarSet, sorted_items, sorted_pairs};
use super::{COMPILE_ERR, CompileError};
use crate::ast::{Body, Expr, ExprTerms, Location, Rule, Shared, TemplatePart, Term, TermValue};

/// localVarGenerator: `__local<n>__`, skipping names the modules use. Its `n` is Go's
/// `int`, so past `i64::MAX` it goes on from `i64::MIN`, as OPA's would; the names the
/// modules use are skipped from the numbers not negative (a module's `__local-1__` matters
/// only after 2^63 names, which OPA would not live to generate).
#[derive(Debug, Clone)]
pub struct LocalVarGen {
    /// The numbers whose names the modules use, ascending.
    taken: Vec<u64>,
    prefix: String,
    /// How many names have been generated, or passed over by [`LocalVarGen::advance`].
    calls: u64,
}

impl LocalVarGen {
    pub fn new(exclude: VarSet, suffix: &str) -> LocalVarGen {
        let prefix = format!("__local{suffix}");
        let mut taken: Vec<u64> = exclude
            .iter()
            .filter_map(|v| {
                let n = v.strip_prefix(prefix.as_str())?.strip_suffix("__")?;
                let k: u64 = n.parse().ok()?;
                (k.to_string() == n).then_some(k)
            })
            .collect();
        taken.sort_unstable();
        taken.dedup();
        LocalVarGen {
            taken,
            prefix,
            calls: 0,
        }
    }

    pub fn generate(&mut self) -> Var {
        let v = self.name_at(self.calls);
        self.calls = self.calls.wrapping_add(1);
        v
    }

    /// The name the `k`th generation gives: the `k`th number no module's name takes. A
    /// taken number lies below it where its place among the numbers not taken is `k` or
    /// less (`taken[i] - i` of them lie below `taken[i]`), so the count of those, `j`, is
    /// found by halving, and the name's number is `k + j`.
    pub fn name_at(&self, k: u64) -> Var {
        let (mut lo, mut hi) = (0usize, self.taken.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let free_below = self
                .taken
                .get(mid)
                .map_or(u64::MAX, |t| t.saturating_sub(mid as u64));
            if free_below <= k {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let n = k.wrapping_add(lo as u64);
        format!("{}{}__", self.prefix, i64::from_ne_bytes(n.to_ne_bytes())).into()
    }

    /// How many names have been generated.
    pub fn calls(&self) -> u64 {
        self.calls
    }

    /// Passes over `n` names, as generating them would.
    pub fn advance(&mut self, n: u64) {
        self.calls = self.calls.wrapping_add(n);
    }

    /// Back to where `calls` names had been generated.
    pub fn rewind(&mut self, calls: u64) {
        self.calls = calls;
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

/// The state a rewrite threads through: the generator, the errors, and, rewriting as
/// [`Fast`] does, what it keeps.
#[derive(Debug)]
pub struct Rewriter<'a> {
    pub vargen: &'a mut LocalVarGen,
    pub errs: Vec<CompileError>,
    pub fast: Option<Box<Fast>>,
}

/// A rule's rewrite as [`Fast`] does it (in this crate's tests, OPA's way where they ask).
pub fn fast() -> Option<Box<Fast>> {
    #[cfg(test)]
    if EXACT.with(std::cell::Cell::get) {
        return None;
    }
    Some(Box::default())
}

#[cfg(test)]
thread_local! {
    /// Rewrites OPA's way, walking again each walk, for tests holding [`Fast`] to it.
    pub static EXACT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// OPA's rewrite of a rule without OPA's walks again, for a rule that rewrites without
/// errors (one with errors is rewritten again OPA's way, its errors OPA's).
///
/// rewriteEveryStatement (ast/compile.go) rewrites an `every` statement's body, then walks
/// the whole statement again (rewriteDeclaredVarsInExpr), and that walk rewrites each
/// comprehension in it again (rewriteDeclaredVarsInArrayComprehension and its kin), the
/// `every` statements in the comprehension's body again (rewriteEveryStatement: each one's
/// key and value declared again, given new numbers by localVarGenerator.Generate), and
/// those walk themselves again: twice a level, an `every` and a comprehension alternating
/// 20 deep took 9.3 s (PM M510), OPA 7 minutes at 12. The numbers are in what OPA compiles
/// (and in the partial evaluation buildx reads), so this rewrite numbers as OPA's would:
///
/// - A walk whose work a later walk does again is not walked but counted: the generator
///   passes over as many names as it would have generated ([`LocalVarGen::advance`]),
///   each part of a rule keeping how many its walk generates (`walk`, `whole`). The walk
///   of a comprehension in a statement's body or domain while the statement is rewritten
///   again is done again by the statement's walk; an `every` statement's walk, by the walk
///   of a statement around it whose body is still being rewritten (`pending`), where
///   nothing it would rename (a name the statement declares after a use within it, met
///   unrenamed since the statement began: `last_free`) is lost by not walking it.
/// - Each walk of a comprehension generates the names of the statements it holds in one
///   order, so a name declared again in the walk that is done has, as the name it
///   replaces (RewrittenVars, in error texts), the one at the same place in the walk of
///   the comprehension before (`record`, carried down a walk as `ctx`; `owners`).
/// - A use within a comprehension not walked keeps the name it had: it is given the name
///   its declaration was last given (`successors`) where a walk meets it.
#[derive(Debug, Default)]
pub struct Fast {
    /// The `every` statements being rewritten, outermost first.
    frames: Vec<Frame>,
    /// Where in `frames` the statements whose domain or body is being rewritten are.
    pending: Vec<usize>,
    /// How many of `frames` are being walked again; of them, how many are met for the
    /// first time (not themselves within a walk).
    walks: usize,
    orig_walks: usize,
    clock: u64,
    /// When each name was last met and left as it is.
    last_free: HashMap<Var, u64>,
    /// What is kept of each comprehension's and `every` statement's body, by its address.
    nodes: HashMap<usize, NodeInfo>,
    /// The name each declaration given anew was given in place of the one before.
    successors: HashMap<Var, Var>,
    /// The comprehensions being walked, innermost last.
    owners: Vec<Owner>,
    ctx: Vec<Ctx>,
}

#[derive(Debug, Clone, Copy)]
struct Frame {
    /// Within a walk again: its key and value declared anew.
    again: bool,
    walking: bool,
    /// When its rewrite began.
    t0: u64,
    orig_walks_below: usize,
}

#[derive(Debug)]
struct NodeInfo {
    /// Holds the body's place, so that no other body takes the address while this is kept
    /// (and a change to the body moves it: see [`Shared::downgrade`]).
    _body: std::rc::Weak<Body>,
    /// How many names a walk of it generates: of a comprehension, a walk of it; of an
    /// `every` statement, rewriteDeclaredVarsInExpr's of it (`walk`) and
    /// rewriteEveryStatement's (`whole`). Wrapping: a walk 64 levels deep generates more
    /// than 2^64, and OPA's counter, an int, wraps.
    walk: Option<u64>,
    whole: Option<u64>,
    record: Option<Record>,
}

/// The last walk of a comprehension, or of an `every` statement as a term walk: when it
/// was, how many names had been generated as it began, and whether it was the rewrite of
/// the comprehension as written.
#[derive(Debug, Clone, Copy)]
struct Record {
    time: u64,
    start: u64,
    orig: bool,
}

/// A comprehension walked: how many names had been generated as this walk began, and as
/// the walk of it before began (none where that was its rewrite as written).
#[derive(Debug, Clone, Copy)]
struct Owner {
    cur: u64,
    prev: Option<u64>,
}

/// What a walk knows of the walk before of what it walks: when that was, and how many
/// names fewer had been generated as it met what this walk now meets (none: the
/// rewrite as written, whose names the terms keep).
#[derive(Debug, Clone, Copy, Default)]
struct Ctx {
    time: u64,
    delta: Option<u64>,
}

impl Fast {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn node(&self, body: &Shared<Body>) -> Option<&NodeInfo> {
        self.nodes.get(&body.addr())
    }

    fn node_mut(&mut self, body: &Shared<Body>) -> &mut NodeInfo {
        self.nodes.entry(body.addr()).or_insert_with(|| NodeInfo {
            _body: Shared::downgrade(body),
            walk: None,
            whole: None,
            record: None,
        })
    }

    fn record(&mut self, body: &Shared<Body>, record: Record) {
        self.node_mut(body).record = Some(record);
    }

    fn ctx(&self) -> Ctx {
        self.ctx.last().copied().unwrap_or_default()
    }

    /// What a walk meeting `body`'s node, `here` names in, knows of the walk of it before.
    fn ctx_for(&self, body: &Shared<Body>, here: u64) -> Ctx {
        let c = self.ctx();
        match self.node(body).and_then(|n| n.record) {
            Some(r) if r.time > c.time => Ctx {
                time: r.time,
                delta: (!r.orig).then(|| here.wrapping_sub(r.start)),
            },
            _ => c,
        }
    }

    /// The name its declaration was last given, of a use left as it was.
    fn successor(&mut self, v: &str) -> Option<Var> {
        let mut cur = self.successors.get(v)?.clone();
        while let Some(n) = self.successors.get(&cur) {
            cur = n.clone();
        }
        self.successors.insert(v.into(), cur.clone());
        Some(cur)
    }

    fn met_free(&mut self, v: &str) {
        let t = self.tick();
        self.last_free.insert(v.into(), t);
    }

    /// Whether the frame closing has a statement around it whose body is being
    /// rewritten, with no statement between met for the first time and being walked
    /// (whose scope a walk of it later would not have).
    fn enclosed(&self) -> bool {
        let Some(p) = self.pending.last().and_then(|&i| self.frames.get(i)) else {
            return false;
        };
        self.orig_walks == p.orig_walks_below
    }

    /// How many names the walks of a comprehension's head and body generate.
    fn touch_g(&self, terms: &[&mut Term], body: &[Expr]) -> Option<u64> {
        let mut n = self.body_g(body)?;
        for t in terms {
            n = n.wrapping_add(self.term_g(t)?);
        }
        Some(n)
    }

    /// rewriteDeclaredVarsInBody of a body rewritten.
    fn body_g(&self, body: &[Expr]) -> Option<u64> {
        let mut n = 0u64;
        for e in body {
            let g = match &e.terms {
                ExprTerms::Every(ev) => self.node(&ev.body)?.whole?,
                // A body rewritten without errors keeps no declarations.
                _ if e.is_assignment() || e.is_some() => return None,
                _ => self.walk_g(e)?,
            };
            n = n.wrapping_add(g);
        }
        Some(n)
    }

    /// rewriteDeclaredVarsInExpr of an expression rewritten.
    fn walk_g(&self, e: &Expr) -> Option<u64> {
        let mut n = match &e.terms {
            ExprTerms::Term(t) => self.term_g(t)?,
            ExprTerms::Call(c) => self.terms_g(c)?,
            ExprTerms::Some(d) => self.terms_g(&d.symbols)?,
            ExprTerms::Every(ev) => return self.node(&ev.body)?.walk,
        };
        for w in &e.with {
            n = n
                .wrapping_add(self.term_g(&w.target)?)
                .wrapping_add(self.term_g(&w.value)?);
        }
        Some(n)
    }

    /// rewriteDeclaredVarsInExpr of an `every` statement rewritten.
    fn every_walk_g(&self, e: &Expr) -> Option<u64> {
        let ExprTerms::Every(ev) = &e.terms else {
            return self.walk_g(e);
        };
        let mut n = self.term_g(&ev.value)?.wrapping_add(self.term_g(&ev.domain)?);
        if let Some(k) = &ev.key {
            n = n.wrapping_add(self.term_g(k)?);
        }
        for b in ev.body.iter() {
            n = n.wrapping_add(self.walk_g(b)?);
        }
        for w in &e.with {
            n = n
                .wrapping_add(self.term_g(&w.target)?)
                .wrapping_add(self.term_g(&w.value)?);
        }
        Some(n)
    }

    /// rewriteEveryStatement of an `every` statement rewritten: its domain, its key and
    /// value declared anew, its body, and its walk.
    fn every_whole_g(&self, e: &Expr, walk: u64) -> Option<u64> {
        let ExprTerms::Every(ev) = &e.terms else {
            return None;
        };
        let declares = |t: &Term| t.as_var().is_some_and(|v| !vars::is_wildcard(v));
        let kv = u64::from(ev.key.as_ref().is_none_or(declares)) + u64::from(declares(&ev.value));
        Some(
            self.term_g(&ev.domain)?
                .wrapping_add(kv)
                .wrapping_add(self.body_g(&ev.body)?)
                .wrapping_add(walk),
        )
    }

    fn terms_g(&self, ts: &[Term]) -> Option<u64> {
        let mut n = 0u64;
        for t in ts {
            n = n.wrapping_add(self.term_g(t)?);
        }
        Some(n)
    }

    /// A GenericVisitor walk of rewriteDeclaredVarsInTerm over a term: its comprehensions'
    /// walks, as kept.
    fn term_g(&self, t: &Term) -> Option<u64> {
        match &t.value {
            TermValue::Null
            | TermValue::Bool(_)
            | TermValue::Number(_)
            | TermValue::String(_)
            | TermValue::Var(_) => Some(0),
            TermValue::Ref(r)
                if r.len() == 1 && matches!(r.first().and_then(Term::as_var), Some("data" | "input")) =>
            {
                Some(0)
            }
            TermValue::Ref(r) | TermValue::Array(r) | TermValue::Call(r) | TermValue::Set(r) => {
                self.terms_g(r)
            }
            TermValue::Object(o) => {
                let mut n = 0u64;
                for (k, v) in o.iter() {
                    n = n.wrapping_add(self.term_g(k)?).wrapping_add(self.term_g(v)?);
                }
                Some(n)
            }
            TermValue::ArrayCompr(_, body)
            | TermValue::SetCompr(_, body)
            | TermValue::ObjectCompr(_, _, body) => self.node(body)?.walk,
            TermValue::TemplateString { parts, .. } => {
                let mut n = 0u64;
                for p in parts {
                    n = n.wrapping_add(match p {
                        TemplatePart::Term(t) => self.term_g(t)?,
                        TemplatePart::Expr(e) => self.walk_g(e)?,
                    });
                }
                Some(n)
            }
        }
    }

    /// What is kept of an `every` statement as its rewrite or a walk of it leaves it.
    fn keep_every(&mut self, e: &Expr) {
        let ExprTerms::Every(ev) = &e.terms else {
            return;
        };
        let walk = self.every_walk_g(e);
        let whole = walk.and_then(|w| self.every_whole_g(e, w));
        let n = self.node_mut(&ev.body);
        n.walk = walk;
        n.whole = whole;
    }
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
    let again = if let Some(f) = rw.fast.as_mut() {
        let t0 = f.tick();
        let again = f.walks > 0;
        f.pending.push(f.frames.len());
        f.frames.push(Frame {
            again,
            walking: false,
            t0,
            orig_walks_below: f.orig_walks,
        });
        again
    } else {
        false
    };
    term_recursive(rw, stack, &mut ev.domain);
    stack.push();
    let fail = |rw: &mut Rewriter<'_>, m: String| rw.errs.push(err(&ev.loc.clone().or(loc.clone()), m));
    let mut failed = false;
    if let Some(k) = ev.key.as_mut() {
        if let Some(v) = k.as_var().map(Rc::from)
            && !vars::is_wildcard(&v)
        {
            match declare_again(rw, stack, &v, again) {
                Ok(gv) => k.value = TermValue::Var(gv),
                Err(m) => {
                    fail(rw, m);
                    failed = true;
                }
            }
        }
    } else {
        ev.key = Some(Term::new(TermValue::Var(rw.vargen.generate()), None));
    }
    if !failed
        && let Some(v) = ev.value.as_var().map(Rc::from)
        && !vars::is_wildcard(&v)
    {
        match declare_again(rw, stack, &v, again) {
            Ok(gv) => ev.value.value = TermValue::Var(gv),
            Err(m) => {
                fail(rw, m);
                failed = true;
            }
        }
    }
    if failed {
        stack.pop();
        if let Some(f) = rw.fast.as_mut() {
            f.frames.pop();
            f.pending.pop();
        }
        return Opened::Failed;
    }
    let body = std::mem::take(&mut ev.body).into_inner();
    Opened::Body(expr, body)
}

/// rewriteDeclaredVar of an `every` statement's key or value; declared again, within a
/// walk, the name it replaces is the one OPA's walk before gave it (see [`Fast`]).
fn declare_again(rw: &mut Rewriter<'_>, stack: &mut Stack, v: &Var, again: bool) -> Result<Var, String> {
    let here = rw.vargen.calls();
    let gv = declare(rw, stack, v, Occurrence::Declared)?;
    if again && let Some(f) = rw.fast.as_mut() {
        let prev = f
            .owners
            .last()
            .and_then(|o| {
                o.prev
                    .map(|p| rw.vargen.name_at(p.wrapping_add(here.wrapping_sub(o.cur))))
            })
            .unwrap_or_else(|| v.clone());
        stack.rewritten.insert(gv.clone(), prev);
        f.successors.insert(v.clone(), gv.clone());
    }
    Ok(gv)
}

/// The rest of rewriteEveryStatement, its body rewritten: the whole statement walked
/// again in its scope (but for what `settled` says the walk would leave as it is, or, as
/// [`Fast`], where a walk later does its work), and the scope ended. Gives back what a
/// walk of the body it is in may skip of it.
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
    let out = match rw.fast.as_mut() {
        None => rewrite_expr_settled(rw, stack, expr, settled),
        Some(f) => {
            f.pending.pop();
            let frame = f.frames.last().copied();
            let skip = frame.is_some_and(|fr| f.enclosed() && (fr.again || scope_clean(f, stack, fr.t0)));
            let g = if skip { f.every_walk_g(&expr) } else { None };
            let time = f.tick();
            let start = rw.vargen.calls();
            let out = match g {
                Some(g) => {
                    rw.vargen.advance(g);
                    expr
                }
                None => {
                    let again = frame.is_some_and(|fr| fr.again);
                    if let Some(fr) = f.frames.last_mut() {
                        fr.walking = true;
                    }
                    f.walks += 1;
                    if !again {
                        f.orig_walks += 1;
                    }
                    let out = rewrite_expr_settled(rw, stack, expr, settled);
                    if let Some(f) = rw.fast.as_mut() {
                        f.walks = f.walks.saturating_sub(1);
                        if !again {
                            f.orig_walks = f.orig_walks.saturating_sub(1);
                        }
                    }
                    out
                }
            };
            if let Some(f) = rw.fast.as_mut() {
                f.frames.pop();
                if let ExprTerms::Every(ev) = &out.terms {
                    f.record(
                        &ev.body,
                        Record {
                            time,
                            start,
                            orig: false,
                        },
                    );
                }
                f.keep_every(&out);
            }
            out
        }
    };
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

/// Whether the walk of an `every` statement first met, its scope on top, would rename
/// nothing: no name it declares was met within it and left as it was since it began
/// (`t0`); only those could the walk rename, the scopes below it declaring nothing since.
fn scope_clean(f: &Fast, stack: &Stack, t0: u64) -> bool {
    let Some(s) = stack.peek() else { return true };
    s.occurrence.iter().all(|(name, occ)| {
        *occ == Occurrence::Seen
            || s.vs.get(name) == Some(name)
            || f.last_free.get(name).is_none_or(|t| *t < t0)
    })
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

/// A statement being walked, the place of its body's next expression, and when its walk
/// began with how many names had been generated (see [`rewrite_expr_settled`]).
type Open = (Expr, usize, Option<(u64, u64)>);

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
    // next expression and, rewriting as [`Fast`] does, when its walk began and how many
    // names had been generated (a statement within another's).
    let mut open: Vec<Open> = Vec::new();
    let mut next = Some(expr);
    loop {
        if let Some(mut e) = next.take() {
            match &mut e.terms {
                ExprTerms::Term(t) => walk_decl_terms(rw, stack, t),
                ExprTerms::Call(c) => c.iter_mut().for_each(|t| walk_decl_terms(rw, stack, t)),
                ExprTerms::Some(d) => d.symbols.iter_mut().for_each(|t| walk_decl_terms(rw, stack, t)),
                ExprTerms::Every(ev) => {
                    let mut enter = None;
                    if !open.is_empty()
                        && let Some(f) = rw.fast.as_mut()
                    {
                        let start = rw.vargen.calls();
                        let ctx = f.ctx_for(&ev.body, start);
                        f.ctx.push(ctx);
                        enter = Some((f.tick(), start));
                    }
                    if let Some(k) = ev.key.as_mut() {
                        walk_decl_terms(rw, stack, k);
                    }
                    walk_decl_terms(rw, stack, &mut ev.value);
                    walk_decl_terms(rw, stack, &mut ev.domain);
                    open.push((e, 0, enter));
                    continue;
                }
            }
            rewrite_with(rw, stack, &mut e);
            match open.last_mut() {
                Some((parent, i, _)) => put_back(parent, i.saturating_sub(1), e),
                None => return e,
            }
            continue;
        }
        let outermost = open.len() == 1;
        let Some((parent, i, _)) = open.last_mut() else {
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
        let Some((mut done, _, enter)) = open.pop() else {
            return Expr::term(Term::boolean(true, None));
        };
        rewrite_with(rw, stack, &mut done);
        if let (Some((time, start)), Some(f)) = (enter, rw.fast.as_mut()) {
            f.ctx.pop();
            if let ExprTerms::Every(ev) = &done.terms {
                f.record(
                    &ev.body,
                    Record {
                        time,
                        start,
                        orig: false,
                    },
                );
            }
            f.keep_every(&done);
        }
        match open.last_mut() {
            Some((parent, i, _)) => put_back(parent, i.saturating_sub(1), done),
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
                if gv == v
                    && let Some(f) = rw.fast.as_mut()
                {
                    f.met_free(&v);
                }
                t.value = TermValue::Var(gv);
                stack.seen(&v);
            } else if let Some(succ) = rw.fast.as_mut().and_then(|f| f.successor(&v)) {
                // A use a walk left as it was: OPA's term has the name its declaration was
                // last given, and meets it.
                if let Some(gv) = stack.use_var(&succ) {
                    t.value = TermValue::Var(gv);
                    stack.seen(&succ);
                } else {
                    if stack.occurrence(&succ) == Occurrence::New {
                        stack.insert(&succ, &succ, Occurrence::Seen);
                    }
                    t.value = TermValue::Var(succ);
                }
            } else {
                if stack.occurrence(&v) == Occurrence::New {
                    stack.insert(&v, &v, Occurrence::Seen);
                }
                if let Some(f) = rw.fast.as_mut() {
                    f.met_free(&v);
                }
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
                } else if let Some(f) = rw.fast.as_mut() {
                    f.met_free(x);
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

/// rewriteDeclaredVarsIn{Array,Set,Object}Comprehension. Rewriting as [`Fast`], a walk
/// of it in an `every` statement's domain or body as the statement is rewritten again is
/// counted, not walked: the statement's walk does it again.
fn comprehension(rw: &mut Rewriter<'_>, stack: &mut Stack, terms: &mut [&mut Term], body: &mut Shared<Body>) {
    let mut start = 0;
    let mut time = 0;
    let mut first = true;
    if let Some(f) = rw.fast.as_mut() {
        start = rw.vargen.calls();
        first = f.walks == 0;
        let doomed = f.frames.last().is_some_and(|fr| fr.again && !fr.walking);
        if doomed && let Some(g) = f.node(body).and_then(|n| n.walk) {
            let time = f.tick();
            f.record(
                body,
                Record {
                    time,
                    start,
                    orig: false,
                },
            );
            rw.vargen.advance(g);
            return;
        }
        let ctx = f.ctx_for(body, start);
        f.owners.push(Owner {
            cur: start,
            prev: ctx.delta.map(|d| start.wrapping_sub(d)),
        });
        f.ctx.push(ctx);
        time = f.tick();
    }
    let mut used = VarSet::new();
    for t in terms.iter() {
        used.extend(vars::term_vars(t));
    }
    stack.push();
    let b = std::mem::replace(body, Shared::from(Vec::new())).into_inner();
    *body = rewrite_body(rw, stack, &used, b).into();
    for t in terms.iter_mut() {
        term_recursive(rw, stack, t);
    }
    stack.pop();
    if let Some(f) = rw.fast.as_mut() {
        f.owners.pop();
        f.ctx.pop();
        f.record(
            body,
            Record {
                time,
                start,
                orig: first,
            },
        );
        let walk = f.touch_g(terms, body);
        f.node_mut(body).walk = walk;
    }
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
