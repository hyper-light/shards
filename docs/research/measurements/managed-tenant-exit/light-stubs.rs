//! Compile-only stand-ins for host integration; none of these methods is executed.
#![allow(dead_code)]
use shards_abi::run::managed::{Channel, Event as CaptureEvent, Record, StreamState};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

mod common {
    use super::*;
    #[derive(Debug)]
    pub struct Run {
        pub status: Option<i32>,
        pub stdout: String,
        pub stderr: String,
    }
    impl std::fmt::Display for Run {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{self:?}")
        }
    }
    pub fn cannot_run_vms() -> bool {
        true
    }
    pub fn sha256_digest(_: &[u8]) -> String {
        panic!("compile-only stub")
    }
    pub fn shards() -> &'static Path {
        panic!("compile-only stub")
    }
    pub fn test_guest() -> &'static Path {
        panic!("compile-only stub")
    }
    pub fn kernel() -> &'static Path {
        panic!("compile-only stub")
    }
    pub fn guest_init() -> &'static Path {
        panic!("compile-only stub")
    }
}
enum Event {
    Record(u64, Vec<u8>),
    End,
}
enum Ask {
    Ack(u64),
    Cancel,
}
enum Command {
    Handler(Ask),
}
struct Sender;
impl Sender {
    fn send(&self, _: Command) -> Result<(), String> {
        panic!("compile-only stub")
    }
}
struct Scope {
    member: u32,
    channel: Channel,
}
struct Observer {
    scope: Scope,
    sender: Sender,
}
impl Observer {
    fn next(&self, _: Instant) -> Event {
        panic!("compile-only stub")
    }
    fn join(&mut self, _: Instant) -> Result<(), String> {
        panic!("compile-only stub")
    }
    fn ack_and_finish(&self, _: u64, _: Instant) {
        panic!("compile-only stub")
    }
}
struct Fixture {
    context: PathBuf,
    record: Option<PathBuf>,
    observers: Vec<Observer>,
    observed: BTreeMap<(u32, u8), (u64, u64)>,
    initial_fds: Option<usize>,
}
impl Fixture {
    fn new() -> Self {
        panic!("compile-only stub")
    }
    fn command(&self, _: &[&str], _: Duration) -> common::Run {
        panic!("compile-only stub")
    }
    fn start_with_options(&mut self, _: &[&str], _: &[&str], _: &str, _: Instant) {
        panic!("compile-only stub")
    }
    fn observe(&mut self, _: u32, _: Channel, _: Option<Scope>, _: bool, _: Instant) -> usize {
        panic!("compile-only stub")
    }
    fn main_progress(&self, _: Duration) {
        panic!("compile-only stub")
    }
    fn shutdown(&mut self, _: Instant) -> Result<(), String> {
        panic!("compile-only stub")
    }
    fn terminal(&self, _: u32, _: Channel) -> (u64, u64) {
        panic!("compile-only stub")
    }
}
include!("payload.rs");
#[path = "tenant_exit.rs"]
mod tenant_exit;

mod helper {
    #![allow(dead_code)]
    use std::io::{self, Read as _, Write as _};
    include!("helper.rs");
}
