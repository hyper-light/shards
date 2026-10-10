//! OPA's parser (ast/parser.go, ast/parser_ext.go, v1.14.1), for Rego v1 with the
//! capabilities buildx gives it: the features `rego_v1`, `keywords_in_refs` and
//! `template_strings`, no further future keywords, annotations not processed.

use crate::ast::{
    self, Body, Every, Expr, ExprTerms, Head, Import, Location, Module, Package, Rule, SomeDecl,
    TemplatePart, Term, TermValue, With,
};
use crate::number::Float;
use crate::scanner::{Scanner, Token};
use crate::value::Number;

/// OPA's `ast.Error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub code: &'static str,
    pub message: String,
    pub loc: Option<Location>,
    /// The source line and the offending byte's index in it.
    pub details: Option<(String, usize)>,
    /// A type error's details (ErrorDetails.Lines), each written on its own line.
    pub lines: Vec<String>,
}

pub const PARSE_ERR: &str = "rego_parse_error";

/// A module's parse errors: OPA's `ast.Errors`, or the single `*ast.Error` it returns for
/// an empty module, each written as OPA writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    One(Box<Error>),
    Many(Vec<Error>),
}

impl ParseError {
    pub fn errors(&self) -> Vec<Error> {
        match self {
            ParseError::One(e) => vec![(**e).clone()],
            ParseError::Many(v) => v.clone(),
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::One(e) => write!(f, "{e}"),
            ParseError::Many(v) => match v.as_slice() {
                [] => f.write_str("no error(s)"),
                [e] => write!(f, "1 error occurred: {e}"),
                v => {
                    write!(f, "{} errors occurred:", v.len())?;
                    for e in v {
                        write!(f, "\n{e}")?;
                    }
                    Ok(())
                }
            },
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(l) = &self.loc {
            if l.file.is_empty() {
                write!(f, "{}:{}: ", l.row, l.col)?;
            } else {
                write!(f, "{}:{}: ", l.file, l.row)?;
            }
        }
        write!(f, "{}: {}", self.code, self.message)?;
        if let Some((line, idx)) = &self.details {
            let trimmed = line.trim_start_matches('\t');
            let tabs = line.len() - trimmed.len();
            let indent = idx.saturating_sub(tabs);
            write!(f, "\n\t{trimmed}\n\t{}^", " ".repeat(indent))?;
        }
        for line in &self.lines {
            write!(f, "\n\t{line}")?;
        }
        Ok(())
    }
}

impl Error {
    pub fn new(code: &'static str, loc: Option<Location>, message: String) -> Error {
        Error {
            code,
            message,
            loc,
            details: None,
            lines: Vec::new(),
        }
    }
}

/// newParserErrorDetail.
fn detail(bs: &[u8], offset: usize) -> (String, usize) {
    if bs.is_empty() {
        return (String::new(), 0);
    }
    let mut offset = offset.min(bs.len() - 1);
    let space = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c | 0x85 | 0xa0);
    while offset > 0 && bs.get(offset).is_some_and(|&b| space(b)) {
        offset -= 1;
    }
    let nl = |b: Option<&u8>| matches!(b, Some(b'\r' | b'\n'));
    let mut begin = offset;
    while begin > 0 && !nl(bs.get(begin)) {
        begin -= 1;
    }
    if nl(bs.get(begin)) {
        begin += 1;
    }
    let mut end = offset;
    while end < bs.len() && !nl(bs.get(end)) {
        end += 1;
    }
    begin = begin.min(end);
    let line = String::from_utf8_lossy(bs.get(begin..end).unwrap_or_default()).into_owned();
    (line, offset - begin)
}

const MAX_DEPTH: usize = 100_000;

#[derive(Debug, Clone)]
struct State<'a> {
    errors: Vec<Error>,
    comments: usize,
    hints: Vec<String>,
    s: Scanner<'a>,
    loc: Location,
    lit: String,
    last_end: usize,
    tok_end: usize,
    wildcard: usize,
    tok: Token,
    skipped_nl: bool,
}

#[derive(Debug, Clone)]
pub enum Statement {
    Package(Package),
    Import(Import),
    Rule(Box<Rule>),
    Body(Body),
}

#[derive(Debug)]
pub struct Parser<'a> {
    s: State<'a>,
    /// The parsed-term cache: offsets strictly decreasing from its end.
    cache: Vec<(usize, Option<Term>, State<'a>)>,
    depth: usize,
}

/// The future keywords Rego v1 has (futureKeywordsV0).
const V1_KEYWORDS: [(&str, Token); 4] = [
    ("in", Token::In),
    ("every", Token::Every),
    ("contains", Token::Contains),
    ("if", Token::If),
];

fn is_future_keyword(s: &str) -> bool {
    V1_KEYWORDS.iter().any(|(k, _)| *k == s)
}

/// Rego v1's rules on a statement list (parseModule).
pub fn parse_module(file: &str, src: &str) -> Result<Module, ParseError> {
    let stmts = parse_statements(file, src).map_err(ParseError::Many)?;
    let Some(first) = stmts.first() else {
        let loc = Location {
            file: file.into(),
            ..Location::default()
        };
        return Err(ParseError::One(Box::new(Error::new(
            PARSE_ERR,
            Some(loc),
            "empty module".into(),
        ))));
    };
    let mut errs = Vec::new();
    let package = match first {
        Statement::Package(p) => Some(p.clone()),
        other => {
            errs.push(Error::new(
                PARSE_ERR,
                statement_loc(other),
                "package expected".into(),
            ));
            None
        }
    };
    let mut imports = Vec::new();
    let mut rules = Vec::new();
    for stmt in stmts.iter().skip(1) {
        match stmt {
            Statement::Import(i) => imports.push(i.clone()),
            Statement::Rule(r) => rules.push((**r).clone()),
            Statement::Body(b) => match rule_from_body(b) {
                Ok(mut r) => {
                    r.generated_body = true;
                    rules.push(r);
                }
                Err(e) => {
                    let loc = b.first().and_then(|e| e.loc.clone());
                    errs.push(Error::new(PARSE_ERR, loc, e));
                }
            },
            Statement::Package(p) => {
                errs.push(Error::new(PARSE_ERR, p.loc.clone(), "unexpected package".into()))
            }
        }
    }
    for rule in &rules {
        let mut r = Some(rule);
        while let Some(x) = r {
            errs.extend(check_rego_v1_rule(x));
            r = x.else_.as_deref();
        }
    }
    if !errs.is_empty() {
        return Err(ParseError::Many(errs));
    }
    let Some(package) = package else {
        return Err(ParseError::Many(errs));
    };
    Ok(Module {
        package,
        imports,
        rules,
    })
}

fn statement_loc(s: &Statement) -> Option<Location> {
    match s {
        Statement::Package(p) => p.loc.clone(),
        Statement::Import(i) => i.loc.clone(),
        Statement::Rule(r) => r.loc.clone(),
        Statement::Body(b) => b.first().and_then(|e| e.loc.clone()),
    }
}

/// checkRegoV1Rule.
fn check_rego_v1_rule(rule: &Rule) -> Vec<Error> {
    let t = if rule.is_function() { "function" } else { "rule" };
    let mut errs = Vec::new();
    let name = rule.head.name.as_deref().unwrap_or_default();
    if rule.head.reference.len() < 2 && ast::is_keyword(name) {
        errs.push(Error::new(
            PARSE_ERR,
            rule.loc.clone(),
            format!("{name} keyword cannot be used for rule name"),
        ));
    }
    if rule.generated_body && rule.head.generated_value {
        errs.push(Error::new(
            PARSE_ERR,
            rule.loc.clone(),
            format!("{t} must have value assignment and/or body declaration"),
        ));
    }
    if !rule.generated_body && !rule.head.keywords.contains(&Token::If) && !rule.default {
        errs.push(Error::new(
            PARSE_ERR,
            rule.loc.clone(),
            format!("`if` keyword is required before {t} body"),
        ));
    }
    if rule.head.kind() == ast::RuleKind::MultiValue && !rule.head.keywords.contains(&Token::Contains) {
        errs.push(Error::new(
            PARSE_ERR,
            rule.loc.clone(),
            "`contains` keyword is required for partial set rules".into(),
        ));
    }
    errs
}

/// ParseRuleFromBody and the rule-from-expression forms it tries.
fn rule_from_body(body: &Body) -> Result<Rule, String> {
    let [expr] = body.as_slice() else {
        return Err("multiple expressions cannot be used for rule head".into());
    };
    if !expr.with.is_empty() {
        return Err("expressions using with keyword cannot be used for rule head".into());
    }
    if expr.negated {
        return Err("negated expressions cannot be used for rule head".into());
    }
    match &expr.terms {
        ExprTerms::Some(_) => Err("'some' declarations cannot be used for rule head".into()),
        ExprTerms::Term(term) => match &term.value {
            TermValue::Ref(r) if r.len() > 2 => complete_rule_with_dots(term),
            TermValue::Ref(_) => partial_set_rule(term),
            _ => Err(format!("{} cannot be used for rule name", term.value_name())),
        },
        ExprTerms::Every(_) => Err("expression cannot be used for rule head".into()),
        ExprTerms::Call(terms) => {
            if expr.is_equality() || expr.is_assignment() {
                let mut rule = complete_rule_from_eq(expr)?;
                if expr.is_assignment() {
                    rule.head.assign = true;
                }
                return Ok(rule);
            }
            let op = expr.operator_name().unwrap_or_default();
            if ast::is_builtin_name(&op) {
                return Err("rule name conflicts with built-in function".into());
            }
            rule_from_call(terms)
        }
    }
}

fn simple_rule(loc: Option<Location>, head: Head, body_loc: Option<Location>) -> Rule {
    Rule {
        default: false,
        head,
        body: ast::true_body(body_loc),
        else_: None,
        loc,
        generated_body: false,
    }
}

fn first_is_var(r: &[Term]) -> Result<(), String> {
    match r.first() {
        Some(t) if t.as_var().is_some() => Ok(()),
        _ => Err(format!("invalid rule head: {}", ref_text(r))),
    }
}

fn ref_text(r: &[Term]) -> String {
    let mut s = String::new();
    ast::write_ref(&mut s, r);
    s
}

fn complete_rule_from_eq(expr: &Expr) -> Result<Rule, String> {
    let (Some(lhs), Some(rhs)) = (expr.operand(0), expr.operand(1)) else {
        return Err("assignment requires two operands".into());
    };
    let mut rule = rule_from_call_eq(lhs, rhs)
        .or_else(|_| partial_object_rule(lhs, rhs))
        .or_else(|_| complete_rule(lhs, rhs))?;
    rule.loc = expr.loc.clone();
    rule.head.loc = expr.loc.clone();
    Ok(rule)
}

fn complete_rule(lhs: &Term, rhs: &Term) -> Result<Rule, String> {
    let mut head = match &lhs.value {
        TermValue::Var(v) => Head::var(v, lhs.loc.clone()),
        TermValue::Ref(r) => {
            first_is_var(r)?;
            if r.len() > 1 && r.last().is_some_and(|t| !t.is_ground()) {
                return Err("ref not ground".into());
            }
            Head::reference(r.to_vec(), None)
        }
        _ => return Err(format!("{} cannot be used for rule name", lhs.value_name())),
    };
    head.value = Some(rhs.clone());
    head.loc = lhs.loc.clone();
    let mut rule = simple_rule(lhs.loc.clone(), head, rhs.loc.clone());
    rule.generated_body = true;
    Ok(rule)
}

fn complete_rule_with_dots(term: &Term) -> Result<Rule, String> {
    let r = term.as_ref().unwrap_or_default();
    first_is_var(r)?;
    let mut head = Head::reference(r.to_vec(), Some(Term::boolean(true, term.loc.clone())));
    head.generated_value = true;
    head.loc = term.loc.clone();
    Ok(simple_rule(term.loc.clone(), head, term.loc.clone()))
}

fn partial_object_rule(lhs: &Term, rhs: &Term) -> Result<Rule, String> {
    let Some(r) = lhs.as_ref() else {
        return Err(format!("{} cannot be used as rule name", lhs.value_name()));
    };
    first_is_var(r)?;
    let mut head = Head::reference(r.to_vec(), Some(rhs.clone()));
    if let ([name, key], Some(v)) = (r, r.first().and_then(Term::as_var)) {
        let _ = name;
        head.name = Some(v.into());
        head.key = Some(key.clone());
    }
    head.loc = rhs.loc.clone();
    Ok(simple_rule(rhs.loc.clone(), head, rhs.loc.clone()))
}

fn partial_set_rule(term: &Term) -> Result<Rule, String> {
    let r = match term.as_ref() {
        Some(r) if r.len() > 1 => r,
        _ => return Err(format!("{}s cannot be used for rule head", term.value_name())),
    };
    first_is_var(r)?;
    let mut head = Head::reference(r.to_vec(), None);
    if let [first, key] = r {
        let v = first
            .as_var()
            .ok_or_else(|| format!("{}s cannot be used for rule head", term.value_name()))?;
        head = Head::var(v, first.loc.clone());
        head.key = Some(key.clone());
    }
    head.loc = term.loc.clone();
    Ok(simple_rule(term.loc.clone(), head, term.loc.clone()))
}

fn rule_from_call_eq(lhs: &Term, rhs: &Term) -> Result<Rule, String> {
    let call = lhs.as_call().ok_or("must be call")?;
    let (op, args) = call.split_first().ok_or("must be call")?;
    let r = op
        .as_ref()
        .ok_or_else(|| format!("{}s cannot be used in function signature", op.value_name()))?;
    first_is_var(r)?;
    let mut head = Head::reference(r.to_vec(), Some(rhs.clone()));
    head.loc = lhs.loc.clone();
    head.args = args.to_vec();
    Ok(simple_rule(lhs.loc.clone(), head, rhs.loc.clone()))
}

fn rule_from_call(terms: &[Term]) -> Result<Rule, String> {
    let Some((op, args)) = terms.split_first().filter(|(_, a)| !a.is_empty()) else {
        return Err("rule argument list must take at least one argument".into());
    };
    let loc = op.loc.clone();
    let r = op.as_ref().unwrap_or_default();
    first_is_var(r)?;
    let mut head = Head::reference(r.to_vec(), Some(Term::boolean(true, loc.clone())));
    head.loc = loc.clone();
    head.args = args.to_vec();
    Ok(simple_rule(loc.clone(), head, loc))
}

/// ParseStatementsWithOpts: the statements of a module, or the parse errors.
pub fn parse_statements(file: &str, src: &str) -> Result<Vec<Statement>, Vec<Error>> {
    let mut p = Parser::new(file, src.as_bytes());
    let stmts = p.parse();
    if p.s.errors.is_empty() {
        Ok(stmts)
    } else {
        Err(p.s.errors)
    }
}

fn op_ref(name: &str, loc: Option<Location>) -> Term {
    Term::reference(vec![Term::var(name, loc.clone())], loc)
}

fn member_ref(with_key: bool, loc: Option<Location>) -> Term {
    let name = if with_key { "member_3" } else { "member_2" };
    Term::reference(
        vec![
            Term::var("internal", loc.clone()),
            Term::string(name, loc.clone()),
        ],
        loc,
    )
}

impl<'a> Parser<'a> {
    fn new(file: &str, src: &'a [u8]) -> Parser<'a> {
        let mut s = Scanner::new(src);
        for (k, t) in V1_KEYWORDS {
            s.add_keyword(k, t);
        }
        Parser {
            s: State {
                errors: Vec::new(),
                comments: 0,
                hints: Vec::new(),
                s,
                loc: Location {
                    file: file.into(),
                    ..Location::default()
                },
                lit: String::new(),
                last_end: 0,
                tok_end: 0,
                wildcard: 0,
                tok: Token::Illegal,
                skipped_nl: false,
            },
            cache: Vec::new(),
            depth: 0,
        }
    }

    fn loc(&self) -> Option<Location> {
        Some(self.s.loc.clone())
    }

    fn save(&self) -> State<'a> {
        self.s.clone()
    }

    fn restore(&mut self, s: State<'a>) {
        self.s = s;
    }

    fn parse(&mut self) -> Vec<Statement> {
        self.scan();
        let mut stmts = Vec::new();
        while self.s.tok != Token::Eof {
            let s = self.save();
            if let Some(pkg) = self.parse_package() {
                stmts.push(Statement::Package(pkg));
                continue;
            } else if !self.s.errors.is_empty() {
                break;
            }
            self.restore(s);
            let s = self.save();
            if let Some(imp) = self.parse_import() {
                let root = imp
                    .path
                    .as_ref()
                    .and_then(|r| r.first())
                    .and_then(Term::as_var)
                    .unwrap_or_default()
                    .to_string();
                if root == "rego" {
                    self.rego_v1_import(&imp);
                }
                if root == "future" {
                    self.future_import(&imp);
                }
                stmts.push(Statement::Import(imp));
                continue;
            } else if !self.s.errors.is_empty() {
                break;
            }
            self.restore(s);
            let s = self.save();
            if let Some(rules) = self.parse_rules() {
                stmts.extend(rules.into_iter().map(|r| Statement::Rule(Box::new(r))));
                continue;
            } else if !self.s.errors.is_empty() {
                break;
            }
            self.restore(s);
            if let Some(body) = self.parse_query(true, Token::Eof) {
                stmts.push(Statement::Body(body));
                continue;
            }
            break;
        }
        stmts
    }

    fn parse_package(&mut self) -> Option<Package> {
        if self.s.tok != Token::Package {
            return None;
        }
        let loc = self.loc();
        self.scan_ws();
        if matches!(self.s.tok, Token::Dot | Token::LBrack) {
            return None;
        }
        if self.s.tok == Token::Whitespace {
            self.scan();
        }
        if !self.is_ident_or_allowed_ref_keyword() {
            self.illegal_token();
            return None;
        }
        let term = self.parse_term();
        let mut path = None;
        if let Some(term) = term {
            match &term.value {
                TermValue::Var(v) => {
                    path = Some(vec![
                        Term::var("data", term.loc.clone()),
                        Term::string(v, term.loc.clone()),
                    ]);
                }
                TermValue::Ref(v) => {
                    let first = v.first()?;
                    let mut out = vec![Term::var("data", first.loc.clone())];
                    let Some(name) = first.as_var() else {
                        self.errorf(
                            first.loc.clone(),
                            format!("unexpected {} token: expecting var", first.value_name()),
                        );
                        return None;
                    };
                    out.push(Term::string(name, first.loc.clone()));
                    for t in v.iter().skip(1) {
                        if t.as_string().is_none() {
                            self.errorf(
                                t.loc.clone(),
                                format!("unexpected {} token: expecting string", t.value_name()),
                            );
                            return None;
                        }
                        out.push(t.clone());
                    }
                    path = Some(out);
                }
                _ => {
                    self.illegal_token();
                    return None;
                }
            }
        }
        let Some(path) = path else {
            if self.s.errors.is_empty() {
                let l = self.loc();
                self.error(l, "expected path".into());
            }
            return None;
        };
        Some(Package { path, loc })
    }

    fn parse_import(&mut self) -> Option<Import> {
        if self.s.tok != Token::Import {
            return None;
        }
        let loc = self.loc();
        self.scan_ws();
        if matches!(self.s.tok, Token::Dot | Token::LBrack) {
            return None;
        }
        if self.s.tok == Token::Whitespace {
            self.scan();
        }
        if !self.is_ident_or_allowed_ref_keyword() {
            self.illegal_token();
            return None;
        }
        // The path is read without the future keywords (presentParser).
        let prev = self.s.s.keywords.clone();
        for (k, _) in V1_KEYWORDS {
            self.s.s.keywords.remove(k);
        }
        let saved_cache = std::mem::take(&mut self.cache);
        let term = self.parse_term();
        self.cache = saved_cache;
        for (k, t) in prev.iter() {
            self.s.s.add_keyword(k, *t);
        }
        let mut path = None;
        if let Some(term) = term {
            match &term.value {
                TermValue::Var(_) => {
                    let l = term.loc.clone();
                    path = Some(Term::reference(vec![term], l));
                }
                TermValue::Ref(v) => {
                    for t in v.iter().skip(1) {
                        if t.as_string().is_none() {
                            self.errorf(
                                t.loc.clone(),
                                format!("unexpected {} token: expecting string", t.value_name()),
                            );
                            return None;
                        }
                    }
                    path = Some(term.clone());
                }
                _ => {}
            }
        }
        let Some(path) = path else {
            let l = self.loc();
            self.error(l, "expected path".into());
            return None;
        };
        let r = path.as_ref().unwrap_or_default().to_vec();
        let root = r.first().and_then(Term::as_var).unwrap_or_default().to_string();
        if !matches!(root.as_str(), "data" | "input" | "future" | "rego") {
            self.hint("if this is unexpected, try updating OPA".into());
            let got = r.first().map(Term::to_string).unwrap_or_default();
            self.errorf(path.loc.clone(), format!("unexpected import path, must begin with one of: {{data, future, input, rego}}, got: {got}"));
            return None;
        }
        if self.s.tok == Token::As {
            self.scan();
            if self.s.tok != Token::Ident {
                self.illegal("expected var");
                return None;
            }
            if let Some(alias) = self.parse_term()
                && let Some(v) = alias.as_var()
            {
                return Some(Import {
                    path,
                    alias: Some(v.into()),
                    loc,
                });
            }
            self.illegal("expected var");
            return None;
        }
        if let [t] = r.as_slice() {
            let name = t.as_var().unwrap_or_default().to_string();
            if ast::is_keyword(&name) {
                self.errorf(
                    t.loc.clone(),
                    format!("unexpected import path, must not end with a keyword, got: {name}"),
                );
                self.hint("import a different path or use an alias".into());
            }
        } else if root != "future"
            && let Some(t) = r.last()
        {
            let name = t.as_string().unwrap_or_default().to_string();
            if ast::is_keyword(&name) {
                self.errorf(
                    t.loc.clone(),
                    format!("unexpected import path, must not end with a keyword, got: {name}"),
                );
                self.hint("import a different path or use an alias".into());
            }
        }
        Some(Import {
            path,
            alias: None,
            loc,
        })
    }

    /// futureImport: in Rego v1 every future keyword is one already.
    fn future_import(&mut self, imp: &Import) {
        let r = imp.path.as_ref().unwrap_or_default();
        if r.len() == 1 || r.get(1).and_then(Term::as_string) != Some("keywords") {
            self.errorf(
                imp.path.loc.clone(),
                "invalid import, must be `future.keywords`".into(),
            );
            return;
        }
        if imp.alias.is_some() {
            self.errorf(imp.path.loc.clone(), "`future` imports cannot be aliased".into());
            return;
        }
        if r.len() == 3 {
            let Some(kw) = r.get(2).and_then(Term::as_string) else {
                self.errorf(
                    imp.path.loc.clone(),
                    "invalid import, must be `future.keywords.x`, e.g. `import future.keywords.in`".into(),
                );
                return;
            };
            if !is_future_keyword(kw) {
                self.errorf(
                    imp.path.loc.clone(),
                    "unexpected keyword, must be one of [contains every if in]".into(),
                );
            }
        }
    }

    /// regoV1Import: `import rego.v1` is accepted, and changes nothing in v1.
    fn rego_v1_import(&mut self, imp: &Import) {
        let r = imp.path.as_ref().unwrap_or_default();
        if r.len() != 2 || r.get(1).and_then(Term::as_string) != Some("v1") {
            self.errorf(
                imp.path.loc.clone(),
                format!("invalid import `{}`, must be `rego.v1`", ref_text(r)),
            );
        }
    }

    fn is_ident_or_allowed_ref_keyword(&mut self) -> bool {
        if self.s.tok == Token::Ident {
            return true;
        }
        if self.is_allowed_ref_keyword(self.s.tok) {
            self.s.tok = Token::Ident;
            return true;
        }
        false
    }

    fn scan_ahead_ref(&mut self) -> bool {
        if self.is_allowed_ref_keyword(self.s.tok) {
            let s = self.save();
            self.scan_ws();
            let tok = self.s.tok;
            self.restore(s);
            if matches!(tok, Token::Dot | Token::LBrack) {
                self.s.tok = Token::Ident;
                return true;
            }
        }
        false
    }

    fn is_allowed_ref_keyword(&self, t: Token) -> bool {
        let s = t.name();
        ast::is_keyword(s) || self.s.s.is_keyword(s)
    }

    fn parse_rules(&mut self) -> Option<Vec<Rule>> {
        let rule_loc = self.loc();
        let _ = self.scan_ahead_ref();
        let mut default = false;
        if self.s.tok == Token::Default {
            self.scan();
            default = true;
            let _ = self.scan_ahead_ref();
        }
        if self.s.tok != Token::Ident {
            return None;
        }
        let (mut head, uses_contains) = self.parse_head(default)?;
        if uses_contains {
            head.keywords.push(Token::Contains);
        }
        let mut rule = Rule {
            default,
            head,
            body: Vec::new(),
            else_: None,
            loc: rule_loc.clone(),
            generated_body: false,
        };
        if default {
            if !self.validate_default_rule_value(&rule) {
                return None;
            }
            if !rule.head.args.is_empty() && !self.validate_default_rule_args(&rule) {
                return None;
            }
            rule.body = ast::true_body(rule.loc.clone());
            return Some(vec![rule]);
        }
        let has_if = self.s.tok == Token::If;
        let r = rule.head.ref_path();
        if has_if && !uses_contains && r.len() == 2 {
            if let Some(v) = r.get(1) {
                let is_ref = v.as_ref().is_some();
                if (!v.is_ground() || is_ref) && rule.head.args.is_empty() {
                    rule.head.key = Some(v.clone());
                }
            }
            if rule.head.value.is_none() {
                rule.head.generated_value = true;
                rule.head.value = Some(Term::boolean(true, rule.head.loc.clone()));
            } else {
                let name = r.first().and_then(Term::as_var)?;
                rule.head.name = Some(name.into());
            }
        }
        if !has_if && !uses_contains && rule.head.args.is_empty() && r.len() == 2 {
            let name = r.first().and_then(Term::as_var)?;
            rule.head.name = Some(name.into());
            rule.head.key = r.get(1).cloned();
            if rule.head.value.is_none() {
                rule.head.reference = r.get(..r.len() - 1).unwrap_or_default().to_vec();
            }
        }
        let mut body_done = false;
        if has_if {
            rule.head.keywords.push(Token::If);
            self.scan();
            let s = self.save();
            if let Some(expr) = self.parse_literal() {
                let is_set_literal =
                    matches!(&expr.terms, ExprTerms::Term(t) if matches!(t.value, TermValue::Set(_)));
                if !is_set_literal {
                    push_expr(&mut rule.body, expr);
                    body_done = true;
                }
            }
            if !body_done {
                self.restore(s);
            }
        }
        if !body_done {
            if has_if || self.s.tok == Token::LBrace {
                self.scan();
                rule.body = self.parse_body(Token::RBrace)?;
                self.scan();
            } else if uses_contains {
                rule.body = ast::true_body(rule.loc.clone());
                rule.generated_body = true;
                rule.loc = rule.head.loc.clone();
                return Some(vec![rule]);
            } else {
                return None;
            }
        }
        if self.s.tok == Token::Else && !self.scan_ahead_ref() {
            let r = rule.head.ref_path();
            if r.len() > 1 && !r.iter().all(Term::is_ground) {
                let l = self.loc();
                self.error(
                    l,
                    "else keyword cannot be used on rules with variables in head".into(),
                );
                return None;
            }
            if rule.head.key.is_some() {
                let l = self.loc();
                self.error(l, "else keyword cannot be used on multi-value rules".into());
                return None;
            }
            let head = rule.head.clone();
            rule.else_ = Some(self.parse_elses(&head)?);
        }
        let mut rules = vec![rule.clone()];
        while self.s.tok == Token::LBrace {
            if rule.else_.is_some() {
                let l = self.loc();
                self.error(l, "expected else keyword".into());
                return None;
            }
            let loc = self.loc();
            self.scan();
            let body = self.parse_body(Token::RBrace)?;
            self.scan();
            let mut head = rule.head.clone();
            for a in head.args.iter_mut() {
                if a.as_var().is_some_and(|v| v.starts_with('$')) {
                    a.value = TermValue::Var(self.gen_wildcard().into());
                }
            }
            set_head_loc(&mut head, &loc);
            rules.push(Rule {
                default: false,
                head,
                body,
                else_: None,
                loc,
                generated_body: false,
            });
        }
        Some(rules)
    }

    /// parseElse for each else branch in turn, chained: OPA recurses once a branch,
    /// without a depth limit, which a loop does without a frame a branch.
    fn parse_elses(&mut self, head: &Head) -> Option<Box<Rule>> {
        let mut branches = Vec::new();
        loop {
            let (branch, more) = self.parse_else(head)?;
            branches.push(branch);
            if !more {
                break;
            }
        }
        let mut tail = None;
        while let Some(mut b) = branches.pop() {
            b.else_ = tail;
            tail = Some(Box::new(b));
        }
        tail
    }

    /// One else branch, and whether another follows it.
    fn parse_else(&mut self, head: &Head) -> Option<(Rule, bool)> {
        let loc = self.loc();
        let mut h = head.clone();
        h.generated_value = false;
        for a in h.args.iter_mut() {
            if a.as_var().is_some_and(|v| v.starts_with('$')) {
                a.value = TermValue::Var(self.gen_wildcard().into());
            }
        }
        h.loc = self.loc();
        self.scan();
        match self.s.tok {
            Token::LBrace | Token::If => {
                h.generated_value = true;
                h.value = Some(Term::boolean(true, None));
            }
            Token::Assign | Token::Unify => {
                h.assign = self.s.tok == Token::Assign;
                self.scan();
                h.value = Some(self.parse_term_infix_call()?);
            }
            _ => {
                self.illegal("expected else value term or rule body");
                return None;
            }
        }
        let mut rule = Rule {
            default: false,
            head: h,
            body: Vec::new(),
            else_: None,
            loc: loc.clone(),
            generated_body: false,
        };
        let has_if = self.s.tok == Token::If;
        let has_lbrace = self.s.tok == Token::LBrace;
        if !has_if && !has_lbrace {
            rule.body = ast::true_body(loc);
            rule.generated_body = true;
            return Some((rule, false));
        }
        if has_if {
            rule.head.keywords.push(Token::If);
            self.scan();
        }
        if self.s.tok == Token::LBrace {
            self.scan();
            rule.body = self.parse_body(Token::RBrace)?;
            self.scan();
        } else if self.s.tok != Token::Eof {
            let expr = self.parse_literal()?;
            push_expr(&mut rule.body, expr);
        } else {
            self.illegal("rule body expected");
            return None;
        }
        let more = self.s.tok == Token::Else;
        Some((rule, more))
    }

    fn parse_head(&mut self, default: bool) -> Option<(Head, bool)> {
        let loc = self.loc();
        let term = self.parse_var();
        let Some(r) = self.parse_head_finish(term) else {
            self.illegal("expected rule head name");
            return None;
        };
        let mut head = match &r.value {
            TermValue::Var(v) => Head::var(v, r.loc.clone()),
            TermValue::Ref(x) => Head::reference(x.to_vec(), None),
            TermValue::Call(c) => {
                let (op, args) = c.split_first()?;
                let reference = match &op.value {
                    TermValue::Var(_) => vec![op.clone()],
                    TermValue::Ref(y) => {
                        if y.first().and_then(Term::as_var).is_none() {
                            self.illegal(&format!("rule head ref {} invalid", ref_text(y)));
                            return None;
                        }
                        y.to_vec()
                    }
                    _ => Vec::new(),
                };
                let mut h = Head::reference(reference, None);
                h.args = args.to_vec();
                h
            }
            _ => return None,
        };
        head.loc = loc;
        let name = ref_text(&head.ref_path());
        match self.s.tok {
            Token::Contains => {
                if !head.args.is_empty() {
                    self.illegal(&format!("the contains keyword can only be used with multi-value rule definitions (e.g., {name} contains <VALUE> {{ ... }})"));
                }
                self.scan();
                head.key = self.parse_term_infix_call();
                if head.key.is_none() {
                    self.illegal(&format!(
                        "expected rule key term (e.g., {name} contains <VALUE> {{ ... }})"
                    ));
                }
                return Some((head, true));
            }
            Token::Unify => {
                self.scan();
                head.value = self.parse_term_infix_call();
                if head.value.is_none() {
                    let key = head
                        .key
                        .as_ref()
                        .map(Term::to_string)
                        .unwrap_or_else(|| "<nil>".into());
                    self.illegal(&format!(
                        "expected rule value term (e.g., {name}[{key}] = <VALUE> {{ ... }})"
                    ));
                }
            }
            Token::Assign => {
                self.scan();
                head.assign = true;
                head.value = self.parse_term_infix_call();
                if head.value.is_none() {
                    let msg = if !head.args.is_empty() {
                        format!("expected function value term (e.g., {name}(...) := <VALUE> {{ ... }})")
                    } else if head.key.is_some() {
                        format!("expected partial rule value term (e.g., {name}[...] := <VALUE> {{ ... }})")
                    } else if default {
                        format!("expected default rule value term (e.g., default {name} := <VALUE>)")
                    } else {
                        format!("expected rule value term (e.g., {name} := <VALUE> {{ ... }})")
                    };
                    self.illegal(&msg);
                }
            }
            _ => {}
        }
        if head.value.is_none() && head.key.is_none() && (head.ref_path().len() != 2 || !head.args.is_empty())
        {
            head.generated_value = true;
            head.value = Some(Term::boolean(true, head.loc.clone()));
        }
        Some((head, false))
    }

    fn parse_body(&mut self, end: Token) -> Option<Body> {
        if !self.enter() {
            return None;
        }
        let r = self.parse_query(false, end);
        self.leave();
        r
    }

    fn parse_query(&mut self, require_semi: bool, end: Token) -> Option<Body> {
        let mut body = Vec::new();
        if self.s.tok == end {
            let l = self.loc();
            self.error(l, "found empty body".into());
            return None;
        }
        loop {
            let expr = self.parse_literal()?;
            push_expr(&mut body, expr);
            if self.s.tok == Token::Semicolon {
                self.scan();
                continue;
            }
            if self.s.tok == end || require_semi {
                return Some(body);
            }
            if !self.s.skipped_nl {
                if self.s.errors.is_empty() {
                    self.illegal(&format!(
                        "expected \\n or {} or {}",
                        Token::Semicolon.name(),
                        end.name()
                    ));
                }
                return None;
            }
        }
    }

    fn parse_literal(&mut self) -> Option<Expr> {
        let loc = self.loc();
        let expr = self.parse_literal_inner();
        expr.map(|mut e| {
            e.loc = loc;
            e
        })
    }

    fn parse_literal_inner(&mut self) -> Option<Expr> {
        if self.is_allowed_ref_keyword(self.s.tok) {
            let s = self.save();
            self.scan_ws();
            let tok = self.s.tok;
            self.restore(s);
            if matches!(tok, Token::Dot | Token::LBrack) {
                self.s.tok = Token::Ident;
                return self.parse_literal_expr(false);
            }
        }
        let mut negated = false;
        if self.s.tok == Token::Not {
            let s = self.save();
            self.scan_ws();
            let tok = self.s.tok;
            self.restore(s);
            if !matches!(tok, Token::Dot | Token::LBrack) {
                self.scan();
                negated = true;
            }
        }
        match self.s.tok {
            Token::Some => {
                if negated {
                    self.illegal("illegal negation of 'some'");
                    return None;
                }
                self.parse_some()
            }
            Token::Every => {
                if negated {
                    self.illegal("illegal negation of 'every'");
                    return None;
                }
                self.parse_every()
            }
            _ => self.parse_literal_expr(negated),
        }
    }

    fn parse_literal_expr(&mut self, negated: bool) -> Option<Expr> {
        let mut expr = self.parse_expr()?;
        expr.negated = negated;
        if self.s.tok == Token::With {
            expr.with = self.parse_with()?;
        }
        Some(expr)
    }

    fn parse_with(&mut self) -> Option<Vec<With>> {
        let mut withs = Vec::new();
        loop {
            let loc = self.loc();
            self.scan();
            if self.s.tok != Token::Ident {
                self.illegal("expected ident");
                return None;
            }
            let target = self.parse_term()?;
            if !matches!(target.value, TermValue::Ref(_) | TermValue::Var(_)) {
                self.illegal("expected with target path");
            }
            if self.s.tok != Token::As {
                self.illegal("expected as keyword");
                return None;
            }
            self.scan();
            let value = self.parse_term_infix_call()?;
            withs.push(With { target, value, loc });
            if self.s.tok != Token::With {
                break;
            }
        }
        Some(withs)
    }

    fn parse_some(&mut self) -> Option<Expr> {
        let loc = self.loc();
        let s = self.save();
        self.scan();
        if let Some(term) = self.parse_term_infix_call()
            && let Some(call) = term.as_call()
        {
            match call.first().map(Term::to_string).as_deref() {
                Some("internal.member_2") if call.len() != 3 => {
                    self.illegal("illegal domain");
                    return None;
                }
                Some("internal.member_3") if call.len() != 4 => {
                    self.illegal("illegal domain");
                    return None;
                }
                Some("internal.member_2" | "internal.member_3") => {}
                _ => {
                    self.illegal("expected `x in xs` or `x, y in xs` expression");
                    return None;
                }
            }
            let decl = SomeDecl {
                symbols: vec![term.clone()],
                loc: loc.clone(),
            };
            let mut expr = Expr::new(ExprTerms::Some(decl), loc);
            if self.s.tok == Token::With {
                expr.with = self.parse_with()?;
            }
            return Some(expr);
        }
        self.restore(s);
        let mut symbols = Vec::new();
        loop {
            self.scan();
            if self.s.tok != Token::Ident {
                self.illegal("expected var");
                return None;
            }
            symbols.push(self.parse_var());
            self.scan();
            if self.s.tok != Token::Comma {
                break;
            }
        }
        Some(Expr::new(
            ExprTerms::Some(SomeDecl {
                symbols,
                loc: loc.clone(),
            }),
            loc,
        ))
    }

    fn parse_every(&mut self) -> Option<Expr> {
        let loc = self.loc();
        self.scan();
        let term = self.parse_term_infix_call()?;
        let Some(call) = term.as_call() else {
            self.illegal("expected `x[, y] in xs { ... }` expression");
            return None;
        };
        let (key, value, domain) = match call.first().map(Term::to_string).as_deref() {
            Some("internal.member_2") => {
                let [_, v, d] = call else {
                    self.illegal("illegal domain");
                    return None;
                };
                (None, v.clone(), d.clone())
            }
            Some("internal.member_3") => {
                let [_, k, v, d] = call else {
                    self.illegal("illegal domain");
                    return None;
                };
                if k.as_var().is_none() {
                    self.illegal("expected key to be a variable");
                    return None;
                }
                (Some(k.clone()), v.clone(), d.clone())
            }
            _ => {
                self.illegal("expected `x[, y] in xs { ... }` expression");
                return None;
            }
        };
        if value.as_var().is_none() {
            self.illegal("expected value to be a variable");
            return None;
        }
        if self.s.tok == Token::LBrace {
            self.scan();
            let body = self.parse_body(Token::RBrace)?;
            self.scan();
            let every = Every {
                key,
                value,
                domain,
                body,
                loc: loc.clone(),
            };
            let mut expr = Expr::new(ExprTerms::Every(Box::new(every)), loc);
            if self.s.tok == Token::With {
                expr.with = self.parse_with()?;
            }
            return Some(expr);
        }
        self.illegal("missing body");
        None
    }

    fn parse_expr(&mut self) -> Option<Expr> {
        let lhs = self.parse_term_infix_call()?;
        if let Some(op) = self.parse_term_op(&[Token::Assign, Token::Unify]) {
            let rhs = self.parse_term_infix_call()?;
            return Some(Expr::new(ExprTerms::Call(vec![op, lhs, rhs]), None));
        }
        if let TermValue::Call(c) = lhs.value {
            return Some(Expr::new(ExprTerms::Call(c.into_inner()), None));
        }
        Some(Expr::term(lhs))
    }

    fn parse_term_infix_call(&mut self) -> Option<Term> {
        if !self.enter() {
            return None;
        }
        let r = self.parse_term_in(None, true);
        self.leave();
        r
    }

    fn parse_term_infix_call_in_list(&mut self) -> Option<Term> {
        if !self.enter() {
            return None;
        }
        let r = self.parse_term_in(None, false);
        self.leave();
        r
    }

    fn call(&self, op: Term, args: Vec<Term>, loc: Option<Location>) -> Term {
        let mut c = vec![op];
        c.extend(args);
        Term::new(TermValue::Call(c.into()), loc)
    }

    fn parse_term_in(&mut self, lhs: Option<Term>, key_val: bool) -> Option<Term> {
        if !self.enter() {
            return None;
        }
        let r = self.parse_term_in_inner(lhs, key_val);
        self.leave();
        r
    }

    fn parse_term_in_inner(&mut self, lhs: Option<Term>, key_val: bool) -> Option<Term> {
        let lhs = match lhs {
            Some(l) => Some(l),
            None => self.parse_term_relation(None),
        };
        let lhs = lhs?;
        if key_val && self.s.tok == Token::Comma {
            let s = self.save();
            self.scan();
            if let Some(mhs) = self.parse_term_relation(None)
                && let Some(op) = self.parse_term_op_member(true)
                && let Some(rhs) = self.parse_term_relation(None)
            {
                let loc = lhs.loc.clone();
                let call = self.call(op, vec![lhs.clone(), mhs, rhs], loc);
                return if self.s.tok == Token::In {
                    self.parse_term_in(Some(call), key_val)
                } else {
                    Some(call)
                };
            }
            self.restore(s);
        }
        let _ = self.scan_ahead_ref();
        if let Some(op) = self.parse_term_op_member(false)
            && let Some(rhs) = self.parse_term_relation(None)
        {
            let loc = lhs.loc.clone();
            let call = self.call(op, vec![lhs, rhs], loc);
            return if self.s.tok == Token::In {
                self.parse_term_in(Some(call), key_val)
            } else {
                Some(call)
            };
        }
        Some(lhs)
    }

    fn parse_binary(
        &mut self,
        lhs: Option<Term>,
        ops: &[Token],
        next: fn(&mut Self, Option<Term>) -> Option<Term>,
        again: fn(&mut Self, Option<Term>) -> Option<Term>,
    ) -> Option<Term> {
        if !self.enter() {
            return None;
        }
        let lhs = match lhs {
            Some(l) => Some(l),
            None => next(self, None),
        };
        // An operator whose right side fails leaves the left side, as OPA's do.
        let r = match lhs {
            None => None,
            Some(lhs) => match self.parse_term_op(ops) {
                Some(op) => match next(self, None) {
                    Some(rhs) => {
                        let loc = lhs.loc.clone();
                        let call = self.call(op, vec![lhs, rhs], loc);
                        if ops.contains(&self.s.tok) {
                            again(self, Some(call))
                        } else {
                            Some(call)
                        }
                    }
                    None => Some(lhs),
                },
                None => Some(lhs),
            },
        };
        self.leave();
        r
    }

    fn parse_term_relation(&mut self, lhs: Option<Term>) -> Option<Term> {
        self.parse_binary(
            lhs,
            &[
                Token::Equal,
                Token::Neq,
                Token::Lt,
                Token::Gt,
                Token::Lte,
                Token::Gte,
            ],
            Self::parse_term_or,
            Self::parse_term_relation,
        )
    }

    fn parse_term_or(&mut self, lhs: Option<Term>) -> Option<Term> {
        self.parse_binary(lhs, &[Token::Or], Self::parse_term_and, Self::parse_term_or)
    }

    fn parse_term_and(&mut self, lhs: Option<Term>) -> Option<Term> {
        self.parse_binary(lhs, &[Token::And], Self::parse_term_arith, Self::parse_term_and)
    }

    fn parse_term_arith(&mut self, lhs: Option<Term>) -> Option<Term> {
        self.parse_binary(
            lhs,
            &[Token::Add, Token::Sub],
            Self::parse_term_factor,
            Self::parse_term_arith,
        )
    }

    fn parse_term_factor(&mut self, lhs: Option<Term>) -> Option<Term> {
        self.parse_binary(
            lhs,
            &[Token::Mul, Token::Quo, Token::Rem],
            |p, _| p.parse_term(),
            Self::parse_term_factor,
        )
    }

    fn parse_term(&mut self) -> Option<Term> {
        if !self.enter() {
            return None;
        }
        let r = self.parse_term_inner();
        self.leave();
        r
    }

    fn parse_term_inner(&mut self) -> Option<Term> {
        let at = self.s.loc.offset;
        for (offset, term, post) in self.cache.iter().rev() {
            if *offset < at {
                break;
            }
            if *offset == at {
                let (term, post) = (term.clone(), post.clone());
                self.restore(post);
                return term;
            }
        }
        let s0 = self.save();
        let term = match self.s.tok {
            Token::Null => Some(Term::new(TermValue::Null, self.loc())),
            Token::True => Some(Term::boolean(true, self.loc())),
            Token::False => Some(Term::boolean(false, self.loc())),
            Token::Sub | Token::Dot | Token::Number => self.parse_number(),
            Token::String => self.parse_string(),
            Token::TemplateStringPart | Token::TemplateStringEnd => self.parse_template_string(false),
            Token::RawTemplateStringPart | Token::RawTemplateStringEnd => self.parse_template_string(true),
            Token::Ident | Token::Contains => Some(self.parse_var()),
            Token::LBrack => self.parse_array(),
            Token::LBrace => self.parse_set_or_object(),
            Token::LParen => {
                self.scan();
                match self.parse_term_infix_call() {
                    Some(r) if self.s.tok == Token::RParen => Some(r),
                    Some(_) => {
                        let l = self.loc();
                        self.error(l, "non-terminated expression".into());
                        None
                    }
                    None => None,
                }
            }
            _ => {
                self.illegal_token();
                None
            }
        };
        let term = self.parse_term_finish(term, false);
        let post = self.save();
        let o0 = s0.loc.offset;
        while self.cache.last().is_some_and(|(o, _, _)| *o >= o0) {
            self.cache.pop();
        }
        self.cache.push((o0, term.clone(), post));
        term
    }

    fn parse_term_finish(&mut self, head: Option<Term>, skipws: bool) -> Option<Term> {
        let head = head?;
        self.do_scan(skipws, None);
        match self.s.tok {
            Token::LParen | Token::Dot | Token::LBrack => self.parse_ref(head),
            tok => {
                if tok == Token::Whitespace {
                    self.scan();
                }
                Some(root_doc_ref(head))
            }
        }
    }

    fn parse_head_finish(&mut self, head: Term) -> Option<Term> {
        self.scan_ws();
        match self.s.tok {
            Token::Add
            | Token::Sub
            | Token::Mul
            | Token::Quo
            | Token::Rem
            | Token::And
            | Token::Or
            | Token::Equal
            | Token::Neq
            | Token::Gt
            | Token::Gte
            | Token::Lt
            | Token::Lte => self.illegal_token(),
            Token::Whitespace => self.do_scan(true, None),
            _ => {}
        }
        match self.s.tok {
            Token::LParen | Token::Dot | Token::LBrack => return self.parse_ref(head),
            Token::Whitespace => self.scan(),
            _ => {}
        }
        Some(root_doc_ref(head))
    }

    fn parse_number(&mut self) -> Option<Term> {
        let loc = self.loc();
        let mut prefix = String::new();
        if self.s.tok == Token::Sub {
            prefix.push('-');
            self.scan();
            if !matches!(self.s.tok, Token::Number | Token::Dot) {
                self.illegal("expected number");
                return None;
            }
        }
        if self.s.tok == Token::Dot {
            prefix.push('.');
            self.scan();
            if self.s.tok != Token::Number {
                self.illegal("expected number");
                return None;
            }
        }
        let decimal_prefix = prefix.ends_with('.');
        let lit = self.s.lit.clone();
        let b = lit.as_bytes();
        if !decimal_prefix && b.len() > 1 && b.first() == Some(&b'0') {
            let is_decimal = b.get(1) == Some(&b'.');
            let is_scientific = b.len() > 2 && matches!(b.get(1), Some(b'e' | b'E'));
            if !is_decimal && !is_scientific {
                self.illegal("expected number without leading zero");
                return None;
            }
        }
        let s = format!("{prefix}{lit}");
        let Ok(f) = Float::parse(&s) else {
            self.illegal("invalid float");
            return None;
        };
        let exp = f.go_exponent();
        if !(-100_000..=100_000).contains(&exp) {
            let l = self.loc();
            self.error(l, "number too big".into());
            return None;
        }
        Some(Term::new(TermValue::Number(Number(s.into())), loc))
    }

    fn parse_string(&mut self) -> Option<Term> {
        let lit = self.s.lit.clone();
        if lit.starts_with('"') {
            if lit == "\"\"" {
                return Some(Term::string("", self.loc()));
            }
            let inner = lit.get(1..lit.len().saturating_sub(1)).unwrap_or_default();
            if !inner.contains('\\') {
                return Some(Term::string(inner, self.loc()));
            }
            let Ok(crate::value::Value::String(s)) = crate::value::from_json(&lit) else {
                self.errorf(self.loc(), format!("illegal string literal: {lit}"));
                return None;
            };
            return Some(Term::string(&s, self.loc()));
        }
        if lit.len() < 2 {
            return None;
        }
        Some(Term::string(
            lit.get(1..lit.len() - 1).unwrap_or_default(),
            self.loc(),
        ))
    }

    fn template_part(tok: Token, lit: &str) -> Result<String, String> {
        let inner = || {
            lit.get(1..lit.len().saturating_sub(1))
                .unwrap_or_default()
                .to_string()
        };
        match tok {
            Token::TemplateStringPart | Token::TemplateStringEnd => {
                let s = inner();
                if !s.contains('\\') {
                    return Ok(s);
                }
                match crate::value::from_json(&format!("\"{s}\"")) {
                    Ok(crate::value::Value::String(v)) => Ok(v.to_string()),
                    _ => Err(format!("illegal template-string part: {lit}")),
                }
            }
            Token::RawTemplateStringPart | Token::RawTemplateStringEnd => Ok(inner()),
            _ => Err("expected template-string part".into()),
        }
    }

    fn parse_template_string(&mut self, multi_line: bool) -> Option<Term> {
        let loc = self.loc();
        let mut parts = Vec::new();
        loop {
            let s = match Self::template_part(self.s.tok, &self.s.lit) {
                Ok(s) => s,
                Err(e) => {
                    let l = self.loc();
                    self.error(l, e);
                    return None;
                }
            };
            if !s.is_empty() {
                parts.push(TemplatePart::Term(Term::string(&s, self.loc())));
            }
            if matches!(self.s.tok, Token::TemplateStringEnd | Token::RawTemplateStringEnd) {
                break;
            }
            let before = self.s.comments;
            self.scan();
            let after = self.s.comments;
            let Some(expr) = self.parse_literal() else {
                let l = self.loc();
                self.error(l, "invalid template-string expression".into());
                return None;
            };
            let bad = if expr.negated {
                Some("unexpected negation ('not') in template-string expression".to_string())
            } else if expr.is_equality() {
                Some("unexpected unification ('=') in template-string expression".into())
            } else if expr.is_assignment() {
                Some("unexpected assignment (':=') in template-string expression".into())
            } else if expr.is_every() {
                Some("unexpected 'every' in template-string expression".into())
            } else if expr.is_some() {
                Some("unexpected 'some' in template-string expression".into())
            } else {
                None
            };
            if let Some(m) = bad {
                self.errorf(expr.loc.clone(), m);
                return None;
            }
            let mut non_optional = false;
            if let ExprTerms::Term(t) = &expr.terms
                && after == before
                && matches!(
                    t.value,
                    TermValue::String(_) | TermValue::Number(_) | TermValue::Bool(_) | TermValue::Null
                )
            {
                non_optional = true;
                parts.push(TemplatePart::Term((**t).clone()));
            }
            if !non_optional {
                parts.push(TemplatePart::Expr(Box::new(expr)));
            }
            if self.s.tok != Token::RBrace {
                self.errorf(self.loc(), "expected } to end template string expression".into());
                return None;
            }
            self.do_scan(false, Some(multi_line));
        }
        Some(Term::new(TermValue::TemplateString { multi_line, parts }, loc))
    }

    fn parse_call(&mut self, operator: Term) -> Option<Term> {
        if !self.enter() {
            return None;
        }
        let loc = operator.loc.clone();
        self.scan();
        let r = if self.s.tok == Token::RParen {
            self.scan_ws();
            if operator
                .as_ref()
                .is_some_and(|r| matches!(r, [t] if t.as_var() == Some("set")))
            {
                Some(ast::set_term(Vec::new(), loc))
            } else {
                Some(Term::new(TermValue::Call(vec![operator].into()), loc))
            }
        } else if let Some(r) = self.parse_term_list(Token::RParen, vec![operator]) {
            self.scan_ws();
            Some(Term::new(TermValue::Call(r.into()), loc))
        } else {
            None
        };
        self.leave();
        r
    }

    fn parse_ref(&mut self, head: Term) -> Option<Term> {
        if !self.enter() {
            return None;
        }
        let r = self.parse_ref_inner(head);
        self.leave();
        r
    }

    fn parse_ref_inner(&mut self, head: Term) -> Option<Term> {
        let loc = head.loc.clone();
        match head.value {
            TermValue::Var(_)
            | TermValue::Array(_)
            | TermValue::Object(_)
            | TermValue::Set(_)
            | TermValue::ArrayCompr(..)
            | TermValue::ObjectCompr(..)
            | TermValue::SetCompr(..)
            | TermValue::Call(_) => {}
            _ => self.errorf(
                loc.clone(),
                format!("illegal ref (head cannot be {})", head.value_name()),
            ),
        }
        let mut r = vec![head];
        loop {
            match self.s.tok {
                Token::Dot => {
                    self.scan_ws();
                    if self.s.tok != Token::Ident && !self.is_allowed_ref_keyword(self.s.tok) {
                        self.illegal(&format!("expected {}", Token::Ident.name()));
                        return None;
                    }
                    r.push(Term::string(&self.s.lit.clone(), self.loc()));
                    self.scan_ws();
                }
                Token::LParen => {
                    let op = Term::reference(r, loc.clone());
                    let mut term = self.parse_call(op);
                    if let Some(t) = term.take() {
                        match self.s.tok {
                            Token::Whitespace => {
                                self.scan();
                                return Some(t);
                            }
                            Token::Dot | Token::LBrack => return self.parse_ref(t),
                            _ => return Some(t),
                        }
                    }
                    return None;
                }
                Token::LBrack => {
                    self.scan();
                    let term = self.parse_term_infix_call()?;
                    if self.s.tok != Token::RBrack {
                        self.illegal(&format!("expected {}", Token::LBrack.name()));
                        return None;
                    }
                    r.push(term);
                    self.scan_ws();
                }
                Token::Whitespace => {
                    self.scan();
                    return Some(Term::reference(r, loc));
                }
                _ => return Some(Term::reference(r, loc)),
            }
        }
    }

    fn parse_array(&mut self) -> Option<Term> {
        if !self.enter() {
            return None;
        }
        let loc = self.loc();
        let r = self.parse_array_inner().map(|mut t| {
            t.loc = loc;
            t
        });
        self.leave();
        r
    }

    fn parse_array_inner(&mut self) -> Option<Term> {
        self.scan();
        if self.s.tok == Token::RBrack {
            return Some(Term::new(TermValue::Array(Vec::new().into()), None));
        }
        let mut potential = true;
        if self.s.tok == Token::Comma {
            potential = false;
            self.scan();
        }
        let s = self.save();
        let head = self.parse_term()?;
        match self.s.tok {
            Token::RBrack => return Some(Term::new(TermValue::Array(vec![head].into()), None)),
            Token::Comma => {
                self.scan();
                return self
                    .parse_term_list(Token::RBrack, vec![head])
                    .map(|t| Term::new(TermValue::Array(t.into()), None));
            }
            Token::Or if potential => {
                self.scan();
                if let Some(body) = self.parse_body(Token::RBrack) {
                    return Some(Term::new(TermValue::ArrayCompr(head.into(), body.into()), None));
                }
                if self.s.tok != Token::Comma {
                    return None;
                }
            }
            _ => {}
        }
        self.restore(s);
        self.parse_term_list(Token::RBrack, Vec::new())
            .map(|t| Term::new(TermValue::Array(t.into()), None))
    }

    fn parse_set_or_object(&mut self) -> Option<Term> {
        if !self.enter() {
            return None;
        }
        let loc = self.loc();
        let r = self.parse_set_or_object_inner().map(|mut t| {
            t.loc = loc;
            t
        });
        self.leave();
        r
    }

    fn parse_set_or_object_inner(&mut self) -> Option<Term> {
        self.scan();
        if self.s.tok == Token::RBrace {
            return Some(ast::object_term(Vec::new(), None));
        }
        let mut potential = true;
        if self.s.tok == Token::Comma {
            potential = false;
            self.scan();
        }
        let s = self.save();
        let head = self.parse_term()?;
        match self.s.tok {
            Token::Or if potential => return self.parse_set(s, head, potential),
            Token::RBrace | Token::Comma => return self.parse_set(s, head, potential),
            Token::Colon => return self.parse_object(head, potential),
            _ => {}
        }
        self.restore(s.clone());
        let head = self.parse_term_infix_call_in_list()?;
        match self.s.tok {
            Token::RBrace | Token::Comma => return self.parse_set(s, head, false),
            Token::Colon => return self.parse_object(head, potential),
            _ => {}
        }
        self.illegal("non-terminated set");
        None
    }

    fn parse_set(&mut self, s: State<'a>, head: Term, potential: bool) -> Option<Term> {
        if !self.enter() {
            return None;
        }
        let r = match self.s.tok {
            Token::RBrace => Some(ast::set_term(vec![head], None)),
            Token::Comma => {
                self.scan();
                self.parse_term_list(Token::RBrace, vec![head])
                    .map(|t| ast::set_term(t, None))
            }
            Token::Or => 'or: {
                if potential {
                    self.scan();
                    if let Some(body) = self.parse_body(Token::RBrace) {
                        break 'or Some(Term::new(TermValue::SetCompr(head.into(), body.into()), None));
                    }
                    if self.s.tok != Token::Comma {
                        break 'or None;
                    }
                }
                self.restore(s);
                self.parse_term_list(Token::RBrace, Vec::new())
                    .map(|t| ast::set_term(t, None))
            }
            _ => None,
        };
        self.leave();
        r
    }

    fn parse_object(&mut self, k: Term, potential: bool) -> Option<Term> {
        if !self.enter() {
            return None;
        }
        let r = self.parse_object_inner(k, potential);
        self.leave();
        r
    }

    fn parse_object_inner(&mut self, k: Term, potential: bool) -> Option<Term> {
        if self.s.tok != Token::Colon {
            return None;
        }
        self.scan();
        let s = self.save();
        let v = self.parse_term()?;
        let mut potential_relation = true;
        if potential {
            if matches!(self.s.tok, Token::RBrace | Token::Comma) {
                potential_relation = false;
            }
            if matches!(self.s.tok, Token::RBrace | Token::Comma | Token::Or)
                && let Some(t) = self.parse_object_finish(k.clone(), v, true)
            {
                return Some(t);
            }
        }
        self.restore(s);
        if potential_relation {
            let v = self.parse_term_infix_call_in_list()?;
            if matches!(self.s.tok, Token::RBrace | Token::Comma) {
                return self.parse_object_finish(k, v, false);
            }
        }
        self.illegal("non-terminated object");
        None
    }

    fn parse_object_finish(&mut self, key: Term, val: Term, potential: bool) -> Option<Term> {
        if !self.enter() {
            return None;
        }
        let r = match self.s.tok {
            Token::RBrace => Some(ast::object_term(vec![(key, val)], None)),
            Token::Or => {
                if potential {
                    self.scan();
                    self.parse_body(Token::RBrace).map(|body| {
                        Term::new(TermValue::ObjectCompr(key.into(), val.into(), body.into()), None)
                    })
                } else {
                    self.illegal("non-terminated object");
                    None
                }
            }
            Token::Comma => {
                self.scan();
                self.parse_term_pair_list(Token::RBrace, vec![(key, val)])
                    .map(|r| ast::object_term(r, None))
            }
            _ => None,
        };
        self.leave();
        r
    }

    fn parse_term_list(&mut self, end: Token, mut r: Vec<Term>) -> Option<Vec<Term>> {
        if self.s.tok == end {
            return Some(r);
        }
        loop {
            let term = self.parse_term_infix_call_in_list()?;
            r.push(term);
            if self.s.tok == end {
                return Some(r);
            }
            if self.s.tok == Token::Comma {
                self.scan();
                if self.s.tok == end {
                    return Some(r);
                }
                continue;
            }
            self.illegal(&format!("expected {:?} or {:?}", Token::Comma.name(), end.name()));
            return None;
        }
    }

    fn parse_term_pair_list(&mut self, end: Token, mut r: Vec<(Term, Term)>) -> Option<Vec<(Term, Term)>> {
        if self.s.tok == end {
            return Some(r);
        }
        loop {
            let key = self.parse_term_infix_call_in_list()?;
            if self.s.tok != Token::Colon {
                self.illegal(&format!("expected {:?}", Token::Colon.name()));
                return None;
            }
            self.scan();
            let val = self.parse_term_infix_call_in_list()?;
            r.push((key, val));
            if self.s.tok == end {
                return Some(r);
            }
            if self.s.tok == Token::Comma {
                self.scan();
                if self.s.tok == end {
                    return Some(r);
                }
                continue;
            }
            self.illegal(&format!("expected {:?} or {:?}", Token::Comma.name(), end.name()));
            return None;
        }
    }

    fn parse_term_op(&mut self, values: &[Token]) -> Option<Term> {
        if values.contains(&self.s.tok) {
            let loc = self.loc();
            let r = op_ref(self.s.tok.name(), loc);
            self.scan();
            return Some(r);
        }
        None
    }

    fn parse_term_op_member(&mut self, with_key: bool) -> Option<Term> {
        if self.s.tok == Token::In {
            let r = member_ref(with_key, self.loc());
            self.scan();
            return Some(r);
        }
        None
    }

    fn parse_var(&mut self) -> Term {
        if self.s.lit == "_" {
            let w = self.gen_wildcard();
            return Term::var(&w, self.loc());
        }
        Term::var(&self.s.lit.clone(), self.loc())
    }

    fn gen_wildcard(&mut self) -> String {
        let v = format!("${}", self.s.wildcard);
        self.s.wildcard += 1;
        v
    }

    fn error(&mut self, loc: Option<Location>, reason: String) {
        let mut msg = reason;
        write_hints(&mut msg, &self.s.hints);
        let details = loc.as_ref().map(|l| detail(self.s.s.bs, l.offset));
        self.s.errors.push(Error {
            code: PARSE_ERR,
            message: msg,
            loc,
            details,
            lines: Vec::new(),
        });
        self.s.hints.clear();
    }

    fn errorf(&mut self, loc: Option<Location>, msg: String) {
        self.error(loc, msg);
    }

    fn hint(&mut self, s: String) {
        self.s.hints.push(s);
    }

    fn illegal(&mut self, note: &str) {
        if self.s.tok == Token::Illegal {
            self.errorf(self.loc(), "illegal token".into());
            return;
        }
        let tok = self.s.tok.name();
        let kind = if is_future_keyword(tok) || self.s.tok.is_keyword() {
            "keyword"
        } else {
            "token"
        };
        let msg = if note.is_empty() {
            format!("unexpected {tok} {kind}")
        } else {
            format!("unexpected {tok} {kind}: {note}")
        };
        self.errorf(self.loc(), msg);
    }

    fn illegal_token(&mut self) {
        self.illegal("");
    }

    fn scan(&mut self) {
        self.do_scan(true, None);
    }

    fn scan_ws(&mut self) {
        self.do_scan(false, None);
    }

    fn do_scan(&mut self, skipws: bool, template: Option<bool>) {
        if self.s.tok != Token::Whitespace {
            self.s.last_end = self.s.tok_end;
            self.s.skipped_nl = false;
        }
        loop {
            let (tok, pos, lit, errs) = self.s.s.scan(template);
            self.s.tok = tok;
            self.s.lit = lit;
            self.s.tok_end = pos.end;
            self.s.loc.row = pos.row;
            self.s.loc.col = pos.col;
            self.s.loc.offset = pos.offset;
            self.s.loc.tabs = pos.tabs;
            for e in &errs {
                let l = self.loc();
                self.error(l, e.message.to_string());
            }
            if !errs.is_empty() {
                self.s.tok = Token::Illegal;
            }
            if self.s.tok == Token::Whitespace {
                if self.s.lit == "\n" {
                    self.s.skipped_nl = true;
                }
                if skipws {
                    continue;
                }
            }
            if self.s.tok != Token::Comment {
                break;
            }
            self.s.comments += 1;
        }
    }

    fn validate_default_rule_value(&mut self, rule: &Rule) -> bool {
        let Some(v) = &rule.head.value else {
            self.error(
                rule.loc.clone(),
                "illegal default rule (must have a value)".into(),
            );
            return false;
        };
        let mut bad = Vec::new();
        find_refs_vars_calls(v, &mut bad);
        for kind in &bad {
            self.error(
                rule.loc.clone(),
                format!("illegal default rule (value cannot contain {kind})"),
            );
        }
        bad.is_empty()
    }

    fn validate_default_rule_args(&mut self, rule: &Rule) -> bool {
        let mut seen: Vec<String> = Vec::new();
        let mut valid = true;
        for a in &rule.head.args {
            match a.as_var() {
                Some(v) => {
                    if seen.iter().any(|s| s == v) {
                        self.error(
                            rule.loc.clone(),
                            format!("illegal default rule (arguments cannot be repeated {v})"),
                        );
                        valid = false;
                    }
                    seen.push(v.to_string());
                }
                None => {
                    self.error(
                        rule.loc.clone(),
                        format!(
                            "illegal default rule (arguments cannot contain {})",
                            a.value_name()
                        ),
                    );
                    valid = false;
                }
            }
        }
        valid
    }

    fn enter(&mut self) -> bool {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            let l = self.loc();
            self.error(l, "max parsing recursion depth exceeded".into());
            self.depth -= 1;
            return false;
        }
        true
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }
}

/// Appends an expression to a body, numbering it (Body.Append).
fn push_expr(body: &mut Body, mut expr: Expr) {
    expr.index = body.len();
    body.push(expr);
}

fn set_head_loc(head: &mut Head, loc: &Option<Location>) {
    head.loc = loc.clone();
}

/// The head itself, or a ref to it when it names a root document.
fn root_doc_ref(head: Term) -> Term {
    if matches!(head.as_var(), Some("data" | "input")) {
        let loc = head.loc.clone();
        return Term::reference(vec![head], loc);
    }
    head
}

fn write_hints(msg: &mut String, hints: &[String]) {
    match hints {
        [] => {}
        [h] => {
            msg.push_str(" (hint: ");
            msg.push_str(h);
            msg.push(')');
        }
        _ => {
            msg.push_str(" (hints: ");
            msg.push_str(&hints.join(", "));
            msg.push(')');
        }
    }
}

/// The kinds of ref, var and call a default value holds, comprehensions skipped
/// (validateDefaultRuleValue's visitor).
fn find_refs_vars_calls(t: &Term, out: &mut Vec<&'static str>) {
    match &t.value {
        TermValue::ArrayCompr(..) | TermValue::SetCompr(..) | TermValue::ObjectCompr(..) => {}
        TermValue::Ref(_) => out.push("ref"),
        TermValue::Var(_) => out.push("var"),
        TermValue::Call(_) => out.push("call"),
        TermValue::Array(a) | TermValue::Set(a) => a.iter().for_each(|x| find_refs_vars_calls(x, out)),
        TermValue::Object(o) => o.iter().for_each(|(k, v)| {
            find_refs_vars_calls(k, out);
            find_refs_vars_calls(v, out);
        }),
        _ => {}
    }
}
