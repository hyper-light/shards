//! OPA's compiler (ast/compile.go, v1.14.1): the stages that check modules and rewrite
//! them into the form the evaluator runs, in OPA's order, numbering the variables they
//! generate as OPA numbers them. Held to OPA by `tests/compile.rs`, against the compiled
//! modules' text `scripts/rego/generate` records.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use crate::ast::{Body, Expr, ExprTerms, Import, Location, Module, Rule, Term, TermValue};
use crate::builtins::{self, Builtin};
use crate::check;
use crate::types::Type;

pub mod localvars;
pub mod rewrite;
pub mod safety;
pub mod transform;
pub mod vars;

use vars::{Var, VarSet};

pub const COMPILE_ERR: &str = "rego_compile_error";
pub const TYPE_ERR: &str = "rego_type_error";
pub const UNSAFE_VAR_ERR: &str = "rego_unsafe_var_error";
pub const RECURSION_ERR: &str = "rego_recursion_error";

/// OPA's CompileErrorLimitDefault.
const MAX_ERRS: usize = 10;

pub type CompileError = crate::parser::Error;

impl CompileError {
    pub fn compile(loc: Option<Location>, msg: String) -> CompileError {
        CompileError::new(COMPILE_ERR, loc, msg)
    }
}

/// A function the policies may call beside OPA's builtins (buildx's own), by name.
#[derive(Debug, Clone)]
pub struct Function {
    pub name: String,
    pub decl: Type,
}

/// Where a rule is: its module's name and its index there.
pub type RuleId = (String, usize);

/// A rule or one of its else branches: module, index, depth in the else chain.
pub type RuleNode = (String, usize, usize);

/// Whether a ref may name (part of) the document a rule defines: their common parts
/// equal, a variable matching anything.
pub fn refers_to(r: &[Term], rule: &[Term]) -> bool {
    r.iter().zip(rule).all(|(a, b)| a.as_var().is_some() || b.as_var().is_some() || a.equal(b))
        && r.first().and_then(Term::as_var) == rule.first().and_then(Term::as_var)
}

/// OPA's rule tree, flattened: each ground prefix of a rule's ref to the rules there, in
/// the order OPA adds them (modules by name, rules in order).
#[derive(Debug, Default, Clone)]
pub struct RuleTree {
    pub nodes: BTreeMap<Vec<String>, Vec<RuleId>>,
}

/// A ref's key in the tree: each part's text (strings quoted, as terms print).
pub fn ref_key(r: &[Term]) -> Vec<String> {
    r.iter().map(Term::to_string).collect()
}

impl RuleTree {
    /// GetRulesExact.
    pub fn exact(&self, r: &[Term]) -> &[RuleId] {
        if r.iter().any(|t| matches!(t.value, TermValue::Ref(_) | TermValue::Call(_))) {
            return &[];
        }
        self.nodes.get(&ref_key(r)).map(Vec::as_slice).unwrap_or_default()
    }

    /// Whether any rule's path starts with this ref (TreeNode.Find found a node).
    pub fn has_node(&self, r: &[Term]) -> bool {
        let k = ref_key(r);
        self.nodes.keys().any(|p| p.starts_with(&k))
    }
}

#[derive(Debug)]
pub struct Compiler {
    /// The modules, by name: OPA's `c.sorted` order.
    pub modules: BTreeMap<String, Module>,
    pub errors: Vec<CompileError>,
    /// Generated names to the names they replace (RewrittenVars).
    pub rewritten: HashMap<Var, Var>,
    pub tree: RuleTree,
    vargen: localvars::LocalVarGen,
    functions: Vec<Function>,
    print: bool,
    limit_reached: bool,
    /// The types the checker knows: the builtins', then the rules' (c.TypeEnv).
    pub type_env: check::TypeEnv,
}

/// The builtins a policy may call (buildx's allow-list).
pub fn allowed(name: &str) -> Option<&'static Builtin> {
    builtins::registry().get(name).filter(|b| b.allowed)
}

impl Compiler {
    pub fn new(modules: BTreeMap<String, Module>, functions: Vec<Function>, print: bool) -> Compiler {
        let decls = builtins::registry().values().filter(|b| b.allowed).map(|b| (b.name.as_str(), &b.decl));
        let type_env = check::TypeEnv::with_builtins(decls.chain(functions.iter().map(|f| (f.name.as_str(), &f.decl))));
        Compiler {
            type_env,
            modules,
            errors: Vec::new(),
            rewritten: HashMap::new(),
            tree: RuleTree::default(),
            vargen: localvars::LocalVarGen::new(VarSet::new(), ""),
            functions,
            print,
            limit_reached: false,
        }
    }

    /// c.err: records errors up to OPA's limit; false once the limit is reached.
    pub fn err(&mut self, errs: Vec<CompileError>) -> bool {
        if errs.is_empty() {
            return true;
        }
        if self.limit_reached {
            return false;
        }
        let remaining = MAX_ERRS.saturating_sub(self.errors.len());
        let take = remaining.min(errs.len());
        self.errors.extend(errs.into_iter().take(take));
        if self.errors.len() == MAX_ERRS {
            self.errors.push(CompileError::compile(None, "error limit reached".into()));
            self.limit_reached = true;
            return false;
        }
        true
    }

    /// The declaration of a callable builtin or host function.
    pub fn builtin_decl(&self, name: &str) -> Option<&Type> {
        allowed(name).map(|b| &b.decl).or_else(|| self.functions.iter().find(|f| f.name == name).map(|f| &f.decl))
    }

    /// GetArity: a builtin's declared arguments, or a function rule's; None when neither.
    pub fn arity(&self, r: &[Term]) -> Option<usize> {
        let name = text_of_ref(r);
        if let Some(Type::Function { args, .. }) = self.builtin_decl(&name) {
            return Some(args.len());
        }
        let first = self.tree.exact(r).first()?;
        self.rule(first).map(|rule| rule.head.args.len())
    }

    pub fn rule(&self, id: &RuleId) -> Option<&Rule> {
        self.modules.get(&id.0).and_then(|m| m.rules.get(id.1))
    }

    /// Compiles the modules, stage by stage, stopping after the first stage that fails.
    pub fn compile(&mut self) {
        let stages: [fn(&mut Compiler); 22] = [
            Compiler::resolve_all_refs,
            Compiler::init_local_var_gen,
            rewrite::rewrite_rule_head_refs,
            Compiler::check_keyword_overrides,
            Compiler::check_imports,
            Compiler::remove_imports,
            Compiler::set_rule_tree,
            Compiler::rewrite_local_vars,
            rewrite::rewrite_template_strings,
            rewrite::check_void_calls,
            rewrite::rewrite_print_calls,
            rewrite::rewrite_expr_terms,
            rewrite::rewrite_comprehension_terms,
            rewrite::rewrite_refs_in_head,
            rewrite::rewrite_with_modifiers,
            Compiler::check_rule_conflicts,
            Compiler::check_undefined_funcs,
            Compiler::check_safety_rule_heads,
            Compiler::check_safety_rule_bodies,
            Compiler::rewrite_equals_and_dynamics,
            Compiler::check_recursion,
            Compiler::check_types,
        ];
        for stage in stages {
            stage(self);
            if !self.errors.is_empty() {
                return;
            }
        }
    }

    fn names(&self) -> Vec<String> {
        self.modules.keys().cloned().collect()
    }

    /// resolveAllRefs: rule names and imports to the refs they stand for.
    fn resolve_all_refs(&mut self) {
        // getExports: each package's rules' ground prefixes, without repeats.
        let mut exports: HashMap<Vec<String>, Vec<Vec<Term>>> = HashMap::new();
        for m in self.modules.values() {
            let key = ref_key(&m.package.path);
            for r in &m.rules {
                let prefix = ground_prefix(&r.head.ref_path());
                let list = exports.entry(key.clone()).or_default();
                if !list.iter().any(|p| ref_key(p) == ref_key(&prefix)) {
                    list.push(prefix);
                }
            }
        }
        let mut errs = Vec::new();
        for m in self.modules.values_mut() {
            let mut globals: HashMap<Var, Vec<Term>> = HashMap::new();
            for r in exports.get(&ref_key(&m.package.path)).map(Vec::as_slice).unwrap_or_default() {
                if let Some(v) = r.first().and_then(Term::as_var) {
                    let mut path = m.package.path.clone();
                    path.push(Term::string(v, None));
                    globals.insert(v.into(), path);
                }
            }
            for imp in &m.imports {
                let path = imp.path.as_ref().unwrap_or_default();
                if matches!(path.first().and_then(Term::as_var), Some("future" | "rego")) {
                    continue;
                }
                globals.insert(import_name(imp), path.to_vec());
            }
            for rule in m.rules.iter_mut() {
                let mut r = Some(rule);
                while let Some(x) = r {
                    if let Err(e) = resolve_rule(&globals, x) {
                        errs.push(CompileError::compile(x.loc.clone(), e));
                    }
                    r = x.else_.as_deref_mut();
                }
            }
        }
        self.err(errs);
    }

    fn init_local_var_gen(&mut self) {
        let mut vis = vars::VarVisitor::default();
        for m in self.modules.values() {
            for r in &m.rules {
                vis.rule(r);
            }
        }
        self.vargen = localvars::LocalVarGen::new(vis.vars, "");
    }

    /// checkKeywordOverrides (Rego v1): no rule, argument or assignment named `data` or
    /// `input`.
    fn check_keyword_overrides(&mut self) {
        for name in self.names() {
            let Some(m) = self.modules.get(&name) else { continue };
            let mut errs = Vec::new();
            for rule in &m.rules {
                let mut r = Some(rule);
                while let Some(x) = r {
                    let head_name = x.head.reference.first().and_then(Term::as_var).map(str::to_string).or_else(|| x.head.name.as_deref().map(str::to_string));
                    if let Some(n) = head_name.filter(|n| n == "data" || n == "input") {
                        errs.push(CompileError::compile(x.loc.clone(), format!("rules must not shadow {n} (use a different rule name)")));
                    }
                    for a in &x.head.args {
                        if let Some([t]) = a.as_ref()
                            && matches!(t.as_var(), Some("data" | "input"))
                        {
                            errs.push(CompileError::compile(a.loc.clone(), format!("args must not shadow {a} (use a different variable name)")));
                        }
                    }
                    r = x.else_.as_deref();
                }
            }
            for rule in &m.rules {
                walk_exprs(rule, &mut |e: &Expr| {
                    if e.is_assignment()
                        && let Some(op) = e.operand(0)
                    {
                        let n = op.to_string();
                        if n == "data" || n == "input" {
                            errs.push(CompileError::compile(e.loc.clone(), format!("variables must not shadow {n} (use a different variable name)")));
                        }
                    }
                });
            }
            if !self.err(errs) {
                return;
            }
        }
    }

    /// checkImports: no import shadowing another.
    fn check_imports(&mut self) {
        let mut errs = Vec::new();
        for m in self.modules.values() {
            let mut seen: HashMap<Var, &Import> = HashMap::new();
            for imp in &m.imports {
                let name = import_name(imp);
                if let Some(prev) = seen.get(&name) {
                    let mut text = format!("import {}", prev.path);
                    if let Some(a) = &prev.alias {
                        text.push_str(&format!(" as {a}"));
                    }
                    errs.push(CompileError::compile(imp.loc.clone(), format!("import must not shadow {text}")));
                } else {
                    seen.insert(name, imp);
                }
            }
        }
        self.err(errs);
    }

    fn remove_imports(&mut self) {
        for m in self.modules.values_mut() {
            m.imports.clear();
        }
    }

    /// setModuleTree and setRuleTree.
    fn set_rule_tree(&mut self) {
        let mut tree = RuleTree::default();
        for (name, m) in &self.modules {
            for (i, r) in m.rules.iter().enumerate() {
                let path = ground_prefix(&rule_ref(&m.package.path, r));
                tree.nodes.entry(ref_key(&path)).or_default().push((name.clone(), i));
            }
        }
        self.tree = tree;
    }

    /// rewriteLocalVars.
    fn rewrite_local_vars(&mut self) {
        let mut vargen = std::mem::replace(&mut self.vargen, localvars::LocalVarGen::new(VarSet::new(), ""));
        let mut rewritten = std::mem::take(&mut self.rewritten);
        let mut all_errs = Vec::new();
        for m in self.modules.values_mut() {
            for rule in m.rules.iter_mut() {
                let mut rw = localvars::Rewriter { vargen: &mut vargen, errs: Vec::new() };
                let mut args_stack = localvars::Stack::default();
                localvars::rewrite_arg_vars(&mut rw, &mut args_stack, rule);
                let mut r = Some(rule);
                while let Some(x) = r {
                    let _ = localvars::rewrite_rule(&mut rw, &mut rewritten, x, &args_stack);
                    r = x.else_.as_deref_mut();
                }
                all_errs.extend(rw.errs);
            }
        }
        self.vargen = vargen;
        self.rewritten = rewritten;
        self.err(all_errs);
    }

    /// checkRuleConflicts, for rules of one path.
    fn check_rule_conflicts(&mut self) {
        let mut errs = Vec::new();
        let paths: Vec<Vec<String>> = self.tree.nodes.keys().cloned().collect();
        for path in &paths {
            let ids = self.tree.nodes.get(path).cloned().unwrap_or_default();
            let rules: Vec<&Rule> = ids.iter().filter_map(|id| self.rule(id)).collect();
            let Some(first) = rules.first() else { continue };
            let pkg = ids.first().and_then(|id| self.modules.get(&id.0)).map(|m| m.package.path.clone()).unwrap_or_default();
            let name = rewrite_vars_in_ref(&self.rewritten, &rule_ref(&pkg, first));
            let kinds: std::collections::HashSet<bool> = rules.iter().map(|r| r.head.kind() == crate::ast::RuleKind::MultiValue).collect();
            let arities: std::collections::HashSet<usize> = rules.iter().map(|r| r.head.args.len()).collect();
            let complete = rules
                .iter()
                .filter(|r| r.head.kind() == crate::ast::RuleKind::SingleValue && r.head.ref_path().iter().skip(1).all(Term::is_ground))
                .count();
            let partial = rules.len() - complete;
            let defaults: Vec<&&Rule> = rules.iter().filter(|r| r.default).collect();
            // A complete rule whose path is a prefix of another rule's conflicts with it.
            let children: Vec<&Vec<String>> = paths.iter().filter(|p| p.len() > path.len() && p.starts_with(path)).collect();
            if first.head.ref_path().iter().skip(1).all(Term::is_ground) && !children.is_empty() {
                // flattenChildren: the children's rules' refs, prefixes only, sorted.
                let mut refs: Vec<Vec<Term>> = Vec::new();
                for p in &children {
                    for id in self.tree.nodes.get(*p).map(Vec::as_slice).unwrap_or_default() {
                        let Some(m) = self.modules.get(&id.0) else { continue };
                        let Some(r) = m.rules.get(id.1) else { continue };
                        let rr = rule_ref(&m.package.path, r);
                        if refs.iter().any(|x| has_prefix(&rr, x)) {
                            continue;
                        }
                        refs.retain(|x| !has_prefix(x, &rr));
                        refs.push(rr);
                    }
                }
                refs.sort_by(|a, b| crate::compare::term_compare(&Term::reference(a.clone(), None), &Term::reference(b.clone(), None)));
                let list: Vec<String> = refs.iter().map(|r| text_of_ref(r)).collect();
                errs.push(CompileError::new(TYPE_ERR, first.loc.clone(), format!("rule {name} conflicts with [{}]", list.join(" "))));
                continue;
            }
            if kinds.len() > 1 || arities.len() > 1 || (complete >= 1 && partial >= 1) {
                errs.push(CompileError::new(TYPE_ERR, first.loc.clone(), format!("conflicting rules {name} found")));
                continue;
            }
            if defaults.len() > 1 {
                let locs: Vec<String> = defaults.iter().map(|r| loc_text(&r.loc)).collect();
                let pkg_loc = self.modules.get(&ids.first().map(|i| i.0.clone()).unwrap_or_default()).and_then(|m| m.package.loc.clone());
                errs.push(CompileError::new(TYPE_ERR, pkg_loc, format!("multiple default rules {name} found at {}", locs.join(", "))));
            }
        }
        self.err(errs);
    }

    /// checkUndefinedFuncs.
    fn check_undefined_funcs(&mut self) {
        let mut errs = Vec::new();
        for m in self.modules.values() {
            for rule in &m.rules {
                walk_exprs(rule, &mut |e: &Expr| {
                    let ExprTerms::Call(terms) = &e.terms else { return };
                    let Some(op) = terms.first().and_then(Term::as_ref) else { return };
                    let operands = terms.len() - 1;
                    match self.arity(op) {
                        Some(arity) => {
                            let bad = if e.generated {
                                !e.is_equality() && operands != arity + 1
                            } else {
                                operands != arity && operands != arity + 1
                            };
                            if bad {
                                let f = rewrite_ref(&self.rewritten, op);
                                let got = if e.generated { operands - 1 } else { operands };
                                errs.push(self.arity_error(e, &f, arity, got));
                            }
                        }
                        None => {
                            let f = rewrite_vars_in_ref(&self.rewritten, op);
                            errs.push(CompileError::new(TYPE_ERR, e.loc.clone(), format!("undefined function {f}")));
                        }
                    }
                });
            }
        }
        self.err(errs);
    }

    /// arityMismatchError: with the declaration's arguments for a builtin.
    fn arity_error(&self, e: &Expr, f: &[Term], exp: usize, act: usize) -> CompileError {
        if let Some(err) = check::arity_error(&self.type_env, f, e) {
            return err;
        }
        let noun = if act == 1 { "argument" } else { "arguments" };
        let f = text_of_ref(f);
        CompileError::new(TYPE_ERR, e.loc.clone(), format!("function {f} has arity {exp}, got {act} {noun}"))
    }

    /// checkSafetyRuleHeads.
    fn check_safety_rule_heads(&mut self) {
        let mut errs = Vec::new();
        for m in self.modules.values() {
            for rule in &m.rules {
                let mut r = Some(rule);
                while let Some(x) = r {
                    if rewrite::head_may_have_vars(x) {
                        let mut vis = vars::VarVisitor::new(vars::SAFETY);
                        vis.body(&x.body);
                        vis.params = vars::Params::default();
                        vis.args(&x.head.args);
                        // Head.Vars: arguments, key, value and the ref past its name.
                        let mut head_vars = vars::VarVisitor::default();
                        head_vars.args(&x.head.args);
                        if let Some(k) = &x.head.key {
                            head_vars.term(k);
                        }
                        if let Some(v) = &x.head.value {
                            head_vars.term(v);
                        }
                        for t in x.head.reference.iter().skip(1) {
                            head_vars.term(t);
                        }
                        for v in head_vars.vars.difference(&vis.vars) {
                            let v = self.rewritten.get(v).cloned().unwrap_or_else(|| v.clone());
                            if !vars::is_generated(&v) {
                                errs.push(CompileError::new(UNSAFE_VAR_ERR, x.head.loc.clone(), format!("var {v} is unsafe")));
                            }
                        }
                    }
                    r = x.else_.as_deref();
                }
            }
        }
        self.err(errs);
    }

    /// checkSafetyRuleBodies: each body reordered so its variables are bound first.
    fn check_safety_rule_bodies(&mut self) {
        let names = self.names();
        for name in names {
            let Some(m) = self.modules.get(&name) else { continue };
            let mut rules = m.rules.clone();
            for rule in rules.iter_mut() {
                let mut r = Some(rule);
                while let Some(x) = r {
                    let mut safe: VarSet = ["data", "input"].iter().map(|s| Rc::from(*s)).collect();
                    let mut v = vars::VarVisitor::default();
                    v.args(&x.head.args);
                    safe.extend(v.vars);
                    let body = std::mem::take(&mut x.body);
                    x.body = rewrite::check_body_safety(self, &safe, body);
                    r = x.else_.as_deref_mut();
                }
            }
            if let Some(m) = self.modules.get_mut(&name) {
                m.rules = rules;
            }
        }
    }

    /// The rules, else branches included, a rule's refs may name (setGraph's edges).
    fn dependencies(&self, rule: &Rule) -> Vec<RuleNode> {
        let mut refs: Vec<Vec<Term>> = Vec::new();
        let mut collect = |t: &Term| -> bool {
            if let TermValue::Ref(r) = &t.value {
                refs.push(r.clone());
            }
            false
        };
        for t in rule.head.args.iter().chain(rule.head.key.iter()).chain(rule.head.value.iter()) {
            safety::walk_terms(t, &mut collect);
        }
        for e in &rule.body {
            safety::walk_terms_expr(e, &mut collect);
        }
        let mut out = Vec::new();
        for r in &refs {
            if r.first().and_then(Term::as_var) != Some("data") {
                continue;
            }
            for (name, m) in &self.modules {
                for (i, x) in m.rules.iter().enumerate() {
                    if refers_to(r, &rule_ref(&m.package.path, x)) {
                        let mut depth = 0;
                        let mut e = Some(x);
                        while let Some(y) = e {
                            let _ = y;
                            out.push((name.clone(), i, depth));
                            depth += 1;
                            e = y.else_.as_deref();
                        }
                    }
                }
            }
        }
        out
    }

    fn rule_node(&self, n: &RuleNode) -> Option<&Rule> {
        let mut r = self.modules.get(&n.0)?.rules.get(n.1)?;
        for _ in 0..n.2 {
            r = r.else_.as_deref()?;
        }
        Some(r)
    }

    /// checkRecursion: a rule that depends on itself, with the path that leads back.
    fn check_recursion(&mut self) {
        let mut errs = Vec::new();
        let paths: Vec<Vec<String>> = self.tree.nodes.keys().cloned().collect();
        for p in &paths {
            for id in self.tree.nodes.get(p).cloned().unwrap_or_default() {
                let mut depth = 0;
                while let Some(rule) = self.rule_node(&(id.0.clone(), id.1, depth)) {
                    let node = (id.0.clone(), id.1, depth);
                    let mut visited = std::collections::HashSet::new();
                    let path = self.dfs(&node, &node, &mut visited);
                    if !path.is_empty() {
                        let text = |n: &RuleNode| -> String {
                            let pkg = self.modules.get(&n.0).map(|m| m.package.path.clone()).unwrap_or_default();
                            self.rule_node(n).map(|r| text_of_ref(&rule_ref(&pkg, r))).unwrap_or_default()
                        };
                        let names: Vec<String> = path.iter().rev().map(text).collect();
                        errs.push(CompileError::new(
                            RECURSION_ERR,
                            rule.loc.clone(),
                            format!("rule {} is recursive: {}", text(&node), names.join(" -> ")),
                        ));
                    }
                    depth += 1;
                }
            }
        }
        self.err(errs);
    }

    /// util.dfsRecursive.
    fn dfs(&self, u: &RuleNode, z: &RuleNode, visited: &mut std::collections::HashSet<RuleNode>) -> Vec<RuleNode> {
        if !visited.insert(u.clone()) {
            return Vec::new();
        }
        let Some(rule) = self.rule_node(u) else { return Vec::new() };
        for v in self.dependencies(rule) {
            if &v == z {
                return vec![z.clone(), u.clone()];
            }
            let mut p = self.dfs(&v, z, visited);
            if !p.is_empty() {
                p.push(u.clone());
                return p;
            }
        }
        Vec::new()
    }

    /// Every rule and else branch, each after the rules it depends on (Graph.Sort).
    fn sorted_rules(&self) -> Vec<RuleNode> {
        let mut marked = std::collections::HashSet::new();
        let mut out = Vec::new();
        for (name, m) in &self.modules {
            for (i, rule) in m.rules.iter().enumerate() {
                let mut depth = 0;
                let mut r = Some(rule);
                while let Some(x) = r {
                    self.sort_visit(&(name.clone(), i, depth), &mut marked, &mut out);
                    depth += 1;
                    r = x.else_.as_deref();
                }
            }
        }
        out
    }

    /// graphSort.Visit: CheckRecursion has ruled out cycles.
    fn sort_visit(&self, n: &RuleNode, marked: &mut std::collections::HashSet<RuleNode>, out: &mut Vec<RuleNode>) {
        if !marked.insert(n.clone()) {
            return;
        }
        if let Some(rule) = self.rule_node(n) {
            for d in self.dependencies(rule) {
                self.sort_visit(&d, marked, out);
            }
        }
        out.push(n.clone());
    }

    /// checkTypes: the rules type checked in dependency order, their types kept.
    fn check_types(&mut self) {
        let sorted = self.sorted_rules();
        let mut env = self.type_env.clone();
        env.wrap();
        let errs = {
            let mut rules = Vec::with_capacity(sorted.len());
            for n in &sorted {
                let pkg = self.modules.get(&n.0).map(|m| m.package.path.as_slice()).unwrap_or_default();
                if let Some(rule) = self.rule_node(n) {
                    rules.push((rule_ref(pkg, rule), rule));
                }
            }
            check::Checker::new(Some(&self.rewritten)).check_types(&mut env, &rules)
        };
        self.type_env = env;
        for e in errs {
            self.err(vec![e]);
        }
    }

    /// PassesTypeCheck: whether a body has no type errors against the rules' types.
    pub fn passes_type_check(&self, body: &Body) -> bool {
        let mut env = self.type_env.clone();
        check::Checker::new(None).check_body(&mut env, body).is_empty()
    }

    fn rewrite_equals_and_dynamics(&mut self) {
        rewrite::rewrite_equals(self);
        rewrite::rewrite_dynamic_terms(self);
    }

    pub fn vargen(&mut self) -> &mut localvars::LocalVarGen {
        &mut self.vargen
    }

    pub fn print_enabled(&self) -> bool {
        self.print
    }
}

/// Rule.Ref: the package's path, then the head's ref, its first part as a string.
pub fn rule_ref(package: &[Term], rule: &Rule) -> Vec<Term> {
    let mut out = package.to_vec();
    let head = rule.head.ref_path();
    if let Some((first, rest)) = head.split_first() {
        let name = first.as_var().unwrap_or_default();
        out.push(Term::string(name, first.loc.clone()));
        out.extend(rest.iter().cloned());
    }
    out
}

/// Ref.HasPrefix.
pub fn has_prefix(r: &[Term], prefix: &[Term]) -> bool {
    prefix.len() <= r.len() && r.iter().zip(prefix).all(|(a, b)| a.equal(b))
}

/// Location text as OPA writes it: `file:row`.
pub fn loc_text(l: &Option<Location>) -> String {
    match l {
        Some(l) if !l.file.is_empty() => format!("{}:{}", l.file, l.row),
        Some(l) => format!("{}:{}", l.row, l.col),
        None => String::new(),
    }
}

/// A ref's text.
pub fn text_of_ref(r: &[Term]) -> String {
    let mut s = String::new();
    crate::ast::write_ref(&mut s, r);
    s
}

/// rewriteVarsInRef: generated names back to the ones written.
pub fn rewrite_ref(rewritten: &HashMap<Var, Var>, r: &[Term]) -> Vec<Term> {
    let mut r = r.to_vec();
    for t in r.iter_mut() {
        transform_vars(t, &mut |v: &Var| rewritten.get(v).cloned());
    }
    r
}

/// The text of a ref, its generated names written as they were.
pub fn rewrite_vars_in_ref(rewritten: &HashMap<Var, Var>, r: &[Term]) -> String {
    text_of_ref(&rewrite_ref(rewritten, r))
}

/// Ref.GroundPrefix: the head, then the ground terms up to the first that is not.
pub fn ground_prefix(r: &[Term]) -> Vec<Term> {
    let mut out: Vec<Term> = r.first().cloned().into_iter().collect();
    out.extend(r.iter().skip(1).take_while(|t| t.is_ground()).cloned());
    out
}

/// Import.Name: its alias, else its last part.
pub fn import_name(imp: &Import) -> Var {
    if let Some(a) = &imp.alias {
        return a.clone();
    }
    let r = imp.path.as_ref().unwrap_or_default();
    match r.last() {
        Some(t) if r.len() == 1 => t.as_var().map(Rc::from).unwrap_or_else(|| Rc::from("")),
        Some(t) => t.as_string().map(Rc::from).unwrap_or_else(|| Rc::from("")),
        None => Rc::from(""),
    }
}

/// Replaces variables everywhere under a term, closures included.
pub fn transform_vars(t: &mut Term, f: &mut dyn FnMut(&Var) -> Option<Var>) {
    transform::terms_mut(t, &mut |x: &mut Term| {
        if let TermValue::Var(v) = &x.value
            && let Some(n) = f(v)
        {
            x.value = TermValue::Var(n);
        }
    });
}

/// Every expression of a rule and its else chain, closures included (WalkExprs).
pub fn walk_exprs(rule: &Rule, f: &mut dyn FnMut(&Expr)) {
    let mut r = Some(rule);
    while let Some(x) = r {
        for t in head_terms(x) {
            transform::exprs_in_term(t, f);
        }
        for e in &x.body {
            transform::exprs(e, f);
        }
        r = x.else_.as_deref();
    }
}

fn head_terms(r: &Rule) -> Vec<&Term> {
    let mut out: Vec<&Term> = r.head.args.iter().collect();
    out.extend(r.head.key.iter());
    out.extend(r.head.value.iter());
    out
}

/// declaredVars of a body: what its `:=` and `some` declare, closures skipped.
pub fn declared_vars(body: &[Expr]) -> VarSet {
    let mut out = VarSet::new();
    for e in body {
        declared_in_expr(e, &mut out);
    }
    out
}

fn declared_in_expr(e: &Expr, out: &mut VarSet) {
    if e.is_assignment() && e.operand(2).is_none() {
        if let Some(lhs) = e.operand(0) {
            out.extend(vars::term_vars(lhs));
        }
    } else if let ExprTerms::Some(d) = &e.terms {
        for s in &d.symbols {
            match &s.value {
                TermValue::Var(v) => {
                    out.insert(v.clone());
                }
                TermValue::Call(c) => {
                    let args = c.get(1..).unwrap_or_default();
                    if args.len() == 3
                        && let Some(a) = args.get(1)
                    {
                        out.extend(vars::term_vars(a));
                    }
                    if let Some(a) = args.first() {
                        out.extend(vars::term_vars(a));
                    }
                }
                _ => {}
            }
        }
    }
    if let ExprTerms::Every(ev) = &e.terms {
        for x in &ev.body {
            declared_in_expr(x, out);
        }
    }
}

/// resolveRefsInRule.
fn resolve_rule(globals: &HashMap<Var, Vec<Term>>, rule: &mut Rule) -> Result<(), String> {
    let mut arg_vars = VarSet::new();
    for a in &rule.head.args {
        collect_arg_vars(a, &mut arg_vars)?;
    }
    let mut ignore: Vec<VarSet> = vec![arg_vars, declared_vars(&rule.body)];
    if rule.head.reference.is_empty() {
        rule.head.reference = rule.head.ref_path();
    }
    for t in rule.head.reference.iter_mut().skip(1) {
        resolve_term(globals, &mut ignore, t);
    }
    if let Some(k) = rule.head.key.as_mut() {
        resolve_term(globals, &mut ignore, k);
    }
    if let Some(v) = rule.head.value.as_mut() {
        resolve_term(globals, &mut ignore, v);
    }
    for e in rule.body.iter_mut() {
        resolve_expr(globals, &mut ignore, e);
    }
    Ok(())
}

/// The variables of the arguments, refusing `data` and `input` refs.
fn collect_arg_vars(t: &Term, out: &mut VarSet) -> Result<(), String> {
    match &t.value {
        TermValue::Var(v) => {
            out.insert(v.clone());
        }
        TermValue::Ref(r) => {
            if let [only] = r.as_slice()
                && matches!(only.as_var(), Some("data" | "input"))
            {
                return Err(format!("args must not shadow {t} (use a different variable name)"));
            }
            for x in r {
                collect_arg_vars(x, out)?;
            }
        }
        TermValue::Object(o) => {
            for (_, v) in vars::sorted_pairs(o) {
                collect_arg_vars(v, out)?;
            }
        }
        TermValue::Array(a) => {
            for x in a {
                collect_arg_vars(x, out)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn ignored(ignore: &[VarSet], v: &str) -> bool {
    ignore.iter().any(|s| s.contains(v))
}

fn resolve_expr(globals: &HashMap<Var, Vec<Term>>, ignore: &mut Vec<VarSet>, e: &mut Expr) {
    match &mut e.terms {
        ExprTerms::Term(t) => resolve_term(globals, ignore, t),
        ExprTerms::Call(c) => c.iter_mut().for_each(|t| resolve_term(globals, ignore, t)),
        ExprTerms::Some(d) => {
            if let Some(call) = d.symbols.first_mut()
                && let TermValue::Call(c) = &mut call.value
            {
                c.iter_mut().for_each(|t| resolve_term(globals, ignore, t));
            }
        }
        ExprTerms::Every(ev) => {
            let mut locals = VarSet::new();
            if let Some(k) = &ev.key {
                locals.extend(vars::term_vars(k));
            }
            locals.extend(vars::term_vars(&ev.value));
            ignore.push(locals);
            resolve_term(globals, ignore, &mut ev.domain);
            for x in ev.body.iter_mut() {
                resolve_expr(globals, ignore, x);
            }
            ignore.pop();
        }
    }
    for w in e.with.iter_mut() {
        resolve_term(globals, ignore, &mut w.target);
        resolve_term(globals, ignore, &mut w.value);
    }
}

fn resolve_term(globals: &HashMap<Var, Vec<Term>>, ignore: &mut Vec<VarSet>, t: &mut Term) {
    let loc = t.loc.clone();
    match &mut t.value {
        TermValue::Var(v) => {
            if let Some(g) = globals.get(v)
                && !ignored(ignore, v)
            {
                let r: Vec<Term> = g.iter().map(|x| Term::new(x.value.clone(), loc.clone())).collect();
                t.value = TermValue::Ref(r);
            }
        }
        TermValue::Ref(r) => {
            let mut out = Vec::with_capacity(r.len());
            for (i, x) in r.iter_mut().enumerate() {
                match &x.value {
                    TermValue::Var(v) if globals.contains_key(v) && !ignored(ignore, v) => {
                        let g: Vec<Term> = globals.get(v).cloned().unwrap_or_default().into_iter().map(|y| Term::new(y.value, x.loc.clone())).collect();
                        if i == 0 {
                            out = g;
                        } else {
                            out.push(Term::reference(g, x.loc.clone()));
                        }
                    }
                    TermValue::Ref(_)
                    | TermValue::Array(_)
                    | TermValue::Object(_)
                    | TermValue::Set(_)
                    | TermValue::ArrayCompr(..)
                    | TermValue::SetCompr(..)
                    | TermValue::ObjectCompr(..)
                    | TermValue::Call(_) => {
                        let mut y = x.clone();
                        resolve_term(globals, ignore, &mut y);
                        out.push(y);
                    }
                    _ => out.push(x.clone()),
                }
            }
            *r = out;
        }
        TermValue::Object(o) => {
            let mut pairs: Vec<(Term, Term)> = vars::sorted_pairs(o).into_iter().cloned().collect();
            for (k, v) in pairs.iter_mut() {
                resolve_term(globals, ignore, k);
                resolve_term(globals, ignore, v);
            }
            *o = pairs;
        }
        TermValue::Array(a) | TermValue::Call(a) => a.iter_mut().for_each(|x| resolve_term(globals, ignore, x)),
        TermValue::Set(s) => {
            let mut items: Vec<Term> = vars::sorted_items(s).into_iter().cloned().collect();
            items.iter_mut().for_each(|x| resolve_term(globals, ignore, x));
            *s = items;
        }
        TermValue::ArrayCompr(term, body) | TermValue::SetCompr(term, body) => {
            ignore.push(declared_vars(body));
            resolve_term(globals, ignore, term);
            body.iter_mut().for_each(|e| resolve_expr(globals, ignore, e));
            ignore.pop();
        }
        TermValue::ObjectCompr(k, v, body) => {
            ignore.push(declared_vars(body));
            resolve_term(globals, ignore, k);
            resolve_term(globals, ignore, v);
            body.iter_mut().for_each(|e| resolve_expr(globals, ignore, e));
            ignore.pop();
        }
        TermValue::TemplateString { parts, .. } => {
            for p in parts.iter_mut() {
                if let crate::ast::TemplatePart::Expr(e) = p {
                    resolve_expr(globals, ignore, e);
                }
            }
        }
        _ => {}
    }
}

/// A body consisting of one `true`.
pub fn is_empty_body(b: &Body) -> bool {
    matches!(b.as_slice(), [e] if matches!(&e.terms, ExprTerms::Term(t) if matches!(t.value, TermValue::Bool(true))))
}
