//! Dockerfiles as BuildKit's Dockerfile frontend reads them (moby/buildkit
//! dockerfile/1.27.1), for `shards build` (docs/research/image-build.md):
//! - `parser`: the text into instructions, with BuildKit's errors and warnings;
//! - `lex`: words expanded and split as BuildKit's shell-like lexer does it;
//! - `instructions`: lines into typed instructions and build stages, with their flags;
//! - `lint`: the build checks, as `# check=` configures them;
//! - `go`: the Go string handling both inherit.
//!
//! Each is held to BuildKit's own code byte for byte by `tests/oracle.rs`, against what
//! scripts/dockerfile/generate records BuildKit doing; each module's documentation lists
//! where it deliberately does better, and testdata/deviations.json each case of it.

pub mod go;
pub mod instructions;
mod json;
pub mod lex;
pub mod lint;
pub mod parser;
mod tables;
