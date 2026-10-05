//! text/template/exec.go: walking a parse tree against data, with Go's variable
//! scoping, truth, range, calls, argument checks and error texts.

use std::borrow::Cow;
use std::rc::Rc;

use crate::fmt::{sprint, sprint_printable};
use crate::funcs::{self, Func, truth};
use crate::parse::{Arg, Branch, Command, List, Node, Number, Pipe, Trees, Variable};
use crate::reflect::{Key, Param, R, child, indirect, indirect_interface, is_true};
use crate::strconv;
use crate::value::{Kind, Object, Value};

/// The deepest nesting of template calls and of if, range and with, together. Go
/// counts template calls alone and allows 100000, on goroutine stacks that grow; this
/// executor recurses on a thread's fixed stack. Measured on a 2 MiB stack (Rust's
/// default for a spawned thread) in a debug build, a template call with a with in it
/// costs about 3.2 KiB (649 such levels overflow it), so 200 levels take about 320 KiB,
/// leaving room for the deepest expression the parser allows (parse.rs's MAX_DEPTH).
/// lib.rs's tests run the deepest allowed on such a stack.
pub(crate) const MAX_EXEC_DEPTH: usize = 200;

/// The templates one parse defined, and what their errors cite.
#[derive(Debug, Clone)]
pub(crate) struct Set {
    pub trees: Trees,
    pub text: String,
    pub parse_name: String,
}

/// Why a walk stopped early: an error, or a break or continue in a range.
pub(crate) enum Flow {
    Err(String),
    Break,
    Continue,
}

type X<T> = Result<T, Flow>;

/// The node being evaluated, for errors.
#[derive(Clone, Copy)]
enum At<'t> {
    Nothing,
    Node(&'t Node),
    List(&'t List),
    Pipe(&'t Pipe),
    Cmd(&'t Command),
    Arg(&'t Arg),
}

impl At<'_> {
    fn pos(&self) -> usize {
        match self {
            At::Nothing => 0,
            At::Node(n) => n.pos(),
            At::List(l) => l.pos,
            At::Pipe(p) => p.pos,
            At::Cmd(c) => c.pos,
            At::Arg(a) => a.pos(),
        }
    }

    fn context(&self) -> String {
        match self {
            At::Nothing => String::new(),
            At::Node(n) => n.to_string(),
            At::List(l) => l.to_string(),
            At::Pipe(p) => p.to_string(),
            At::Cmd(c) => c.to_string(),
            At::Arg(a) => a.to_string(),
        }
    }
}

/// The last argument of a command: the value piped in, if any (exec.go's missingVal).
enum Fin<'d> {
    Missing,
    Val(R<'d>),
}

impl Fin<'_> {
    fn present(&self) -> bool {
        matches!(self, Fin::Val(_))
    }
}

/// What a command calls.
enum Callee {
    Func(Func),
    Method(Rc<dyn Object>, &'static [Kind]),
}

pub(crate) struct State<'t, 'd, 'o> {
    set: &'t Set,
    name: &'t str,
    out: &'o mut String,
    node: At<'t>,
    vars: Vec<(&'t str, R<'d>)>,
    depth: usize,
    header: bool,
    /// `missingkey=error`: a map's missing key is an error, not `<no value>`.
    missing_key_error: bool,
}

/// Runs the template `name` of the set against `data`, writing to `out`.
pub(crate) fn execute(
    set: &Set,
    name: &str,
    data: &Value,
    header: bool,
    missing_key_error: bool,
    out: &mut String,
) -> Result<(), String> {
    let dot = if data.is_nil() {
        R::Invalid
    } else {
        R::Plain(Cow::Borrowed(data))
    };
    let mut s = State {
        set,
        name,
        out,
        node: At::Nothing,
        vars: vec![("$", dot.clone())],
        depth: 0,
        header,
        missing_key_error,
    };
    let Some(root) = set.trees.get(name) else {
        return Err(s.err(format!(
            "{} is an incomplete or empty template",
            strconv::quote(name)
        )));
    };
    match s.walk_list(&dot, root) {
        Ok(()) => Ok(()),
        Err(Flow::Err(e)) => Err(e),
        Err(Flow::Break | Flow::Continue) => Ok(()),
    }
}

impl<'t, 'd> State<'t, 'd, '_> {
    fn at(&mut self, node: At<'t>) {
        self.node = node;
    }

    /// exec.go's errorf.
    fn err(&self, msg: impl std::fmt::Display) -> String {
        if let At::Nothing = self.node {
            return format!("template: {}: {msg}", self.name);
        }
        let pos = self.node.pos();
        let text = self.set.text.get(..pos).unwrap_or(&self.set.text);
        let byte = match text.rfind('\n') {
            None => pos,
            Some(i) => pos - (i + 1),
        };
        let line = 1 + text.bytes().filter(|&b| b == b'\n').count();
        format!(
            "template: {}:{line}:{byte}: executing {} at <{}>: {msg}",
            self.set.parse_name,
            strconv::quote(self.name),
            self.node.context()
        )
    }

    fn errorf<T>(&self, msg: impl std::fmt::Display) -> X<T> {
        Err(Flow::Err(self.err(msg)))
    }

    fn set_var(&mut self, name: &str, value: R<'d>) -> X<()> {
        if let Some(v) = self.vars.iter_mut().rev().find(|(n, _)| *n == name) {
            v.1 = value;
            return Ok(());
        }
        self.errorf(format!("undefined variable: {name}"))
    }

    fn set_top_var(&mut self, n: usize, value: R<'d>) {
        let i = self.vars.len().wrapping_sub(n);
        if let Some(v) = self.vars.get_mut(i) {
            v.1 = value;
        }
    }

    fn var_value(&self, name: &str) -> X<R<'d>> {
        match self.vars.iter().rev().find(|(n, _)| *n == name) {
            Some((_, v)) => Ok(v.clone()),
            None => self.errorf(format!("undefined variable: {name}")),
        }
    }

    fn walk_list(&mut self, dot: &R<'d>, list: &'t List) -> X<()> {
        self.at(At::List(list));
        for n in &list.nodes {
            self.walk(dot, n)?;
        }
        Ok(())
    }

    fn walk(&mut self, dot: &R<'d>, node: &'t Node) -> X<()> {
        self.at(At::Node(node));
        match node {
            Node::Action { pipe, .. } => {
                let val = self.eval_pipeline(dot, Some(pipe))?;
                if pipe.decl.is_empty() {
                    self.print_value(node, &val);
                }
                Ok(())
            }
            Node::Break { .. } => Err(Flow::Break),
            Node::Continue { .. } => Err(Flow::Continue),
            Node::Comment { .. } => Ok(()),
            Node::If(b) => self.walk_if_or_with(false, dot, b),
            Node::With(b) => self.walk_if_or_with(true, dot, b),
            Node::Range(b) => self.walk_range(dot, node, b),
            Node::Template { name, pipe, .. } => self.walk_template(dot, node, name, pipe.as_ref()),
            Node::Text { text, .. } => {
                self.out.push_str(text);
                Ok(())
            }
        }
    }

    /// exec.go's check on template depth, made for control nesting too.
    fn too_deep(&self) -> X<()> {
        if self.depth >= MAX_EXEC_DEPTH {
            return self.errorf(format!("exceeded maximum template depth ({MAX_EXEC_DEPTH})"));
        }
        Ok(())
    }

    fn walk_if_or_with(&mut self, with: bool, dot: &R<'d>, b: &'t Branch) -> X<()> {
        self.too_deep()?;
        let mark = self.vars.len();
        self.depth += 1;
        let r = self.if_or_with(with, dot, b);
        self.depth -= 1;
        self.vars.truncate(mark);
        r
    }

    fn if_or_with(&mut self, with: bool, dot: &R<'d>, b: &'t Branch) -> X<()> {
        let val = self.eval_pipeline(dot, Some(&b.pipe))?;
        if is_true(&indirect_interface(val.clone())) {
            if with {
                self.walk_list(&val, &b.list)
            } else {
                self.walk_list(dot, &b.list)
            }
        } else if let Some(e) = &b.else_list {
            self.walk_list(dot, e)
        } else {
            Ok(())
        }
    }

    fn walk_range(&mut self, dot: &R<'d>, node: &'t Node, r: &'t Branch) -> X<()> {
        self.at(At::Node(node));
        self.too_deep()?;
        let mark = self.vars.len();
        self.depth += 1;
        let res = self.range(dot, r);
        self.depth -= 1;
        self.vars.truncate(mark);
        match res {
            Err(Flow::Break) => Ok(()),
            res => res,
        }
    }

    fn range(&mut self, dot: &R<'d>, r: &'t Branch) -> X<()> {
        let (val, _) = indirect(self.eval_pipeline(dot, Some(&r.pipe))?);
        let mark = self.vars.len();
        let items: Vec<(R<'d>, R<'d>)> = match &val {
            R::Invalid => Vec::new(),
            R::Plain(v) | R::Iface(v) => match v.as_ref() {
                Value::Int(_) | Value::Uint(_) if r.pipe.decl.len() > 1 => {
                    return self.errorf(format!(
                        "can't use {} to iterate over more than one variable",
                        sprint(std::slice::from_ref(v.as_ref()))
                    ));
                }
                Value::Int(n) => {
                    for i in 0..*n {
                        self.one_iteration(r, mark, R::Invalid, R::owned(Value::Int(i)))?;
                    }
                    if *n > 0 {
                        return Ok(());
                    }
                    Vec::new()
                }
                Value::Uint(n) => {
                    for i in 0..*n {
                        self.one_iteration(r, mark, R::Invalid, R::owned(Value::Uint(i)))?;
                    }
                    if *n > 0 {
                        return Ok(());
                    }
                    Vec::new()
                }
                Value::List(kind, items) => (0..items.len())
                    .filter_map(|i| {
                        let e = child(v, Key::Index(i))?;
                        let index = R::owned(Value::Int(i64::try_from(i).unwrap_or(i64::MAX)));
                        Some((index, R::elem(*kind, e)))
                    })
                    .collect(),
                Value::Map(kind, m) => m
                    .keys()
                    .filter_map(|k| {
                        let e = child(v, Key::Name(k))?;
                        Some((R::owned(Value::String(k.clone())), R::elem(*kind, e)))
                    })
                    .collect(),
                other => {
                    return self.errorf(format!(
                        "range can't iterate over {}",
                        sprint_printable(std::slice::from_ref(other))
                    ));
                }
            },
        };
        if !items.is_empty() {
            for (index, elem) in items {
                self.one_iteration(r, mark, index, elem)?;
            }
            return Ok(());
        }
        if let Some(e) = &r.else_list {
            self.walk_list(dot, e)?;
        }
        Ok(())
    }

    fn one_iteration(&mut self, r: &'t Branch, mark: usize, index: R<'d>, elem: R<'d>) -> X<()> {
        let decl = &r.pipe.decl;
        let name = |i: usize| {
            decl.get(i)
                .and_then(|v| v.ident.first())
                .map_or("", String::as_str)
        };
        if !decl.is_empty() {
            if r.pipe.is_assign {
                if decl.len() > 1 {
                    self.set_var(name(0), index.clone())?;
                } else {
                    self.set_var(name(0), elem.clone())?;
                }
            } else {
                self.set_top_var(1, elem.clone());
            }
        }
        if decl.len() > 1 {
            if r.pipe.is_assign {
                self.set_var(name(1), elem.clone())?;
            } else {
                self.set_top_var(2, index);
            }
        }
        let res = self.walk_list(&elem, &r.list);
        self.vars.truncate(mark);
        match res {
            Err(Flow::Continue) => Ok(()),
            res => res,
        }
    }

    fn walk_template(&mut self, dot: &R<'d>, node: &'t Node, name: &'t str, pipe: Option<&'t Pipe>) -> X<()> {
        self.at(At::Node(node));
        let set = self.set;
        let Some(root) = set.trees.get(name) else {
            return self.errorf(format!("template {} not defined", strconv::quote(name)));
        };
        self.too_deep()?;
        let dot = self.eval_pipeline(dot, pipe)?;
        let saved_vars = std::mem::replace(&mut self.vars, vec![("$", dot.clone())]);
        let saved_name = std::mem::replace(&mut self.name, name);
        let saved_node = self.node;
        self.depth += 1;
        let res = self.walk_list(&dot, root);
        self.depth -= 1;
        self.vars = saved_vars;
        self.name = saved_name;
        self.node = saved_node;
        res
    }

    fn eval_pipeline(&mut self, dot: &R<'d>, pipe: Option<&'t Pipe>) -> X<R<'d>> {
        let Some(pipe) = pipe else {
            return Ok(R::Invalid);
        };
        self.at(At::Pipe(pipe));
        let mut value = Fin::Missing;
        for cmd in &pipe.cmds {
            let v = self.eval_command(dot, cmd, value)?;
            value = Fin::Val(indirect_interface(v));
        }
        let value = match value {
            Fin::Val(v) => v,
            Fin::Missing => R::Invalid,
        };
        for var in &pipe.decl {
            let name = var.ident.first().map_or("", String::as_str);
            if pipe.is_assign {
                self.set_var(name, value.clone())?;
            } else {
                self.vars.push((name, value.clone()));
            }
        }
        Ok(value)
    }

    fn not_a_function(&self, args: &'t [Arg], fin: &Fin<'d>) -> X<()> {
        if args.len() > 1 || fin.present() {
            let first = args.first().map(ToString::to_string).unwrap_or_default();
            return self.errorf(format!("can't give argument to non-function {first}"));
        }
        Ok(())
    }

    fn eval_command(&mut self, dot: &R<'d>, cmd: &'t Command, fin: Fin<'d>) -> X<R<'d>> {
        let Some(first) = cmd.args.first() else {
            return self.errorf("empty command");
        };
        match first {
            Arg::Field { ident, .. } => return self.eval_field_node(dot, first, ident, &cmd.args, fin),
            Arg::Chain { node, field, .. } => {
                return self.eval_chain_node(dot, first, node, field, &cmd.args, fin);
            }
            Arg::Identifier { name, .. } => {
                return self.eval_function(dot, first, name, At::Cmd(cmd), &cmd.args, fin);
            }
            Arg::Pipe(p) => {
                self.not_a_function(&cmd.args, &fin)?;
                return self.eval_pipeline(dot, Some(p));
            }
            Arg::Variable(v) => return self.eval_variable_node(dot, first, v, &cmd.args, fin),
            _ => {}
        }
        self.at(At::Arg(first));
        self.not_a_function(&cmd.args, &fin)?;
        match first {
            Arg::Bool { val, .. } => Ok(R::owned(Value::Bool(*val))),
            Arg::Dot { .. } => Ok(dot.clone()),
            Arg::Nil { .. } => self.errorf("nil is not a command"),
            Arg::Number(n) => self.ideal_constant(first, n),
            Arg::String { text, .. } => Ok(R::owned(Value::String(text.clone()))),
            _ => self.errorf(format!(
                "can't evaluate command {}",
                strconv::quote(&first.to_string())
            )),
        }
    }

    /// exec.go's idealConstant: a number's type, from its syntax alone.
    fn ideal_constant(&mut self, arg: &'t Arg, n: &Number) -> X<R<'d>> {
        self.at(At::Arg(arg));
        let t = n.text.as_bytes();
        let is_hex_int = t.len() > 2
            && t.first() == Some(&b'0')
            && matches!(t.get(1), Some(b'x' | b'X'))
            && !n.text.contains(['p', 'P']);
        let is_rune = t.first() == Some(&b'\'');
        if n.is_float && !is_hex_int && !is_rune && n.text.contains(['.', 'e', 'E', 'p', 'P']) {
            return Ok(R::owned(Value::Float(n.float)));
        }
        if n.is_int {
            return Ok(R::owned(Value::Int(n.int)));
        }
        if n.is_uint {
            return self.errorf(format!("{} overflows int", n.text));
        }
        Ok(R::Invalid)
    }

    fn eval_field_node(
        &mut self,
        dot: &R<'d>,
        node: &'t Arg,
        ident: &'t [String],
        args: &'t [Arg],
        fin: Fin<'d>,
    ) -> X<R<'d>> {
        self.at(At::Arg(node));
        self.eval_field_chain(dot, dot.clone(), node, ident, args, fin)
    }

    fn eval_chain_node(
        &mut self,
        dot: &R<'d>,
        node: &'t Arg,
        inner: &'t Arg,
        field: &'t [String],
        args: &'t [Arg],
        fin: Fin<'d>,
    ) -> X<R<'d>> {
        self.at(At::Arg(node));
        if field.is_empty() {
            return self.errorf("internal error: no fields in evalChainNode");
        }
        if let Arg::Nil { .. } = inner {
            return self.errorf(format!("indirection through explicit nil in {node}"));
        }
        let pipe = self.eval_arg(dot, None, inner)?;
        self.eval_field_chain(dot, pipe, node, field, args, fin)
    }

    fn eval_variable_node(
        &mut self,
        dot: &R<'d>,
        node: &'t Arg,
        v: &'t Variable,
        args: &'t [Arg],
        fin: Fin<'d>,
    ) -> X<R<'d>> {
        self.at(At::Arg(node));
        let name = v.ident.first().map_or("", String::as_str);
        let value = self.var_value(name)?;
        if v.ident.len() == 1 {
            self.not_a_function(args, &fin)?;
            return Ok(value);
        }
        self.eval_field_chain(dot, value, node, v.ident.get(1..).unwrap_or(&[]), args, fin)
    }

    fn eval_field_chain(
        &mut self,
        dot: &R<'d>,
        mut receiver: R<'d>,
        node: &'t Arg,
        ident: &'t [String],
        args: &'t [Arg],
        fin: Fin<'d>,
    ) -> X<R<'d>> {
        let Some((last, rest)) = ident.split_last() else {
            return Ok(receiver);
        };
        for name in rest {
            receiver = self.eval_field(dot, name, node, &[], Fin::Missing, receiver)?;
        }
        self.eval_field(dot, last, node, args, fin, receiver)
    }

    fn eval_function(
        &mut self,
        dot: &R<'d>,
        node: &'t Arg,
        name: &'t str,
        cmd: At<'t>,
        args: &'t [Arg],
        fin: Fin<'d>,
    ) -> X<R<'d>> {
        self.at(At::Arg(node));
        let Some((f, builtin)) = funcs::find(name, self.header) else {
            return self.errorf(format!("{} is not a defined function", strconv::quote(name)));
        };
        self.eval_call(dot, Callee::Func(f), builtin, cmd, name, args, fin)
    }

    /// exec.go's evalField: a method, a struct field or a map key.
    fn eval_field(
        &mut self,
        dot: &R<'d>,
        name: &'t str,
        node: &'t Arg,
        args: &'t [Arg],
        fin: Fin<'d>,
        receiver: R<'d>,
    ) -> X<R<'d>> {
        if let R::Invalid = receiver {
            return Ok(R::Invalid);
        }
        let typ = receiver.type_name();
        let (receiver, is_nil) = indirect(receiver);
        if is_nil {
            return self.errorf(format!("nil pointer evaluating {typ}.{name}"));
        }
        let has_args = args.len() > 1 || fin.present();
        if let R::Plain(v) = &receiver {
            match v.as_ref() {
                Value::Object(o) => {
                    if let Some(params) = o.method(name) {
                        let callee = Callee::Method(Rc::clone(o), params);
                        return self.eval_call(dot, callee, false, At::Arg(node), name, args, fin);
                    }
                    if let Some(f) = o.field(name) {
                        if has_args {
                            return self
                                .errorf(format!("{name} has arguments but cannot be invoked as function"));
                        }
                        return Ok(R::owned(f));
                    }
                }
                Value::Map(kind, _) => {
                    if has_args {
                        return self.errorf(format!("{name} is not a method but has arguments"));
                    }
                    return match child(v, Key::Name(name)) {
                        Some(e) => Ok(R::elem(*kind, e)),
                        None if self.missing_key_error => {
                            self.errorf(format!("map has no entry for key {}", strconv::quote(name)))
                        }
                        None => Ok(R::Invalid),
                    };
                }
                _ => {}
            }
        }
        self.errorf(format!("can't evaluate field {name} in type {typ}"))
    }

    /// exec.go's evalCall.
    #[allow(clippy::too_many_arguments)]
    fn eval_call(
        &mut self,
        dot: &R<'d>,
        callee: Callee,
        builtin: bool,
        node: At<'t>,
        name: &'t str,
        args: &'t [Arg],
        fin: Fin<'d>,
    ) -> X<R<'d>> {
        let args = args.get(1..).unwrap_or(&[]);
        let (params, variadic): (Vec<Param>, Option<Param>) = match &callee {
            Callee::Func(f) => {
                let sig = f.sig();
                (sig.params.to_vec(), sig.variadic)
            }
            Callee::Method(_, kinds) => (kinds.iter().map(|k| Param::of(*k)).collect(), None),
        };
        let num_in = args.len() + usize::from(fin.present());
        let num_fixed;
        if variadic.is_some() {
            num_fixed = params.len();
            if num_in < num_fixed {
                return self.errorf(format!(
                    "wrong number of args for {name}: want at least {} got {}",
                    num_fixed,
                    args.len()
                ));
            }
        } else {
            num_fixed = args.len();
            if num_in != params.len() {
                return self.errorf(format!(
                    "wrong number of args for {name}: want {} got {num_in}",
                    params.len()
                ));
            }
        }
        if builtin && let Callee::Func(f @ (Func::And | Func::Or)) = callee {
            let or = f == Func::Or;
            let mut v = R::Invalid;
            for arg in args {
                v = self.eval_arg(dot, Some(Param::Value), arg)?;
                if truth(v.clone()) == or {
                    return Ok(v);
                }
            }
            if let Fin::Val(fin) = fin {
                v = self.validate_type(fin, Some(Param::Value))?;
            }
            return Ok(v);
        }
        let mut argv: Vec<R<'d>> = Vec::with_capacity(num_in);
        for (i, arg) in args.iter().enumerate() {
            let typ = if i < num_fixed {
                params.get(i).copied()
            } else {
                variadic
            };
            argv.push(self.eval_arg(dot, typ, arg)?);
        }
        let mut callee_name = String::new();
        if let Fin::Val(fin) = fin {
            let t = match variadic {
                Some(v) if num_in > num_fixed => Some(v),
                Some(_) => params.get(num_in - 1).copied(),
                None => params.last().copied(),
            };
            if args.is_empty() {
                callee_name = match fin.value() {
                    Some(Value::String(s)) => s.clone(),
                    _ => format!("<{} Value>", fin.type_name()),
                };
            }
            argv.push(self.validate_type(fin, t)?);
        }
        let result = match callee {
            Callee::Func(Func::Call) if builtin => {
                if let Some(a) = args.first() {
                    callee_name = a.to_string();
                }
                let fun = indirect_interface(argv.into_iter().next().unwrap_or(R::Invalid));
                match fun {
                    R::Invalid => Err("call of nil".to_string()),
                    f => Err(format!("non-function {callee_name} of type {}", f.type_name())),
                }
            }
            Callee::Func(f) => f.call(argv),
            Callee::Method(o, _) => {
                let vals: Vec<Value> = argv.iter().map(R::to_any).collect();
                o.call(name, &vals).map(R::owned)
            }
        };
        match result {
            Ok(v) => Ok(v),
            Err(e) => {
                self.at(node);
                self.errorf(format!("error calling {name}: {e}"))
            }
        }
    }

    /// exec.go's validateType: the value, if assignable to the parameter.
    fn validate_type(&self, value: R<'d>, typ: Option<Param>) -> X<R<'d>> {
        let Some(typ) = typ else {
            return Ok(value);
        };
        match value {
            R::Invalid => match typ {
                Param::Any => Ok(R::Iface(Cow::Owned(Value::Nil))),
                Param::Value => Ok(R::Invalid),
                t => self.errorf(format!("invalid value; expected {t}")),
            },
            v if matches!(typ, Param::Value | Param::Any) => Ok(v),
            v => {
                let v = match v {
                    R::Iface(x) if !x.is_nil() => R::Plain(x),
                    v => v,
                };
                match &v {
                    R::Plain(x) if typ.accepts(x) => Ok(v),
                    _ => self.errorf(format!(
                        "wrong type for value; expected {typ}; got {}",
                        v.type_name()
                    )),
                }
            }
        }
    }

    /// exec.go's evalArg.
    fn eval_arg(&mut self, dot: &R<'d>, typ: Option<Param>, n: &'t Arg) -> X<R<'d>> {
        self.at(At::Arg(n));
        match n {
            Arg::Dot { .. } => return self.validate_type(dot.clone(), typ),
            Arg::Nil { .. } => {
                return match typ {
                    Some(Param::Any) => Ok(R::Iface(Cow::Owned(Value::Nil))),
                    Some(Param::Value) => Ok(R::Invalid),
                    Some(t) => self.errorf(format!("cannot assign nil to {t}")),
                    None => self.errorf("cannot assign nil to <nil>"),
                };
            }
            Arg::Field { ident, .. } => {
                let v = self.eval_field_node(dot, n, ident, std::slice::from_ref(n), Fin::Missing)?;
                return self.validate_type(v, typ);
            }
            Arg::Variable(var) => {
                let v = self.eval_variable_node(dot, n, var, &[], Fin::Missing)?;
                return self.validate_type(v, typ);
            }
            Arg::Pipe(p) => {
                let v = self.eval_pipeline(dot, Some(p))?;
                return self.validate_type(v, typ);
            }
            Arg::Identifier { name, .. } => {
                let v = self.eval_function(dot, n, name, At::Arg(n), &[], Fin::Missing)?;
                return self.validate_type(v, typ);
            }
            Arg::Chain { node, field, .. } => {
                let v = self.eval_chain_node(dot, n, node, field, &[], Fin::Missing)?;
                return self.validate_type(v, typ);
            }
            _ => {}
        }
        let found = |s: &Self, what: &str| s.errorf(format!("expected {what}; found {n}"));
        match (typ, n) {
            (Some(Param::Bool), Arg::Bool { val, .. }) => Ok(R::owned(Value::Bool(*val))),
            (Some(Param::Bool), _) => found(self, "bool"),
            (Some(Param::Float), Arg::Number(num)) if num.is_float => Ok(R::owned(Value::Float(num.float))),
            (Some(Param::Float), _) => found(self, "float"),
            (Some(Param::Int), Arg::Number(num)) if num.is_int => Ok(R::owned(Value::Int(num.int))),
            (Some(Param::Int), _) => found(self, "integer"),
            (Some(Param::Uint), Arg::Number(num)) if num.is_uint => Ok(R::owned(Value::Uint(num.uint))),
            (Some(Param::Uint), _) => found(self, "unsigned integer"),
            (Some(Param::String), Arg::String { text, .. }) => Ok(R::owned(Value::String(text.clone()))),
            (Some(Param::String), _) => found(self, "string"),
            (Some(Param::Any | Param::Value), _) => self.eval_empty_interface(dot, n),
            (None, _) => self.errorf(format!("can't handle {n} for arg of type <nil>")),
        }
    }

    /// exec.go's evalEmptyInterface, for the constants evalArg leaves it.
    fn eval_empty_interface(&mut self, _dot: &R<'d>, n: &'t Arg) -> X<R<'d>> {
        self.at(At::Arg(n));
        match n {
            Arg::Bool { val, .. } => Ok(R::owned(Value::Bool(*val))),
            Arg::Number(num) => self.ideal_constant(n, num),
            Arg::String { text, .. } => Ok(R::owned(Value::String(text.clone()))),
            _ => self.errorf(format!(
                "can't handle assignment of {n} to empty interface argument"
            )),
        }
    }

    /// exec.go's printValue.
    fn print_value(&mut self, node: &'t Node, v: &R<'d>) {
        self.at(At::Node(node));
        match v {
            R::Invalid => self.out.push_str("<no value>"),
            R::Plain(x) | R::Iface(x) => self
                .out
                .push_str(&sprint_printable(std::slice::from_ref(x.as_ref()))),
        }
    }
}
