//! text/template/parse/parse.go and node.go: the parse trees, and the parser that
//! builds them, with Go's error texts and the lines they cite.

use std::collections::BTreeMap;
use std::fmt;

use crate::lex::{Item, Lexer, T};
use crate::strconv;

/// A list of nodes.
#[derive(Debug, Clone)]
pub(crate) struct List {
    pub pos: usize,
    pub nodes: Vec<Node>,
}

/// A node of a list.
#[derive(Debug, Clone)]
pub(crate) enum Node {
    Text {
        pos: usize,
        text: String,
    },
    Comment {
        pos: usize,
        text: String,
    },
    Action {
        pos: usize,
        pipe: Pipe,
    },
    If(Branch),
    Range(Branch),
    With(Branch),
    Template {
        pos: usize,
        name: String,
        pipe: Option<Pipe>,
    },
    Break {
        pos: usize,
    },
    Continue {
        pos: usize,
    },
}

/// The pipeline and lists of an if, range or with.
#[derive(Debug, Clone)]
pub(crate) struct Branch {
    pub pos: usize,
    pub pipe: Pipe,
    pub list: List,
    pub else_list: Option<List>,
}

/// A pipeline, with the variables it declares or assigns.
#[derive(Debug, Clone)]
pub(crate) struct Pipe {
    pub pos: usize,
    pub is_assign: bool,
    pub decl: Vec<Variable>,
    pub cmds: Vec<Command>,
}

/// A command of a pipeline: a function, field or value and its arguments.
#[derive(Debug, Clone)]
pub(crate) struct Command {
    pub pos: usize,
    pub args: Vec<Arg>,
}

/// A variable and the fields after it (`$x.A.B`).
#[derive(Debug, Clone)]
pub(crate) struct Variable {
    pub pos: usize,
    pub ident: Vec<String>,
}

/// A numeric constant, with each type it fits.
#[derive(Debug, Clone)]
pub(crate) struct Number {
    pub pos: usize,
    pub is_int: bool,
    pub is_uint: bool,
    pub is_float: bool,
    pub int: i64,
    pub uint: u64,
    pub float: f64,
    pub text: String,
}

/// An operand.
#[derive(Debug, Clone)]
pub(crate) enum Arg {
    Field {
        pos: usize,
        ident: Vec<String>,
    },
    Chain {
        pos: usize,
        node: Box<Arg>,
        field: Vec<String>,
    },
    Identifier {
        pos: usize,
        name: String,
    },
    Pipe(Pipe),
    Variable(Variable),
    Bool {
        pos: usize,
        val: bool,
    },
    Dot {
        pos: usize,
    },
    Nil {
        pos: usize,
    },
    Number(Number),
    String {
        pos: usize,
        quoted: String,
        text: String,
    },
}

impl Arg {
    pub(crate) fn pos(&self) -> usize {
        match self {
            Arg::Field { pos, .. }
            | Arg::Chain { pos, .. }
            | Arg::Identifier { pos, .. }
            | Arg::Bool { pos, .. }
            | Arg::Dot { pos }
            | Arg::Nil { pos }
            | Arg::String { pos, .. } => *pos,
            Arg::Pipe(p) => p.pos,
            Arg::Variable(v) => v.pos,
            Arg::Number(n) => n.pos,
        }
    }
}

impl Node {
    pub(crate) fn pos(&self) -> usize {
        match self {
            Node::Text { pos, .. }
            | Node::Comment { pos, .. }
            | Node::Action { pos, .. }
            | Node::Template { pos, .. }
            | Node::Break { pos }
            | Node::Continue { pos } => *pos,
            Node::If(b) | Node::Range(b) | Node::With(b) => b.pos,
        }
    }
}

impl fmt::Display for List {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.nodes.iter().try_for_each(|n| write!(f, "{n}"))
    }
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let branch = |f: &mut fmt::Formatter<'_>, name: &str, b: &Branch| {
            write!(f, "{{{{{name} {}}}}}{}", b.pipe, b.list)?;
            if let Some(e) = &b.else_list {
                write!(f, "{{{{else}}}}{e}")?;
            }
            f.write_str("{{end}}")
        };
        match self {
            Node::Text { text, .. } => f.write_str(text),
            Node::Comment { text, .. } => write!(f, "{{{{{text}}}}}"),
            Node::Action { pipe, .. } => write!(f, "{{{{{pipe}}}}}"),
            Node::If(b) => branch(f, "if", b),
            Node::Range(b) => branch(f, "range", b),
            Node::With(b) => branch(f, "with", b),
            Node::Template { name, pipe, .. } => {
                write!(f, "{{{{template {}", strconv::quote(name))?;
                if let Some(p) = pipe {
                    write!(f, " {p}")?;
                }
                f.write_str("}}")
            }
            Node::Break { .. } => f.write_str("{{break}}"),
            Node::Continue { .. } => f.write_str("{{continue}}"),
        }
    }
}

impl fmt::Display for Pipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.decl.is_empty() {
            for (i, v) in self.decl.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{v}")?;
            }
            f.write_str(if self.is_assign { " = " } else { " := " })?;
        }
        for (i, c) in self.cmds.iter().enumerate() {
            if i > 0 {
                f.write_str(" | ")?;
            }
            write!(f, "{c}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, a) in self.args.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            match a {
                Arg::Pipe(p) => write!(f, "({p})")?,
                _ => write!(f, "{a}")?,
            }
        }
        Ok(())
    }
}

impl fmt::Display for Variable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.ident.join("."))
    }
}

impl fmt::Display for Arg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Arg::Field { ident, .. } => ident.iter().try_for_each(|i| write!(f, ".{i}")),
            Arg::Chain { node, field, .. } => {
                match node.as_ref() {
                    Arg::Pipe(p) => write!(f, "({p})")?,
                    n => write!(f, "{n}")?,
                }
                field.iter().try_for_each(|i| write!(f, ".{i}"))
            }
            Arg::Identifier { name, .. } => f.write_str(name),
            Arg::Pipe(p) => write!(f, "{p}"),
            Arg::Variable(v) => write!(f, "{v}"),
            Arg::Bool { val, .. } => f.write_str(if *val { "true" } else { "false" }),
            Arg::Dot { .. } => f.write_str("."),
            Arg::Nil { .. } => f.write_str("nil"),
            Arg::Number(n) => f.write_str(&n.text),
            Arg::String { quoted, .. } => f.write_str(quoted),
        }
    }
}

/// parse.go's IsEmptyTree, for a list.
pub(crate) fn is_empty_list(l: &List) -> bool {
    l.nodes.iter().all(|n| match n {
        Node::Comment { .. } => true,
        Node::Text { text, .. } => text.trim().is_empty(),
        _ => false,
    })
}

/// What a list ends at, or a node.
enum Parsed {
    Node(Node),
    End,
    Else { pos: usize },
}

impl fmt::Display for Parsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Parsed::Node(n) => write!(f, "{n}"),
            Parsed::End => f.write_str("{{end}}"),
            Parsed::Else { .. } => f.write_str("{{else}}"),
        }
    }
}

/// The deepest nesting of parenthesized pipelines, if, range, with and block, together.
/// Go allows 10000 parentheses and any nesting of actions, on goroutine stacks that
/// grow; this parser recurses on a thread's fixed stack. Measured on a 2 MiB stack
/// (Rust's default for a spawned thread) in a debug build, a nested if costs this
/// parser about 10.5 KiB and a parenthesis about 6.9 KiB (200 and 303 levels overflow
/// it), so 100 levels take at most half of it. lib.rs's tests parse and run the
/// deepest allowed on such a stack.
pub(crate) const MAX_DEPTH: usize = 100;

/// The trees a parse defines, by name.
pub(crate) type Trees = BTreeMap<String, List>;

pub(crate) struct Parser<'p, 'a> {
    name: String,
    parse_name: &'p str,
    lex: &'p mut Lexer<'a>,
    trees: &'p mut Trees,
    has_function: &'p dyn Fn(&str) -> bool,
    token: [Item; 3],
    peek_count: usize,
    vars: Vec<String>,
    action_line: usize,
    range_depth: usize,
    depth: usize,
}

type Res<T> = Result<T, String>;

impl<'p, 'a> Parser<'p, 'a> {
    pub(crate) fn new(
        name: &str,
        parse_name: &'p str,
        lex: &'p mut Lexer<'a>,
        trees: &'p mut Trees,
        has_function: &'p dyn Fn(&str) -> bool,
    ) -> Parser<'p, 'a> {
        Parser {
            name: name.to_owned(),
            parse_name,
            lex,
            trees,
            has_function,
            token: [Item::eof(), Item::eof(), Item::eof()],
            peek_count: 0,
            vars: vec!["$".into()],
            action_line: 0,
            range_depth: 0,
            depth: 0,
        }
    }

    /// A parser for a define or block, sharing this one's scanner and trees.
    fn sub(&mut self, name: &str, depth: usize) -> Parser<'_, 'a> {
        let mut p = Parser::new(name, self.parse_name, self.lex, self.trees, self.has_function);
        p.depth = depth;
        p
    }

    fn tok(&self, i: usize) -> Item {
        self.token.get(i).cloned().unwrap_or_else(Item::eof)
    }

    fn set_tok(&mut self, i: usize, item: Item) {
        if let Some(t) = self.token.get_mut(i) {
            *t = item;
        }
    }

    fn next(&mut self) -> Item {
        if self.peek_count > 0 {
            self.peek_count -= 1;
        } else {
            let item = self.lex.next_item();
            self.set_tok(0, item);
        }
        self.tok(self.peek_count)
    }

    fn backup(&mut self) {
        self.peek_count += 1;
    }

    fn backup2(&mut self, t1: Item) {
        self.set_tok(1, t1);
        self.peek_count = 2;
    }

    fn backup3(&mut self, t2: Item, t1: Item) {
        self.set_tok(1, t1);
        self.set_tok(2, t2);
        self.peek_count = 3;
    }

    fn peek(&mut self) -> Item {
        if self.peek_count > 0 {
            return self.tok(self.peek_count - 1);
        }
        self.peek_count = 1;
        let item = self.lex.next_item();
        self.set_tok(0, item);
        self.tok(0)
    }

    fn next_non_space(&mut self) -> Item {
        loop {
            let t = self.next();
            if t.typ != T::Space {
                return t;
            }
        }
    }

    fn peek_non_space(&mut self) -> Item {
        let t = self.next_non_space();
        self.backup();
        t
    }

    fn errorf(&self, msg: impl fmt::Display) -> String {
        format!("template: {}:{}: {msg}", self.parse_name, self.tok(0).line)
    }

    fn expect(&mut self, expected: T, context: &str) -> Res<Item> {
        let token = self.next_non_space();
        if token.typ != expected {
            return Err(self.unexpected(&token, context));
        }
        Ok(token)
    }

    fn expect_one_of(&mut self, e1: T, e2: T, context: &str) -> Res<Item> {
        let token = self.next_non_space();
        if token.typ != e1 && token.typ != e2 {
            return Err(self.unexpected(&token, context));
        }
        Ok(token)
    }

    fn unexpected(&self, token: &Item, context: &str) -> String {
        if token.typ == T::Error {
            let mut extra = String::new();
            if self.action_line != 0 && self.action_line != token.line {
                extra = format!(" in action started at {}:{}", self.parse_name, self.action_line);
                if token.val.ends_with(" action") {
                    extra = extra.get(" in action".len()..).unwrap_or("").to_owned();
                }
            }
            return self.errorf(format!("{token}{extra}"));
        }
        self.errorf(format!("unexpected {token} in {context}"))
    }

    fn deeper(&mut self) -> Res<()> {
        if self.depth >= MAX_DEPTH {
            return Err(self.errorf("max expression depth exceeded"));
        }
        self.depth += 1;
        Ok(())
    }

    /// parse.go's add: installs the tree unless a nonempty one has the name.
    fn add(&mut self, root: List) -> Res<()> {
        match self.trees.get(&self.name) {
            Some(existing) if !is_empty_list(existing) => {
                if !is_empty_list(&root) {
                    return Err(self.errorf(format!(
                        "template: multiple definition of template {}",
                        strconv::quote(&self.name)
                    )));
                }
            }
            _ => {
                self.trees.insert(self.name.clone(), root);
            }
        }
        Ok(())
    }

    /// parse.go's Parse and parse: the top-level template, with its definitions.
    pub(crate) fn parse(&mut self) -> Res<()> {
        let mut root = List {
            pos: self.peek().pos,
            nodes: Vec::new(),
        };
        while self.peek().typ != T::Eof {
            if self.peek().typ == T::LeftDelim {
                let delim = self.next();
                if self.next_non_space().typ == T::Define {
                    let mut sub = self.sub("definition", 0);
                    sub.parse_definition()?;
                    continue;
                }
                self.backup2(delim);
            }
            match self.text_or_action()? {
                Parsed::Node(n) => root.nodes.push(n),
                end => return Err(self.errorf(format!("unexpected {end}"))),
            }
        }
        self.add(root)
    }

    fn parse_definition(&mut self) -> Res<()> {
        let context = "define clause";
        let name = self.expect_one_of(T::String, T::RawString, context)?;
        self.name = strconv::unquote(&name.val).map_err(|()| self.errorf("invalid syntax"))?;
        self.expect(T::RightDelim, context)?;
        let (root, end) = self.item_list()?;
        if !matches!(end, Parsed::End) {
            return Err(self.errorf(format!("unexpected {end} in {context}")));
        }
        self.add(root)
    }

    fn item_list(&mut self) -> Res<(List, Parsed)> {
        let mut list = List {
            pos: self.peek_non_space().pos,
            nodes: Vec::new(),
        };
        while self.peek_non_space().typ != T::Eof {
            match self.text_or_action()? {
                Parsed::Node(n) => list.nodes.push(n),
                end => return Ok((list, end)),
            }
        }
        Err(self.errorf("unexpected EOF"))
    }

    fn text_or_action(&mut self) -> Res<Parsed> {
        let token = self.next_non_space();
        match token.typ {
            T::Text => Ok(Parsed::Node(Node::Text {
                pos: token.pos,
                text: token.val,
            })),
            T::LeftDelim => {
                self.action_line = token.line;
                let r = self.action();
                self.action_line = 0;
                r
            }
            T::Comment => Ok(Parsed::Node(Node::Comment {
                pos: token.pos,
                text: token.val,
            })),
            _ => Err(self.unexpected(&token, "input")),
        }
    }

    fn action(&mut self) -> Res<Parsed> {
        let token = self.next_non_space();
        match token.typ {
            T::Block => return self.block_control().map(Parsed::Node),
            T::Break | T::Continue => {
                let t = self.next_non_space();
                let what = if token.typ == T::Break {
                    "break"
                } else {
                    "continue"
                };
                if t.typ != T::RightDelim {
                    return Err(self.unexpected(&t, &format!("{{{{{what}}}}}")));
                }
                if self.range_depth == 0 {
                    return Err(self.errorf(format!("{{{{{what}}}}} outside {{{{range}}}}")));
                }
                let pos = token.pos;
                return Ok(Parsed::Node(if token.typ == T::Break {
                    Node::Break { pos }
                } else {
                    Node::Continue { pos }
                }));
            }
            T::Else => return self.else_control(),
            T::End => {
                self.expect(T::RightDelim, "end")?;
                return Ok(Parsed::End);
            }
            T::If => return self.branch("if").map(|b| Parsed::Node(Node::If(b))),
            T::Range => return self.branch("range").map(|b| Parsed::Node(Node::Range(b))),
            T::Template => return self.template_control().map(Parsed::Node),
            T::With => return self.branch("with").map(|b| Parsed::Node(Node::With(b))),
            _ => {}
        }
        self.backup();
        let token = self.peek();
        let pipe = self.pipeline("command", T::RightDelim)?;
        Ok(Parsed::Node(Node::Action { pos: token.pos, pipe }))
    }

    fn pipeline(&mut self, context: &str, end: T) -> Res<Pipe> {
        let token = self.peek_non_space();
        let mut pipe = Pipe {
            pos: token.pos,
            is_assign: false,
            decl: Vec::new(),
            cmds: Vec::new(),
        };
        loop {
            let v = self.peek_non_space();
            if v.typ != T::Variable {
                break;
            }
            self.next();
            let after = self.peek();
            let next = self.peek_non_space();
            if next.typ == T::Assign || next.typ == T::Declare {
                pipe.is_assign = next.typ == T::Assign;
                self.next_non_space();
                pipe.decl.push(variable(v.pos, &v.val));
                self.vars.push(v.val);
            } else if next.typ == T::Char && next.val == "," {
                self.next_non_space();
                pipe.decl.push(variable(v.pos, &v.val));
                self.vars.push(v.val);
                if context == "range" && pipe.decl.len() < 2 {
                    match self.peek_non_space().typ {
                        T::Variable | T::RightDelim | T::RightParen => continue,
                        _ => return Err(self.errorf("range can only initialize variables")),
                    }
                }
                return Err(self.errorf(format!("too many declarations in {context}")));
            } else if after.typ == T::Space {
                self.backup3(v, after);
            } else {
                self.backup2(v);
            }
            break;
        }
        loop {
            let token = self.next_non_space();
            match token.typ {
                t if t == end => {
                    self.check_pipeline(&pipe, context)?;
                    return Ok(pipe);
                }
                T::Bool
                | T::CharConstant
                | T::Complex
                | T::Dot
                | T::Field
                | T::Identifier
                | T::Number
                | T::Nil
                | T::RawString
                | T::String
                | T::Variable
                | T::LeftParen => {
                    self.backup();
                    let cmd = self.command()?;
                    pipe.cmds.push(cmd);
                }
                _ => return Err(self.unexpected(&token, context)),
            }
        }
    }

    fn check_pipeline(&self, pipe: &Pipe, context: &str) -> Res<()> {
        if pipe.cmds.is_empty() {
            return Err(self.errorf(format!("missing value for {context}")));
        }
        for (i, c) in pipe.cmds.iter().enumerate().skip(1) {
            if matches!(
                c.args.first(),
                Some(
                    Arg::Bool { .. }
                        | Arg::Dot { .. }
                        | Arg::Nil { .. }
                        | Arg::Number(_)
                        | Arg::String { .. }
                )
            ) {
                return Err(self.errorf(format!("non executable command in pipeline stage {}", i + 1)));
            }
        }
        Ok(())
    }

    /// parseControl, for if, range and with.
    fn branch(&mut self, context: &str) -> Res<Branch> {
        self.deeper()?;
        let vars = self.vars.len();
        let r = self.branch_inner(context);
        self.vars.truncate(vars);
        self.depth -= 1;
        r
    }

    fn branch_inner(&mut self, context: &str) -> Res<Branch> {
        let pipe = self.pipeline(context, T::RightDelim)?;
        if context == "range" {
            self.range_depth += 1;
        }
        let (list, next) = self.item_list()?;
        if context == "range" {
            self.range_depth -= 1;
        }
        let mut else_list = None;
        if let Parsed::Else { pos } = next {
            if context == "if" && self.peek().typ == T::If {
                self.next();
                let b = self.branch("if")?;
                else_list = Some(List {
                    pos,
                    nodes: vec![Node::If(b)],
                });
            } else if context == "with" && self.peek().typ == T::With {
                self.next();
                let b = self.branch("with")?;
                else_list = Some(List {
                    pos,
                    nodes: vec![Node::With(b)],
                });
            } else {
                let (l, next) = self.item_list()?;
                if !matches!(next, Parsed::End) {
                    return Err(self.errorf(format!("expected end; found {next}")));
                }
                else_list = Some(l);
            }
        }
        Ok(Branch {
            pos: pipe.pos,
            pipe,
            list,
            else_list,
        })
    }

    fn else_control(&mut self) -> Res<Parsed> {
        let peek = self.peek_non_space();
        if peek.typ == T::If || peek.typ == T::With {
            return Ok(Parsed::Else { pos: peek.pos });
        }
        let token = self.expect(T::RightDelim, "else")?;
        Ok(Parsed::Else { pos: token.pos })
    }

    fn block_control(&mut self) -> Res<Node> {
        let context = "block clause";
        let token = self.next_non_space();
        let name = self.template_name(&token, context)?;
        let pipe = self.pipeline(context, T::RightDelim)?;
        self.deeper()?;
        let depth = self.depth;
        let mut block = self.sub(&name, depth);
        let (root, end) = block.item_list()?;
        let ended = matches!(end, Parsed::End);
        if ended {
            block.add(root)?;
        }
        if !ended {
            return Err(self.errorf(format!("unexpected {end} in {context}")));
        }
        self.depth -= 1;
        Ok(Node::Template {
            pos: token.pos,
            name,
            pipe: Some(pipe),
        })
    }

    fn template_control(&mut self) -> Res<Node> {
        let context = "template clause";
        let token = self.next_non_space();
        let name = self.template_name(&token, context)?;
        let mut pipe = None;
        if self.next_non_space().typ != T::RightDelim {
            self.backup();
            pipe = Some(self.pipeline(context, T::RightDelim)?);
        }
        Ok(Node::Template {
            pos: token.pos,
            name,
            pipe,
        })
    }

    fn template_name(&self, token: &Item, context: &str) -> Res<String> {
        match token.typ {
            T::String | T::RawString => {
                strconv::unquote(&token.val).map_err(|()| self.errorf("invalid syntax"))
            }
            _ => Err(self.unexpected(token, context)),
        }
    }

    fn command(&mut self) -> Res<Command> {
        let mut cmd = Command {
            pos: self.peek_non_space().pos,
            args: Vec::new(),
        };
        loop {
            self.peek_non_space();
            if let Some(op) = self.operand()? {
                cmd.args.push(op);
            }
            let token = self.next();
            match token.typ {
                T::Space => continue,
                T::RightDelim | T::RightParen => self.backup(),
                T::Pipe => {}
                _ => return Err(self.unexpected(&token, "operand")),
            }
            break;
        }
        if cmd.args.is_empty() {
            return Err(self.errorf("empty command"));
        }
        Ok(cmd)
    }

    fn operand(&mut self) -> Res<Option<Arg>> {
        let Some(node) = self.term()? else {
            return Ok(None);
        };
        if self.peek().typ != T::Field {
            return Ok(Some(node));
        }
        let pos = self.peek().pos;
        let mut field = Vec::new();
        while self.peek().typ == T::Field {
            let f = self.next().val;
            field.push(f.get(1..).unwrap_or("").to_owned());
        }
        Ok(Some(match node {
            Arg::Field { ident, .. } => Arg::Field {
                pos,
                ident: ident.into_iter().chain(field).collect(),
            },
            Arg::Variable(v) => Arg::Variable(Variable {
                pos,
                ident: v.ident.into_iter().chain(field).collect(),
            }),
            Arg::Bool { .. } | Arg::String { .. } | Arg::Number(_) | Arg::Nil { .. } | Arg::Dot { .. } => {
                return Err(self.errorf(format!(
                    "unexpected . after term {}",
                    strconv::quote(&node.to_string())
                )));
            }
            node => Arg::Chain {
                pos,
                node: Box::new(node),
                field,
            },
        }))
    }

    fn term(&mut self) -> Res<Option<Arg>> {
        let token = self.next_non_space();
        let pos = token.pos;
        Ok(Some(match token.typ {
            T::Identifier => {
                if !(self.has_function)(&token.val) {
                    return Err(self.errorf(format!("function {} not defined", strconv::quote(&token.val))));
                }
                Arg::Identifier { pos, name: token.val }
            }
            T::Dot => Arg::Dot { pos },
            T::Nil => Arg::Nil { pos },
            T::Variable => {
                if !self.vars.contains(&token.val) {
                    return Err(self.errorf(format!("undefined variable {}", strconv::quote(&token.val))));
                }
                Arg::Variable(variable(pos, &token.val))
            }
            T::Field => Arg::Field {
                pos,
                ident: token
                    .val
                    .get(1..)
                    .unwrap_or("")
                    .split('.')
                    .map(str::to_owned)
                    .collect(),
            },
            T::Bool => Arg::Bool {
                pos,
                val: token.val == "true",
            },
            T::CharConstant | T::Complex | T::Number => {
                Arg::Number(number(pos, &token.val, token.typ).map_err(|e| self.errorf(e))?)
            }
            T::LeftParen => {
                self.deeper()?;
                let p = self.pipeline("parenthesized pipeline", T::RightParen);
                self.depth -= 1;
                Arg::Pipe(p?)
            }
            T::String | T::RawString => {
                let text = strconv::unquote(&token.val).map_err(|()| self.errorf("invalid syntax"))?;
                Arg::String {
                    pos,
                    quoted: token.val,
                    text,
                }
            }
            _ => {
                self.backup();
                return Ok(None);
            }
        }))
    }
}

fn variable(pos: usize, name: &str) -> Variable {
    Variable {
        pos,
        ident: name.split('.').map(str::to_owned).collect(),
    }
}

/// node.go's newNumber. Complex constants are left out: shards' values have no complex
/// kind.
fn number(pos: usize, text: &str, typ: T) -> Result<Number, String> {
    let mut n = Number {
        pos,
        is_int: false,
        is_uint: false,
        is_float: false,
        int: 0,
        uint: 0,
        float: 0.0,
        text: text.to_owned(),
    };
    if typ == T::CharConstant {
        let quote = *text.as_bytes().first().ok_or("invalid syntax")?;
        let rest = text.get(1..).unwrap_or("");
        let (r, _, used) = strconv::unquote_char(rest, quote).map_err(|()| "invalid syntax")?;
        if rest.get(used..) != Some("'") {
            return Err(format!("malformed character constant: {text}"));
        }
        n.int = i64::from(r);
        n.is_int = true;
        n.uint = u64::from(r);
        n.is_uint = true;
        n.float = f64::from(r);
        n.is_float = true;
        return Ok(n);
    }
    let imaginary = text
        .strip_suffix('i')
        .is_some_and(|t| strconv::parse_float(t).is_some());
    if typ == T::Complex || imaginary {
        return Err(format!("complex constants are not supported: {text}"));
    }
    let u = strconv::parse_uint(text);
    if let Some(u) = u {
        n.is_uint = true;
        n.uint = u;
    }
    if let Some(i) = strconv::parse_int(text) {
        n.is_int = true;
        n.int = i;
        if i == 0 {
            n.is_uint = true;
            n.uint = u.unwrap_or(0);
        }
    }
    if n.is_int {
        n.is_float = true;
        n.float = n.int as f64;
    } else if n.is_uint {
        n.is_float = true;
        n.float = n.uint as f64;
    } else if let Some(f) = strconv::parse_float(text) {
        if !text.contains(['.', 'e', 'E', 'p', 'P']) {
            return Err(format!("integer overflow: {}", strconv::quote(text)));
        }
        n.is_float = true;
        n.float = f;
        if !n.is_int && (f as i64) as f64 == f {
            n.is_int = true;
            n.int = f as i64;
        }
        if !n.is_uint && (f as u64) as f64 == f {
            n.is_uint = true;
            n.uint = f as u64;
        }
    }
    if !n.is_int && !n.is_uint && !n.is_float {
        return Err(format!("illegal number syntax: {}", strconv::quote(text)));
    }
    Ok(n)
}
