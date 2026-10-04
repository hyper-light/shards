//! Apple's frameworks, bound when first used.
//!
//! A binary that links Security and CoreFoundation has dyld load and initialise both
//! every time it starts: 1.1 ms of a 2.1 ms launch of an empty program on an Apple M5 Max
//! (macOS 26.4.1, n = 400, PM M113). `shards` is one binary for every command, and most
//! never verify a certificate or make a bookmark, so it links neither: the functions it
//! calls are looked up with `dlsym` the first time one is wanted, from the frameworks'
//! fixed paths, and kept.
//!
//! - [`trust`]: a server's certificate chain evaluated by the system, as
//!   rustls-platform-verifier 0.7.1 evaluates it (src/verification/apple.rs): the same
//!   calls, in the same order, with the same arguments.
//! - [`bookmark`]: a directory's bookmark, for the VM process to resolve (D30).
//!
//! Elsewhere this crate is empty.

#[cfg(target_os = "macos")]
pub mod bookmark;
#[cfg(target_os = "macos")]
mod frameworks;
#[cfg(target_os = "macos")]
pub mod trust;
