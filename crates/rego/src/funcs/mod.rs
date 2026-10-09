//! OPA's builtin functions (topdown/*.go, v1.14.1), as buildx's policies may call them.
//!
//! Each is held to OPA by `tests/builtins.rs` against what `scripts/rego/generate`
//! records OPA's own function (`topdown.GetBuiltin`) returning for the calls in
//! `testdata/calls-*.json`: its result, undefined, or its error's kind and text.

use std::collections::HashMap;

use crate::value::Value;

mod collections;
mod encoding;
mod regex;
mod strings;
mod time;

/// Why a builtin failed, as OPA's `handleBuiltinErr` tells them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuiltinError {
    /// builtins.ErrOperand: an `eval_type_error` reading `<name>: <message>`.
    Operand(String),
    /// Any other error: an `eval_builtin_error` reading `<name>: <message>`.
    Other(String),
    /// topdown.Halt.
    Halt(String),
}

impl BuiltinError {
    /// builtins.NewOperandErr.
    pub fn operand(pos: usize, msg: impl std::fmt::Display) -> BuiltinError {
        BuiltinError::Operand(format!("operand {pos} {msg}"))
    }

    /// builtins.NewOperandTypeErr.
    pub fn operand_type(pos: usize, got: &Value, expected: &[&str]) -> BuiltinError {
        match expected {
            [one] => BuiltinError::operand(pos, format!("must be {one} but got {}", got.type_name())),
            _ => BuiltinError::operand(
                pos,
                format!(
                    "must be one of {{{}}} but got {}",
                    expected.join(", "),
                    got.type_name()
                ),
            ),
        }
    }

    /// The error's text, without the builtin's name.
    pub fn message(&self) -> &str {
        match self {
            BuiltinError::Operand(m) | BuiltinError::Other(m) | BuiltinError::Halt(m) => m,
        }
    }
}

/// What a builtin call may read beyond its operands (OPA's `BuiltinContext`): the
/// query's time, its seed for random numbers, and caches that live for one query.
#[derive(Debug, Default)]
pub struct Context {
    /// Nanoseconds since the epoch, fixed for the query (`time.now_ns`).
    pub time_ns: i64,
    /// Bytes read for randomness, in order, as OPA reads its `Seed`.
    pub seed: Vec<u8>,
    pub seed_at: usize,
    /// Per-query results of nondeterministic builtins, by name and operands.
    pub cache: HashMap<(String, Vec<Value>), Value>,
    /// Where more seed comes from once `seed` is read: the system's random source, as
    /// OPA's default Seed is crypto/rand's Reader; none for a seed given whole.
    pub fill: Option<Fill>,
}

/// What fills a buffer with random bytes, or says why it could not.
pub type Fill = fn(&mut [u8]) -> Result<(), String>;

/// A builtin: its operands to its result, `None` when undefined.
pub type Builtin = fn(&mut Context, &[Value]) -> Result<Option<Value>, BuiltinError>;

/// The builtin of this name, if OPA has one and it is ported.
pub fn lookup(name: &str) -> Option<Builtin> {
    strings::lookup(name)
        .or_else(|| regex::lookup(name))
        .or_else(|| encoding::lookup(name))
        .or_else(|| collections::lookup(name))
        .or_else(|| time::lookup(name))
}

// Operand helpers shared by every group, as topdown/builtins reads operands.

pub fn string_operand(v: &Value, pos: usize) -> Result<&str, BuiltinError> {
    v.as_str()
        .ok_or_else(|| BuiltinError::operand_type(pos, v, &["string"]))
}

pub fn number_operand(v: &Value, pos: usize) -> Result<&crate::value::Number, BuiltinError> {
    match v {
        Value::Number(n) => Ok(n),
        _ => Err(BuiltinError::operand_type(pos, v, &["number"])),
    }
}

/// builtins.IntOperand.
pub fn int_operand(v: &Value, pos: usize) -> Result<i64, BuiltinError> {
    let n = number_operand(v, pos)?;
    n.as_i64()
        .ok_or_else(|| BuiltinError::operand(pos, "must be integer number but got floating-point number"))
}

pub fn arg(args: &[Value], i: usize) -> Result<&Value, BuiltinError> {
    args.get(i)
        .ok_or_else(|| BuiltinError::Other(format!("missing operand {}", i + 1)))
}
