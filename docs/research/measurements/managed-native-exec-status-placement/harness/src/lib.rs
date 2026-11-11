//! A component fixture of the frozen native exec and canonical status mechanisms.
#![cfg(all(test, target_os = "linux"))]

extern crate self as shards_ipc;

pub mod child_fds;
mod lifecycle;
mod native_effects;
mod server;
