//! Rego as OPA v1.14.1 evaluates it, set up as buildx v0.37.1's build policies set it
//! up (docker/buildx policy/validate.go): Rego v1, buildx's builtins and functions, and
//! partial evaluation that decides, as OPA's does, which unknown parts of the input a
//! decision needs. Held to OPA by `tests/oracle.rs` against `scripts/rego/generate`.

pub mod ast;
pub mod builtins;
pub mod compare;
pub mod goquote;
pub mod number;
pub mod parser;
pub mod scanner;
pub mod types;
pub mod value;
pub mod compile;
