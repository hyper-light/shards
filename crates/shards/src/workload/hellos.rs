use std::io::{self, Read, Write};
use std::sync::{Mutex, PoisonError, TryLockError};
use std::time::Instant;

use shards_abi::run::{self, kind};
use shards_vmm::{
    platform::UnixStream,
    vm::{Admission, ServedPort},
};

enum Listener {
    Dormant,
    Live { wake: UnixStream },
    Closed,
}
static STATE: Mutex<Listener> = Mutex::new(Listener::Dormant);

pub(super) fn admitting() -> bool {
    matches!(
        *STATE.lock().unwrap_or_else(PoisonError::into_inner),
        Listener::Live { .. }
    )
}

pub(super) fn close() {
    *STATE.lock().unwrap_or_else(PoisonError::into_inner) = Listener::Closed;
    *super::pending() = Vec::new();
}

pub struct Acceptor {
    woken: UnixStream,
}

pub fn exec_port() -> Result<(ServedPort, Acceptor), String> {
    let (wake, woken) = UnixStream::pair().map_err(|e| e.to_string())?;
    wake.set_nonblocking(true).map_err(|e| e.to_string())?;
    woken.set_nonblocking(true).map_err(|e| e.to_string())?;
    let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    if !matches!(*state, Listener::Dormant) {
        return Err("owned HELLO listener already initialized or closed".into());
    }
    *state = Listener::Live { wake };
    Ok((ServedPort::Admitted(admit), Acceptor { woken }))
}

fn admit(conn: UnixStream) -> Admission {
    let state = match STATE.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::WouldBlock) => return Admission::Full,
        Err(TryLockError::Poisoned(e)) => e.into_inner(),
    };
    let Listener::Live { wake } = &*state else {
        return Admission::Closed;
    };
    let mut waiting = match super::PENDING.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::WouldBlock) => return Admission::Full,
        Err(TryLockError::Poisoned(e)) => e.into_inner(),
    };
    if waiting.is_empty() {
        return Admission::Closed;
    }
    let Some(slot) = waiting.iter_mut().find(|w| w.incoming.is_none()) else {
        return Admission::Full;
    };
    slot.incoming = Some(conn);
    match (&*wake).write(&[1]) {
        Ok(1) => Admission::Accepted,
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Admission::Accepted,
        _ => {
            slot.incoming = None;
            Admission::Closed
        }
    }
}

struct Candidate {
    conn: UnixStream,
    hello: [u8; run::HEADER + run::TOKEN],
    got: usize,
    heard: Instant,
}

impl Candidate {
    fn read(&mut self) -> io::Result<bool> {
        let room = self
            .hello
            .get_mut(self.got..)
            .ok_or_else(|| io::Error::other("owned HELLO cursor"))?;
        let n = match (&self.conn).read(room) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => n,
            Err(e) if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {
                return Ok(false);
            }
            Err(e) => return Err(e),
        };
        self.got = self
            .got
            .checked_add(n)
            .ok_or_else(|| io::Error::other("owned HELLO cursor overflow"))?;
        self.heard = Instant::now();
        if self.got >= run::HEADER
            && self
                .hello
                .first_chunk::<{ run::HEADER }>()
                .copied()
                .and_then(run::parse_header)
                != Some((kind::HELLO, run::TOKEN as u32))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid owned HELLO header",
            ));
        }
        Ok(self.got == self.hello.len())
    }
}

fn read_candidates(active: &mut Vec<Candidate>) {
    for i in (0..active.len()).rev() {
        let Some(candidate) = active.get_mut(i) else {
            continue;
        };
        match candidate.read() {
            Ok(false) => {}
            Ok(true) => {
                let candidate = active.swap_remove(i);
                if let Some(token) = candidate.hello.last_chunk::<{ run::TOKEN }>() {
                    super::heard(candidate.conn, token);
                }
            }
            Err(_) => {
                active.swap_remove(i);
            }
        }
    }
}

impl Acceptor {
    pub fn run(self) {
        if let Err(e) = self.drive() {
            shards_vmm::debug!("owned HELLO listener closed: {e}");
        }
    }

    fn drive(&self) -> io::Result<()> {
        let mut active: Vec<Candidate> = Vec::new();
        loop {
            read_candidates(&mut active);
            loop {
                let (limit, incoming) = {
                    let mut waiting = super::pending();
                    let limit = waiting.len();
                    let incoming = if active.capacity() >= limit {
                        waiting.iter_mut().find_map(|w| w.incoming.take())
                    } else {
                        None
                    };
                    (limit, incoming)
                };
                if active.capacity() < limit {
                    active
                        .try_reserve_exact(limit.saturating_sub(active.len()))
                        .map_err(io::Error::other)?;
                    continue;
                }
                while active.len() > limit {
                    active.pop();
                }
                let Some(conn) = incoming else { break };
                if limit == 0 {
                    drop(conn);
                    continue;
                }
                if active.len() == limit {
                    read_candidates(&mut active);
                    if active.len() == limit {
                        let oldest = active
                            .iter()
                            .enumerate()
                            .min_by_key(|(_, c)| c.heard)
                            .map(|(i, _)| i)
                            .ok_or_else(|| io::Error::other("owned HELLO pressure without candidate"))?;
                        active.swap_remove(oldest);
                    }
                }
                active.push(Candidate {
                    conn,
                    hello: [0; run::HEADER + run::TOKEN],
                    got: 0,
                    heard: Instant::now(),
                });
            }
            super::await_hellos(&self.woken, active.iter().map(|c| &c.conn))?;
            let mut byte = [0];
            match (&self.woken).read(&mut byte) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(e) if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {}
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for Acceptor {
    fn drop(&mut self) {
        close();
    }
}
