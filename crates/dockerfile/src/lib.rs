//! Dockerfiles as BuildKit's Dockerfile frontend reads them (moby/buildkit
//! dockerfile/1.27.1), for `shards build` (docs/research/image-build.md):
//! - `parser`: the text into instructions, with BuildKit's errors and warnings;
//! - `lex`: words expanded and split as BuildKit's shell-like lexer does it;
//! - `instructions`: lines into typed instructions and build stages, with their flags;
//! - `lint`: the build checks, as `# check=` configures them;
//! - `plan`: the build planned as BuildKit's Dockerfile2LLB plans it, into `llb`'s
//!   graph: stages, steps, mounts, file operations, the image config and its history;
//! - `image`: an image's config, read and written as BuildKit's Go reads and writes it;
//! - `commit`: `docker commit --change` and `docker import --change` applied to a config as
//!   moby's dockerd applies them (held to dockerd by `tests/commit.rs`);
//! - `platform`: OCI platforms as containerd parses and formats them;
//! - `go`: the Go string, path and time handling all of them inherit.
//!
//! Each is held to BuildKit's own code byte for byte by `tests/oracle.rs`, against what
//! scripts/dockerfile/generate records BuildKit doing; each module's documentation lists
//! where it deliberately does better, and testdata/deviations.json each case of it.

pub mod agentfile;
pub mod commit;
pub mod dockerui;
pub mod export;
pub mod git;
pub mod glob;
pub mod go;
pub mod ignore;
pub mod image;
pub mod instructions;
pub(crate) mod json;
pub use json::compact as json_compact;
pub mod lex;
pub mod lint;
pub mod llb;
pub mod parser;
pub mod pb;
pub mod plan;
pub mod platform;
pub mod subrequests;
mod tables;
pub mod url;
