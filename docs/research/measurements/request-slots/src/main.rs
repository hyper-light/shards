//! What a request costs through a slot of `shards_ipc::slots`, as its client sees it, and
//! what making and removing one costs its daemon (PM M348). M254 and M255 measured Unix's
//! FIFOs in C; this measures shards' own slots, on every host: on Windows an instance of a
//! named pipe, elsewhere FIFOs (D185, D195).
//!
//! - `slot`: a client takes a slot (`slots::take`), asks a request of 64 bytes and reads
//!   its answer, a byte: timed from the take to the answer, as the client sees it. The
//!   daemon keeps two slots offered, a thread waiting on each, and offers another as each
//!   is taken, as it does (`slots::OFFERED`); the client asks every PACE, so that no ask
//!   waits for a slot to be made.
//! - `pair`: the same request and answer over two lines held open (`slots::joined`), as a
//!   slot's `say` and `answer` are once taken: the floor.
//! - `make`, `make, streams`: a slot made and removed on the daemon's thread, offered
//!   nowhere, without its streams and with them.
//!
//! Each runs alone, in blocks of N/2, in two rounds of opposite order, after WARM rounds
//! of each not counted.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use shards_ipc::slots::{self, Requests, Taken, Waited};

const WARM: usize = 100;
const PACE: Duration = Duration::from_millis(2);
const REQUEST: [u8; 64] = [7; 64];
const PATIENCE: Duration = Duration::from_secs(10);

fn other(what: &str) -> io::Error {
    io::Error::other(what.to_string())
}

/// Makes a slot and offers it, and has a thread of `scope`'s wait on it, answer its
/// request, and say its number once its client has gone.
fn offer<'s>(
    scope: &'s std::thread::Scope<'s, '_>,
    requests: &mut Requests,
    served: &mpsc::Sender<io::Result<u64>>,
) -> io::Result<()> {
    let slot = requests.make(false)?;
    let n = slot.number();
    requests.offer(n, false)?;
    let served = served.clone();
    std::thread::Builder::new().spawn_scoped(scope, move || {
        let answered = (|| {
            let Waited::Opened(opened) = slot.wait()? else {
                return Err(other("no client opened the slot"));
            };
            let asked = opened.request()?.ok_or_else(|| other("no whole request"))?;
            asked.session.send(1, &[0])?;
            while asked.session.recv()?.is_some() {}
            Ok(n)
        })();
        let _ = served.send(answered);
    })?;
    Ok(())
}

/// Serves `count` requests on `home`'s slots while `ask` asks them.
fn slot(requests: &mut Requests, home: &Path, count: usize, samples: &mut Vec<Duration>) -> io::Result<()> {
    std::thread::scope(|s| {
        let (served, done) = mpsc::channel();
        let mut offered = 0;
        while offered < count.min(slots::OFFERED) {
            offer(s, requests, &served)?;
            offered += 1;
        }
        let asking = std::thread::Builder::new().spawn_scoped(s, || -> io::Result<Vec<Duration>> {
            let mut took = Vec::with_capacity(count);
            for _ in 0..count {
                std::thread::sleep(PACE);
                let t0 = Instant::now();
                let Taken::Slot(client) = slots::take(home, 0)? else {
                    return Err(other("no daemon"));
                };
                client.ask(7, &REQUEST, &[])?;
                let answer = client.recv()?;
                took.push(t0.elapsed());
                answer.ok_or_else(|| other("no answer"))?;
            }
            Ok(took)
        })?;
        for _ in 0..count {
            let n = done.recv_timeout(PATIENCE).map_err(|e| other(&e.to_string()))??;
            requests.remove(n);
            if offered < count {
                offer(s, requests, &served)?;
                offered += 1;
            }
        }
        samples.extend(asking.join().map_err(|_| other("the client panicked"))??);
        Ok(())
    })
}

/// `count` requests and answers over two lines held open.
fn pair(count: usize, samples: &mut Vec<Duration>) -> io::Result<()> {
    let (say_read, say_write) = slots::joined()?;
    let (answer_read, answer_write) = slots::joined()?;
    std::thread::scope(|s| {
        let echo = std::thread::Builder::new().spawn_scoped(s, move || -> io::Result<()> {
            while slots::read_frame(&say_read)?.is_some() {
                slots::write_frame(&answer_write, 1, &[0])?;
            }
            Ok(())
        })?;
        for _ in 0..count {
            std::thread::sleep(PACE);
            let t0 = Instant::now();
            slots::write_frame(&say_write, 7, &REQUEST)?;
            let answer = slots::read_frame(&answer_read)?;
            samples.push(t0.elapsed());
            answer.ok_or_else(|| other("no answer"))?;
        }
        drop(say_write);
        echo.join().map_err(|_| other("the echo panicked"))?
    })
}

/// `count` slots made and removed, `streams` or not.
fn make(requests: &mut Requests, streams: bool, count: usize, samples: &mut Vec<Duration>) -> io::Result<()> {
    for _ in 0..count {
        let t0 = Instant::now();
        let slot = requests.make(streams)?;
        requests.remove(slot.number());
        drop(slot);
        samples.push(t0.elapsed());
    }
    Ok(())
}

struct Way {
    name: &'static str,
    samples: Vec<Duration>,
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(args.get(1).map_or("request-slots", String::as_str));
    let n: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(2000);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let mut requests = Requests::open(&dir)?;
    let mut ways: Vec<Way> = ["slot", "pair", "make", "make, streams"]
        .into_iter()
        .map(|name| Way {
            name,
            samples: Vec::with_capacity(n),
        })
        .collect();
    let run = |requests: &mut Requests, way: usize, count: usize, samples: &mut Vec<Duration>| match way {
        0 => slot(requests, &dir, count, samples),
        1 => pair(count, samples),
        2 => make(requests, false, count, samples),
        _ => make(requests, true, count, samples),
    };
    for way in 0..ways.len() {
        run(&mut requests, way, WARM, &mut Vec::new())?;
    }
    let half = n / 2;
    for round in 0..2 {
        let mut order: Vec<usize> = (0..ways.len()).collect();
        if round == 1 {
            order.reverse();
        }
        for way in order {
            let mut samples = std::mem::take(&mut ways[way].samples);
            run(&mut requests, way, half, &mut samples)?;
            ways[way].samples = samples;
        }
    }
    println!(
        "host: {} {}, dir: {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        dir.display()
    );
    println!("| way | n | p50 | p90 | p99 | max |");
    println!("|---|---|---|---|---|---|");
    for w in &mut ways {
        w.samples.sort();
        let at = |q: f64| {
            let i = ((w.samples.len().saturating_sub(1)) as f64 * q).round() as usize;
            w.samples.get(i).copied().unwrap_or_default()
        };
        let us = |d: Duration| format!("{:.1}", d.as_secs_f64() * 1e6);
        println!(
            "| {} | {} | {} | {} | {} | {} |",
            w.name,
            w.samples.len(),
            us(at(0.5)),
            us(at(0.9)),
            us(at(0.99)),
            us(w.samples.last().copied().unwrap_or_default())
        );
    }
    drop(requests);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
