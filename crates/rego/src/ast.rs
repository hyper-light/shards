//! OPA's syntax tree (ast/term.go, ast/policy.go) and its text, as OPA's `AppendText`
//! writes it (term_appenders.go, policy_appenders.go): the text error messages, partial
//! results and `print` show.

use std::fmt::Write as _;
use std::rc::Rc;

use crate::goquote;
use crate::scanner::Token;
use crate::value::Number;

/// Where a node is in its module. OPA's Location also lists the columns of the line's
/// tabs, which only its formatter reads: kept, they made each term's location a copy of
/// that list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Location {
    pub file: Rc<str>,
    pub row: usize,
    pub col: usize,
    pub offset: usize,
}

impl Location {
    /// location.Location.Format("%s", msg) as buildx logs a print: `file:row: msg`.
    pub fn format(&self, msg: &str) -> String {
        if self.file.is_empty() {
            format!("{}:{}: {msg}", self.row, self.col)
        } else {
            format!("{}:{}: {msg}", self.file, self.row)
        }
    }
}

#[derive(Debug, Clone)]
pub struct Term {
    pub value: TermValue,
    pub loc: Option<Location>,
}

/// A template string's part: a string, or an expression (OPA's `[]Node`).
#[derive(Debug, Clone)]
pub enum TemplatePart {
    Term(Term),
    Expr(Box<Expr>),
}

/// A node's children, shared until changed: OPA's nodes are pointers, so that a term met
/// again (the parser's cache of what it parsed, the compiler's copies of rules) costs
/// nothing to keep; one changed is copied first (`Rc::make_mut`).
#[derive(Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Shared<T>(Rc<T>);

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Shared(Rc::clone(&self.0))
    }
}

impl<T> std::ops::Deref for Shared<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: Clone> std::ops::DerefMut for Shared<T> {
    fn deref_mut(&mut self) -> &mut T {
        Rc::make_mut(&mut self.0)
    }
}

impl<T> From<T> for Shared<T> {
    fn from(v: T) -> Self {
        Shared(Rc::new(v))
    }
}

impl<T: std::fmt::Display> std::fmt::Display for Shared<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<T: Clone> Shared<T> {
    /// The value, unshared: moved out where nothing else holds it, else copied.
    pub fn into_inner(self) -> T {
        Rc::unwrap_or_clone(self.0)
    }
}

impl<T> Shared<T> {
    /// The value, where nothing else holds it.
    pub fn get_mut(this: &mut Shared<T>) -> Option<&mut T> {
        Rc::get_mut(&mut this.0)
    }

    /// Whether both share one value.
    pub fn ptr_eq(a: &Shared<T>, b: &Shared<T>) -> bool {
        Rc::ptr_eq(&a.0, &b.0)
    }
}

impl<A> FromIterator<A> for Shared<Vec<A>> {
    fn from_iter<I: IntoIterator<Item = A>>(iter: I) -> Self {
        Shared(Rc::new(iter.into_iter().collect()))
    }
}

impl<'a, T> IntoIterator for &'a Shared<Vec<T>> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

#[derive(Debug, Clone)]
pub enum TermValue {
    Null,
    Bool(bool),
    Number(Number),
    String(Rc<str>),
    Var(Rc<str>),
    Ref(Shared<Vec<Term>>),
    Array(Shared<Vec<Term>>),
    /// Keys unique, in insertion order (OPA's object keeps both).
    Object(Shared<Vec<(Term, Term)>>),
    /// Members unique, in insertion order.
    Set(Shared<Vec<Term>>),
    ArrayCompr(Shared<Term>, Shared<Body>),
    SetCompr(Shared<Term>, Shared<Body>),
    ObjectCompr(Shared<Term>, Shared<Term>, Shared<Body>),
    Call(Shared<Vec<Term>>),
    TemplateString {
        multi_line: bool,
        parts: Vec<TemplatePart>,
    },
}

/// A value made a term (a builtin's result, a document read) nests as deep as the value:
/// the last owner of a collection drops its members one after another. What only the
/// parser builds (comprehensions, templates) nests no deeper than its limit and drops
/// as Rust drops it.
impl Drop for TermValue {
    fn drop(&mut self) {
        let mut rest = Vec::new();
        take_members(self, &mut rest);
        while let Some(mut t) = rest.pop() {
            take_members(&mut t.value, &mut rest);
        }
    }
}

impl TermValue {
    /// An array's members, taken out of it (copied where shared); none for another value.
    pub fn take_array(&mut self) -> Option<Vec<Term>> {
        match self {
            TermValue::Array(a) => Some(std::mem::take(a).into_inner()),
            _ => None,
        }
    }

    /// A set's members, taken out of it (copied where shared); none for another value.
    pub fn take_set(&mut self) -> Option<Vec<Term>> {
        match self {
            TermValue::Set(s) => Some(std::mem::take(s).into_inner()),
            _ => None,
        }
    }

    /// An object's pairs, taken out of it (copied where shared); none for another value.
    pub fn take_object(&mut self) -> Option<Vec<(Term, Term)>> {
        match self {
            TermValue::Object(o) => Some(std::mem::take(o).into_inner()),
            _ => None,
        }
    }

    /// A call's terms, taken out of it (copied where shared); none for another value.
    pub fn take_call(&mut self) -> Option<Vec<Term>> {
        match self {
            TermValue::Call(c) => Some(std::mem::take(c).into_inner()),
            _ => None,
        }
    }
}

/// Moves a collection's members to `out` when this is its last owner.
fn take_members(v: &mut TermValue, out: &mut Vec<Term>) {
    match v {
        TermValue::Ref(a) | TermValue::Array(a) | TermValue::Set(a) | TermValue::Call(a) => {
            if let Some(a) = Shared::get_mut(a) {
                out.append(a);
            }
        }
        TermValue::Object(o) => {
            if let Some(o) = Shared::get_mut(o) {
                for (k, v) in o.drain(..) {
                    out.push(k);
                    out.push(v);
                }
            }
        }
        _ => {}
    }
}

impl Term {
    pub fn new(value: TermValue, loc: Option<Location>) -> Term {
        Term { value, loc }
    }

    pub fn var(name: &str, loc: Option<Location>) -> Term {
        Term::new(TermValue::Var(name.into()), loc)
    }

    pub fn string(s: &str, loc: Option<Location>) -> Term {
        Term::new(TermValue::String(s.into()), loc)
    }

    pub fn boolean(b: bool, loc: Option<Location>) -> Term {
        Term::new(TermValue::Bool(b), loc)
    }

    pub fn reference(parts: Vec<Term>, loc: Option<Location>) -> Term {
        Term::new(TermValue::Ref(parts.into()), loc)
    }

    pub fn as_var(&self) -> Option<&str> {
        match &self.value {
            TermValue::Var(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_ref(&self) -> Option<&[Term]> {
        match &self.value {
            TermValue::Ref(r) => Some(r),
            _ => None,
        }
    }

    pub fn as_string(&self) -> Option<&str> {
        match &self.value {
            TermValue::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_call(&self) -> Option<&[Term]> {
        match &self.value {
            TermValue::Call(c) => Some(c),
            _ => None,
        }
    }

    /// Term.IsGround: no variables anywhere.
    /// Whether the term is a value: scalars and collections of them, no variable, ref,
    /// call or comprehension anywhere in it (a member at a time).
    pub fn is_value(&self) -> bool {
        let mut todo = vec![self];
        while let Some(t) = todo.pop() {
            match &t.value {
                TermValue::Null | TermValue::Bool(_) | TermValue::Number(_) | TermValue::String(_) => {}
                TermValue::Array(a) | TermValue::Set(a) => todo.extend(a.iter()),
                TermValue::Object(o) => todo.extend(o.iter().flat_map(|(k, v)| [k, v])),
                _ => return false,
            }
        }
        true
    }

    /// Whether the term has no variables, a member at a time (a term made of a value nests
    /// as deep as the value).
    pub fn is_ground(&self) -> bool {
        let mut todo = vec![self];
        while let Some(t) = todo.pop() {
            match &t.value {
                TermValue::Null | TermValue::Bool(_) | TermValue::Number(_) | TermValue::String(_) => {}
                // A ref's head is a variable naming a document; only the rest counts.
                TermValue::Ref(r) => todo.extend(r.iter().skip(1)),
                TermValue::Array(a) | TermValue::Set(a) | TermValue::Call(a) => todo.extend(a.iter()),
                TermValue::Object(o) => todo.extend(o.iter().flat_map(|(k, v)| [k, v])),
                TermValue::Var(_)
                | TermValue::ArrayCompr(..)
                | TermValue::SetCompr(..)
                | TermValue::ObjectCompr(..)
                | TermValue::TemplateString { .. } => return false,
            }
        }
        true
    }

    /// Equality as OPA's Compare decides it (numbers by NumberCompare), locations aside.
    pub fn equal(&self, other: &Term) -> bool {
        crate::compare::term_compare(self, other) == std::cmp::Ordering::Equal
    }

    /// OPA's `ValueName`, for error messages.
    pub fn value_name(&self) -> &'static str {
        match &self.value {
            TermValue::Null => "null",
            TermValue::Bool(_) => "boolean",
            TermValue::Number(_) => "number",
            TermValue::String(_) => "string",
            TermValue::Var(_) => "var",
            TermValue::Ref(_) => "ref",
            TermValue::Array(_) => "array",
            TermValue::Object(_) => "object",
            TermValue::Set(_) => "set",
            TermValue::ArrayCompr(..) => "arraycomprehension",
            TermValue::SetCompr(..) => "setcomprehension",
            TermValue::ObjectCompr(..) => "objectcomprehension",
            TermValue::Call(_) => "call",
            TermValue::TemplateString { .. } => "templatestring",
        }
    }
}

/// Makes an object term, a repeated key's value replacing the earlier one.
pub fn object_term(pairs: Vec<(Term, Term)>, loc: Option<Location>) -> Term {
    let mut out: Vec<(Term, Term)> = Vec::with_capacity(pairs.len());
    for (k, v) in pairs {
        if let Some(slot) = out.iter_mut().find(|(ek, _)| ek.equal(&k)) {
            slot.1 = v;
        } else {
            out.push((k, v));
        }
    }
    Term::new(TermValue::Object(out.into()), loc)
}

/// Makes a set term, repeated members kept once.
pub fn set_term(items: Vec<Term>, loc: Option<Location>) -> Term {
    let mut out: Vec<Term> = Vec::with_capacity(items.len());
    for t in items {
        if !out.iter().any(|e| e.equal(&t)) {
            out.push(t);
        }
    }
    Term::new(TermValue::Set(out.into()), loc)
}

#[derive(Debug, Clone)]
pub struct With {
    pub target: Term,
    pub value: Term,
    pub loc: Option<Location>,
}

#[derive(Debug, Clone)]
pub struct SomeDecl {
    pub symbols: Vec<Term>,
    pub loc: Option<Location>,
}

#[derive(Debug, Clone)]
pub struct Every {
    pub key: Option<Term>,
    pub value: Term,
    pub domain: Term,
    pub body: Body,
    pub loc: Option<Location>,
}

#[derive(Debug, Clone)]
pub enum ExprTerms {
    Term(Box<Term>),
    Call(Vec<Term>),
    Some(SomeDecl),
    Every(Box<Every>),
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub index: usize,
    pub negated: bool,
    pub generated: bool,
    pub terms: ExprTerms,
    pub with: Vec<With>,
    pub loc: Option<Location>,
}

pub type Body = Vec<Expr>;

impl Expr {
    pub fn new(terms: ExprTerms, loc: Option<Location>) -> Expr {
        Expr {
            index: 0,
            negated: false,
            generated: false,
            terms,
            with: Vec::new(),
            loc,
        }
    }

    pub fn term(t: Term) -> Expr {
        let loc = t.loc.clone();
        Expr::new(ExprTerms::Term(Box::new(t)), loc)
    }

    /// The operator's name of a call expression.
    pub fn operator_name(&self) -> Option<String> {
        match &self.terms {
            ExprTerms::Call(c) => c.first().map(Term::to_string),
            _ => None,
        }
    }

    pub fn is_equality(&self) -> bool {
        self.operator_name().as_deref() == Some("eq")
    }

    pub fn is_assignment(&self) -> bool {
        self.operator_name().as_deref() == Some("assign")
    }

    pub fn is_call(&self) -> bool {
        matches!(self.terms, ExprTerms::Call(_))
    }

    pub fn is_every(&self) -> bool {
        matches!(self.terms, ExprTerms::Every(_))
    }

    pub fn is_some(&self) -> bool {
        matches!(self.terms, ExprTerms::Some(_))
    }

    /// Expr.Operand(i): the i-th argument of a call.
    pub fn operand(&self, i: usize) -> Option<&Term> {
        match &self.terms {
            ExprTerms::Call(c) => c.get(i + 1),
            _ => None,
        }
    }
}

/// A body of one `true` expression, as the parser makes for rules without one.
pub fn true_body(loc: Option<Location>) -> Body {
    vec![Expr::new(
        ExprTerms::Term(Box::new(Term::boolean(true, loc.clone()))),
        loc,
    )]
}

#[derive(Debug, Clone, Default)]
pub struct Head {
    pub name: Option<Rc<str>>,
    pub reference: Vec<Term>,
    pub args: Vec<Term>,
    pub key: Option<Term>,
    pub value: Option<Term>,
    pub assign: bool,
    pub generated_value: bool,
    pub keywords: Vec<Token>,
    pub loc: Option<Location>,
}

/// How a rule defines its document (ast.RuleKind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    SingleValue,
    MultiValue,
}

impl Head {
    /// VarHead.
    pub fn var(name: &str, loc: Option<Location>) -> Head {
        Head {
            name: Some(name.into()),
            reference: vec![Term::var(name, loc.clone())],
            loc,
            ..Head::default()
        }
    }

    /// RefHead: named when the ref is one variable.
    pub fn reference(r: Vec<Term>, value: Option<Term>) -> Head {
        let name = if r.len() < 2 {
            r.first().and_then(Term::as_var).map(Rc::from)
        } else {
            None
        };
        Head {
            name,
            reference: r,
            value,
            ..Head::default()
        }
    }

    /// Head.Ref().
    pub fn ref_path(&self) -> Vec<Term> {
        if let Some(name) = &self.name
            && self.reference.is_empty()
        {
            return vec![Term::var(name, self.loc.clone())];
        }
        self.reference.clone()
    }

    /// Head.RuleKind.
    pub fn kind(&self) -> RuleKind {
        if self.key.is_some() && self.value.is_none() {
            RuleKind::MultiValue
        } else {
            RuleKind::SingleValue
        }
    }
}

/// A rule, and its else branches in a chain. A rule may have more branches than a thread
/// has stack for frames (OPA takes 100000 and more), so what walks the chain loops over
/// it: cloning, dropping and printing a rule included.
pub struct Rule {
    pub default: bool,
    pub head: Head,
    pub body: Body,
    pub else_: Option<Box<Rule>>,
    pub loc: Option<Location>,
    pub generated_body: bool,
}

impl Rule {
    pub fn is_function(&self) -> bool {
        !self.head.args.is_empty()
    }

    /// This branch alone, its else branches left out.
    pub fn branch(&self) -> Rule {
        Rule {
            default: self.default,
            head: self.head.clone(),
            body: self.body.clone(),
            else_: None,
            loc: self.loc.clone(),
            generated_body: self.generated_body,
        }
    }

    /// This rule and its else branches, in order.
    pub fn branches(&self) -> impl Iterator<Item = &Rule> {
        std::iter::successors(Some(self), |r| r.else_.as_deref())
    }
}

impl Clone for Rule {
    fn clone(&self) -> Rule {
        let mut tail = None;
        let rest: Vec<&Rule> = self.branches().skip(1).collect();
        for r in rest.into_iter().rev() {
            let mut b = r.branch();
            b.else_ = tail;
            tail = Some(Box::new(b));
        }
        let mut out = self.branch();
        out.else_ = tail;
        out
    }
}

impl Drop for Rule {
    fn drop(&mut self) {
        let mut e = self.else_.take();
        while let Some(mut r) = e {
            e = r.else_.take();
        }
    }
}

impl std::fmt::Debug for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        /// A branch's fields but its else.
        struct Branch<'a>(&'a Rule);
        impl std::fmt::Debug for Branch<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("Rule")
                    .field("default", &self.0.default)
                    .field("head", &self.0.head)
                    .field("body", &self.0.body)
                    .field("loc", &self.0.loc)
                    .field("generated_body", &self.0.generated_body)
                    .finish()
            }
        }
        let elses: Vec<Branch<'_>> = self.branches().skip(1).map(Branch).collect();
        f.debug_struct("Rule")
            .field("default", &self.default)
            .field("head", &self.head)
            .field("body", &self.body)
            .field("else", &elses)
            .field("loc", &self.loc)
            .field("generated_body", &self.generated_body)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct Package {
    pub path: Vec<Term>,
    pub loc: Option<Location>,
}

#[derive(Debug, Clone)]
pub struct Import {
    pub path: Term,
    pub alias: Option<Rc<str>>,
    pub loc: Option<Location>,
}

#[derive(Debug, Clone)]
pub struct Module {
    pub package: Package,
    pub imports: Vec<Import>,
    pub rules: Vec<Rule>,
}

/// The builtins' names (OPA's `BuiltinMap`), for printing their refs.
pub fn is_builtin_name(name: &str) -> bool {
    crate::builtins::registry().contains_key(name)
}

/// IsVarCompatibleString.
pub fn is_var_compatible(s: &str) -> bool {
    let mut b = s.bytes();
    match b.next() {
        Some(c) if c.is_ascii_alphabetic() || c == b'_' => {}
        _ => return false,
    }
    b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// OPA's KeywordsV1.
pub fn is_keyword(s: &str) -> bool {
    matches!(
        s,
        "not"
            | "package"
            | "import"
            | "as"
            | "default"
            | "else"
            | "with"
            | "null"
            | "true"
            | "false"
            | "some"
            | "if"
            | "contains"
            | "in"
            | "every"
    )
}

/// BuiltinNameFromRef.
fn builtin_name_from_ref(r: &[Term]) -> Option<String> {
    let (first, rest) = r.split_first()?;
    let mut name = first.as_var()?.to_string();
    for t in rest {
        name.push('.');
        name.push_str(t.as_string()?);
    }
    is_builtin_name(&name).then_some(name)
}

fn write_delimited<T: std::fmt::Display>(out: &mut String, items: &[T], delim: &str) {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(delim);
        }
        let _ = write!(out, "{item}");
    }
}

/// strconv.Quote of a string.
pub fn quoted(s: &str) -> String {
    let mut out = String::new();
    goquote::quote(&mut out, s);
    out
}

pub fn write_ref(out: &mut String, r: &[Term]) {
    let Some((first, rest)) = r.split_first() else {
        return;
    };
    if rest.is_empty() {
        match &first.value {
            TermValue::String(s) => out.push_str(s),
            _ => {
                let _ = write!(out, "{first}");
            }
        }
        return;
    }
    if let Some(name) = builtin_name_from_ref(r) {
        out.push_str(&name);
        return;
    }
    match &first.value {
        TermValue::String(s) => out.push_str(s),
        _ => {
            let _ = write!(out, "{first}");
        }
    }
    for p in rest {
        match &p.value {
            TermValue::String(s) => {
                if is_var_compatible(s) && !is_keyword(s) {
                    out.push('.');
                    out.push_str(s);
                } else {
                    out.push('[');
                    if s.chars().any(|c| c == '\\' || goquote::is_control(c)) {
                        out.push_str(&quoted(s));
                    } else {
                        out.push('"');
                        out.push_str(s);
                        out.push('"');
                    }
                    out.push(']');
                }
            }
            _ => {
                let _ = write!(out, "[{p}]");
            }
        }
    }
}

fn count_unescaped_left_curly(s: &str) -> usize {
    let n = s.matches('{').count();
    if n > 0 {
        n.saturating_sub(s.matches("\\{").count())
    } else {
        n
    }
}

/// AppendEscapedTemplateStringStringPart: a `\\` before each `{` not escaped already.
fn escape_template_part(out: &mut String, s: &str) {
    let b = s.as_bytes();
    let mut buf = Vec::with_capacity(b.len() + 4);
    for (i, &c) in b.iter().enumerate() {
        let escaped = i > 0 && b.get(i - 1) == Some(&b'\\');
        if c == b'{' && !escaped {
            buf.push(b'\\');
        }
        buf.push(c);
    }
    out.push_str(&String::from_utf8_lossy(&buf));
}

impl std::fmt::Display for Term {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = String::new();
        match &self.value {
            TermValue::Null => out.push_str("null"),
            TermValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            TermValue::Number(n) => out.push_str(n.text()),
            TermValue::String(s) => goquote::quote(&mut out, s),
            TermValue::Var(v) => {
                if v.starts_with('$') || &**v == "_" {
                    out.push('_');
                } else {
                    out.push_str(v);
                }
            }
            TermValue::Ref(r) => write_ref(&mut out, r),
            TermValue::Array(a) => {
                out.push('[');
                write_delimited(&mut out, a, ", ");
                out.push(']');
            }
            TermValue::Object(o) => {
                if o.is_empty() {
                    out.push_str("{}");
                } else {
                    let mut sorted: Vec<&(Term, Term)> = o.iter().collect();
                    sorted.sort_by(|a, b| crate::compare::term_compare(&a.0, &b.0));
                    out.push('{');
                    for (i, (k, v)) in sorted.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        let _ = write!(out, "{k}: {v}");
                    }
                    out.push('}');
                }
            }
            TermValue::Set(s) => {
                if s.is_empty() {
                    out.push_str("set()");
                } else {
                    let mut sorted: Vec<&Term> = s.iter().collect();
                    sorted.sort_by(|a, b| crate::compare::term_compare(a, b));
                    out.push('{');
                    for (i, t) in sorted.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        let _ = write!(out, "{t}");
                    }
                    out.push('}');
                }
            }
            TermValue::ArrayCompr(t, body) => {
                let _ = write!(out, "[{t} | {}]", BodyText(body));
            }
            TermValue::SetCompr(t, body) => {
                let _ = write!(out, "{{{t} | {}}}", BodyText(body));
            }
            TermValue::ObjectCompr(k, v, body) => {
                let _ = write!(out, "{{{k}: {v} | {}}}", BodyText(body));
            }
            TermValue::Call(c) => write_call(&mut out, c),
            TermValue::TemplateString { parts, .. } => {
                out.push_str("$\"");
                for p in parts {
                    match p {
                        TemplatePart::Expr(e) => {
                            let _ = write!(out, "{{{e}}}");
                        }
                        TemplatePart::Term(t) => match &t.value {
                            TermValue::String(s) => {
                                let ulc = count_unescaped_left_curly(s);
                                let q = quoted(s);
                                let sl = q.len() + ulc - 2;
                                if sl == s.len() {
                                    out.push_str(s);
                                } else if sl == s.len() + ulc {
                                    escape_template_part(&mut out, s);
                                } else {
                                    let inner = q.get(1..q.len() - 1).unwrap_or_default().to_string();
                                    escape_template_part(&mut out, &inner);
                                }
                            }
                            _ => {
                                let _ = write!(out, "{t}");
                            }
                        },
                    }
                }
                out.push('"');
            }
        }
        f.write_str(&out)
    }
}

fn write_call(out: &mut String, c: &[Term]) {
    let Some((op, args)) = c.split_first() else { return };
    let _ = write!(out, "{op}(");
    write_delimited(out, args, ", ");
    out.push(')');
}

/// A body's text: its expressions joined by "; ".
#[derive(Debug)]
pub struct BodyText<'a>(pub &'a [Expr]);

impl std::fmt::Display for BodyText<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, e) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{e}")?;
        }
        Ok(())
    }
}

impl std::fmt::Display for With {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "with {} as {}", self.target, self.value)
    }
}

impl std::fmt::Display for Expr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = String::new();
        if self.negated {
            out.push_str("not ");
        }
        match &self.terms {
            ExprTerms::Call(t) => {
                if self.is_equality() && t.len() == 3 {
                    let (a, b) = (t.get(1), t.get(2));
                    if let (Some(a), Some(b)) = (a, b) {
                        let _ = write!(out, "{a} = {b}");
                    }
                } else {
                    write_call(&mut out, t);
                }
            }
            ExprTerms::Term(t) => {
                let _ = write!(out, "{t}");
            }
            ExprTerms::Some(d) => {
                out.push_str("some ");
                if let Some(call) = d.symbols.first().and_then(Term::as_call) {
                    if let (Some(a), Some(b)) = (call.get(1), call.get(2)) {
                        let _ = write!(out, "{a}");
                        out.push_str(if call.len() == 3 { " in " } else { ", " });
                        let _ = write!(out, "{b}");
                    }
                    if call.len() == 4
                        && let Some(c) = call.get(3)
                    {
                        let _ = write!(out, " in {c}");
                    }
                } else {
                    write_delimited(&mut out, &d.symbols, ", ");
                }
            }
            ExprTerms::Every(e) => {
                out.push_str("every ");
                if let Some(k) = &e.key {
                    let _ = write!(out, "{k}, ");
                }
                let _ = write!(out, "{} in {} {{ {} }}", e.value, e.domain, BodyText(&e.body));
            }
        }
        if !self.with.is_empty() {
            out.push(' ');
        }
        write_delimited(&mut out, &self.with, " ");
        f.write_str(&out)
    }
}

impl Head {
    fn write(&self, out: &mut String) {
        if self.reference.is_empty() {
            out.push_str(self.name.as_deref().unwrap_or_default());
        } else {
            write_ref(out, &self.reference);
        }
        let mut contains_added = false;
        if !self.args.is_empty() {
            out.push('(');
            write_delimited(out, &self.args, ", ");
            out.push(')');
        } else if self.reference.len() == 1
            && let Some(k) = &self.key
        {
            let _ = write!(out, " contains {k}");
            contains_added = true;
        }
        if let Some(v) = &self.value {
            out.push_str(if self.assign { " := " } else { " = " });
            let _ = write!(out, "{v}");
        } else if !contains_added
            && self.name.is_none()
            && let Some(k) = &self.key
        {
            let _ = write!(out, " contains {k}");
        }
    }
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = String::new();
        if self.default {
            out.push_str("default ");
        }
        self.head.write(&mut out);
        if !self.default {
            let _ = write!(out, " if {{ {} }}", BodyText(&self.body));
        }
        let mut e = self.else_.as_deref();
        while let Some(r) = e {
            out.push_str(" else ");
            if let Some(v) = &r.head.value {
                let _ = write!(out, "= {v}");
            }
            let _ = write!(out, " if {{ {} }}", BodyText(&r.body));
            e = r.else_.as_deref();
        }
        f.write_str(&out)
    }
}

impl std::fmt::Display for Module {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = String::new();
        if self.package.path.len() <= 1 {
            out.push_str("package <illegal path \"");
            write_ref(&mut out, &self.package.path);
            out.push_str("\">");
        } else {
            out.push_str("package ");
            write_ref(&mut out, self.package.path.get(1..).unwrap_or_default());
        }
        out.push('\n');
        if !self.imports.is_empty() {
            for imp in &self.imports {
                let _ = write!(out, "\nimport {}", imp.path);
                if let Some(a) = &imp.alias {
                    let _ = write!(out, " as {a}");
                }
            }
            out.push('\n');
        }
        for r in &self.rules {
            let _ = write!(out, "\n{r}");
        }
        f.write_str(&out)
    }
}
