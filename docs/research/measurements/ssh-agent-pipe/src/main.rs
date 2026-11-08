//! How long a dial of an SSH agent's named pipe waits for an instance on Windows (PM M493).
//!
//! OpenSSH for Windows' agent (PowerShell/openssh-portable v9.8.3.0,
//! contrib/win32/win32compat/ssh-agent/agent.c, `agent_listen_loop`) keeps one instance of
//! `\\.\pipe\openssh-ssh-agent` listening at a time: once a client connects, it starts a
//! worker process for that connection (CreateProcessW) and only then makes the next
//! instance. A client that dials meanwhile is told ERROR_PIPE_BUSY, and waits in
//! WaitNamedPipe for the next instance. A build dials the agent once for each connection a
//! step makes (`ssh::agent_stream`), steps at once among them.
//!
//! `ssh-agent-pipe [PIPE] [ROUNDS] [CLIENTS...]`: for each count of clients, that many
//! threads, started together, each dialing the pipe ROUNDS times as `ssh::pipe` dials it
//! (anonymous to the server), waiting for an instance as long as one takes
//! (NMPWAIT_WAIT_FOREVER, then dialed again: another client may take the instance first,
//! Microsoft Learn's "Named Pipe Client"), then asking the agent for its identities
//! (SSH_AGENTC_REQUEST_IDENTITIES, draft-ietf-sshm-ssh-agent) and reading the whole answer,
//! of which it prints nothing but its kind. Each dial's time from its first try to its open
//! handle, the busy dials' alone, each wait for an instance (the agent's interval between
//! instances, what a dial that follows its progress waits on), and each request's round trip, as n, p50, p90, p99 and
//! max; and how many busy dials waited longer than 2 s, the wait shards had (go-winio's
//! default).

use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, OPEN_EXISTING, ReadFile, SECURITY_ANONYMOUS, SECURITY_SQOS_PRESENT, WriteFile,
};
use windows_sys::Win32::System::Pipes::{NMPWAIT_WAIT_FOREVER, WaitNamedPipeW};

struct Dial {
    took: Duration,
    busy: u32,
    /// Each WaitNamedPipe's time, from its call to an instance free: how long the agent
    /// took to make its next instance, the interval at which it shows progress.
    waits: Vec<Duration>,
    round_trip: Duration,
    answered: bool,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn summary(v: &mut [Duration]) -> String {
    v.sort();
    let at = |q: f64| v[((v.len() as f64 - 1.0) * q).round() as usize];
    if v.is_empty() {
        return "n 0".into();
    }
    format!(
        "n {} p50 {:?} p90 {:?} p99 {:?} max {:?}",
        v.len(),
        at(0.5),
        at(0.9),
        at(0.99),
        v[v.len() - 1]
    )
}

fn write_all(h: HANDLE, mut b: &[u8]) -> bool {
    while !b.is_empty() {
        let mut n = 0u32;
        // SAFETY: a handle this thread opened, a buffer of `b`'s length.
        if unsafe { WriteFile(h, b.as_ptr(), b.len() as u32, &mut n, std::ptr::null_mut()) } == 0 || n == 0 {
            return false;
        }
        b = &b[n as usize..];
    }
    true
}

fn read_exact(h: HANDLE, b: &mut [u8]) -> bool {
    let mut at = 0;
    while at < b.len() {
        let mut n = 0u32;
        let rest = &mut b[at..];
        // SAFETY: as write_all.
        if unsafe { ReadFile(h, rest.as_mut_ptr(), rest.len() as u32, &mut n, std::ptr::null_mut()) } == 0 || n == 0
        {
            return false;
        }
        at += n as usize;
    }
    true
}

fn dial(name: &[u16]) -> Result<Dial, String> {
    let began = Instant::now();
    let mut busy = 0;
    let mut waits = Vec::new();
    let h = loop {
        // SAFETY: a NUL-terminated name; no security attributes, no template.
        let h = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS,
                std::ptr::null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE {
            break h;
        }
        // SAFETY: no arguments.
        let e = unsafe { GetLastError() };
        if e != ERROR_PIPE_BUSY {
            return Err(format!("CreateFileW: error {e}"));
        }
        busy += 1;
        let waited = Instant::now();
        // SAFETY: a NUL-terminated name.
        if unsafe { WaitNamedPipeW(name.as_ptr(), NMPWAIT_WAIT_FOREVER) } == 0 {
            // SAFETY: no arguments.
            return Err(format!("WaitNamedPipeW: error {}", unsafe { GetLastError() }));
        }
        waits.push(waited.elapsed());
    };
    let took = began.elapsed();
    let asked = Instant::now();
    // SSH_AGENTC_REQUEST_IDENTITIES, its length first.
    let mut answered = write_all(h, &[0, 0, 0, 1, 11]);
    let mut len = [0u8; 4];
    answered = answered && read_exact(h, &mut len);
    if answered {
        let mut body = vec![0u8; u32::from_be_bytes(len).min(1 << 20) as usize];
        // SSH_AGENT_IDENTITIES_ANSWER.
        answered = read_exact(h, &mut body) && body.first() == Some(&12);
    }
    let round_trip = asked.elapsed();
    // SAFETY: the handle opened above, closed once.
    unsafe { CloseHandle(h) };
    Ok(Dial {
        took,
        busy,
        waits,
        round_trip,
        answered,
    })
}

fn main() {
    let mut args = std::env::args().skip(1);
    let pipe = args.next().unwrap_or_else(|| r"\\.\pipe\openssh-ssh-agent".into());
    let rounds: usize = args.next().and_then(|r| r.parse().ok()).unwrap_or(200);
    let mut counts: Vec<usize> = args.filter_map(|c| c.parse().ok()).collect();
    if counts.is_empty() {
        counts = vec![1, 2, 4, 8, 16, 32];
    }
    println!("pipe {pipe}, {rounds} dials a client");
    for clients in counts {
        let barrier = Arc::new(Barrier::new(clients));
        let threads: Vec<_> = (0..clients)
            .map(|_| {
                let (barrier, name) = (barrier.clone(), wide(&pipe));
                std::thread::spawn(move || {
                    barrier.wait();
                    (0..rounds).map(|_| dial(&name)).collect::<Vec<_>>()
                })
            })
            .collect();
        let began = Instant::now();
        let mut dials = Vec::new();
        let mut failed = Vec::new();
        for t in threads {
            for d in t.join().expect("a client thread") {
                match d {
                    Ok(d) => dials.push(d),
                    Err(e) => failed.push(e),
                }
            }
        }
        let wall = began.elapsed();
        let mut all: Vec<Duration> = dials.iter().map(|d| d.took).collect();
        let mut busy: Vec<Duration> = dials.iter().filter(|d| d.busy > 0).map(|d| d.took).collect();
        let past_2s = busy.iter().filter(|d| **d > Duration::from_secs(2)).count();
        let most_busy = dials.iter().map(|d| d.busy).max().unwrap_or(0);
        let unanswered = dials.iter().filter(|d| !d.answered).count();
        let mut waits: Vec<Duration> = dials.iter().flat_map(|d| d.waits.iter().copied()).collect();
        let mut trips: Vec<Duration> = dials.iter().filter(|d| d.answered).map(|d| d.round_trip).collect();
        println!("clients {clients}: {} dials in {wall:?}, {} failed {:?}", dials.len(), failed.len(), failed.first());
        println!("  every dial: {}", summary(&mut all));
        println!(
            "  busy dials: {}; past 2 s {past_2s}; most busy answers in one dial {most_busy}",
            summary(&mut busy)
        );
        println!("  each wait for an instance: {}", summary(&mut waits));
        println!("  identities asked: {}; unanswered {unanswered}", summary(&mut trips));
    }
}
