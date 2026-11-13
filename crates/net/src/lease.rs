//! A run's credential leases, as its network process holds them (D242, kiln plan
//! 01a1d2b88be57304b972761e2a5c6781 step 4). A credential's value is asked of the run host
//! just in time, by reference, when a flow first needs it; the run host asks its orchestrator,
//! whose broker leases it. The value never passes through the VM process, and this process
//! holds it in memory only while its lease lives: nothing of it is kept past that (R05).
//!
//! Each credential has one lease at a time, in one of two states (R04): asked of the run
//! host, the flows that need it waiting; or held, until its end. Its ends: refused, its
//! waiting flows told why, typed; or released, at its end, at the run's end, or as this
//! process ends, its value wiped as it goes ([`Zeroizing`]). An ask never holds up the loop
//! that makes it: it goes out on the loop's next pass ([`Leases::asks`]), and its answer
//! comes as an event ([`Leases::answered`]) that names the flows to resume
//! ([`Leases::resumed`]).

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

/// Why a lease's value did not come (D242), as its run host said it: each cause typed, so
/// that its asker matches it and never its words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No grant of the run names it: refused before any broker is asked (default deny).
    Ungranted,
    /// A lease of it lives already: one lease of a credential at a time, as this process
    /// asks one.
    Live,
    /// The orchestrator's broker leases the run none of it, in its words: a credential its
    /// store does not hold, or one its policy keeps from the run.
    Denied(String),
    /// Asked under the key its run had before a move replaced it (D238, D176 part 6; the
    /// daemon's `Refused::KeyReplaced`): that key, and the run's key now where it has one,
    /// each 64 lowercase hex digits. The broker alone says which key is current.
    KeyReplaced { asked: String, current: Option<String> },
    /// Its broker could not be asked, or failed, in its words: a later ask may be leased.
    Unavailable(String),
    /// The run has ended, and its leases with it: nothing is leased after its release.
    Ended,
    /// Its lease ended before its value could be used: a later ask is leased anew.
    Lapsed,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ungranted => f.write_str("no grant of the run names it"),
            Self::Live => f.write_str("a lease of it lives already"),
            Self::Denied(why) => write!(f, "its broker leases the run none of it: {why}"),
            Self::KeyReplaced {
                asked,
                current: Some(current),
            } => write!(
                f,
                "asked under {asked}, the key its run had before it moved: its key is {current}"
            ),
            Self::KeyReplaced { asked, current: None } => write!(
                f,
                "asked under {asked}, the key its run had before it moved: it has no key here yet"
            ),
            Self::Unavailable(why) => write!(f, "its broker could not lease it: {why}"),
            Self::Ended => f.write_str("the run has ended"),
            Self::Lapsed => f.write_str("its lease ended before its value could be used"),
        }
    }
}

/// A flow waiting on a lease, by the token its asker resumes it by.
pub type Waiter = u64;

/// What an ask of a credential's value finds.
pub enum Asked<'a> {
    /// Its lease lives: its value, to fill in now.
    Held(&'a [u8]),
    /// Its lease is asked of the run host, and the flow waits for its answer
    /// ([`Leases::resumed`]).
    Waiting,
    /// No lease of it is asked, and why.
    Refused(Refusal),
}

/// Shows what an ask found, never a value.
impl std::fmt::Debug for Asked<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Asked::Held(_) => f.write_str("Held"),
            Asked::Waiting => f.write_str("Waiting"),
            Asked::Refused(refusal) => f.debug_tuple("Refused").field(refusal).finish(),
        }
    }
}

/// A waiting flow's lease, answered or ended: the flow, the credential, and whether its value
/// is held now ([`Leases::held`]) or why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resumed {
    pub waiter: Waiter,
    pub reference: String,
    pub outcome: Result<(), Refusal>,
}

/// The refusals among `resumed` of the asks flow `waiter` waited on, by reference: what its
/// resume carries to it, so that a refusal answers the asks it was for and is not asked again
/// (an answered ask's lease is gone from [`Leases`], held or not).
pub fn refused(resumed: &[Resumed], waiter: Waiter) -> Vec<(String, Refusal)> {
    let mut refused: Vec<(String, Refusal)> = Vec::new();
    for r in resumed.iter().filter(|r| r.waiter == waiter) {
        if let Err(why) = &r.outcome
            && !refused.iter().any(|(reference, _)| *reference == r.reference)
        {
            refused.push((r.reference.clone(), why.clone()));
        }
    }
    refused
}

/// One credential's lease, where it stands (R04).
enum Lease {
    /// Asked of the run host by `id`; the flows waiting on its answer.
    Asked { id: u32, waiting: Vec<Waiter> },
    /// Held until `until` on this process's clock; none where its end is past what that
    /// clock can name, which the run's end releases.
    Held {
        id: u32,
        value: Zeroizing<Vec<u8>>,
        until: Option<Instant>,
    },
}

impl Lease {
    fn id(&self) -> u32 {
        match self {
            Lease::Asked { id, .. } | Lease::Held { id, .. } => *id,
        }
    }

    fn lapsed(&self, now: Instant) -> bool {
        matches!(self, Lease::Held { until: Some(until), .. } if *until <= now)
    }
}

/// Shows where each lease stands, never its value.
impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Lease::Asked { id, waiting } => f
                .debug_struct("Asked")
                .field("id", id)
                .field("waiting", &waiting.len())
                .finish(),
            Lease::Held { id, until, .. } => f
                .debug_struct("Held")
                .field("id", id)
                .field("until", until)
                .finish(),
        }
    }
}

/// The run's leases (D242): its grants' references, which alone are asked, and each one's
/// lease, asked or held.
#[derive(Debug)]
pub struct Leases {
    granted: Vec<String>,
    /// Each credential's lease, by its reference: one at a time.
    each: HashMap<String, Lease>,
    /// The id the next ask takes, unless a live lease has it.
    next: u32,
    /// Asks made since the loop last sent them ([`asks`](Self::asks)).
    unsent: Vec<Vec<u8>>,
    /// Waiting flows whose lease was answered or ended, until their asker takes them.
    resumed: Vec<Resumed>,
    /// The run host can be asked no more: the run has ended.
    ended: bool,
}

impl Leases {
    /// The leases of a run whose grants name `granted`: none asked, none held.
    pub fn new(granted: Vec<String>) -> Leases {
        Leases {
            granted,
            each: HashMap::new(),
            next: 0,
            unsent: Vec::new(),
            resumed: Vec::new(),
            ended: false,
        }
    }

    /// Credential `reference`'s value for flow `waiter`, as it stands at `now`: held, its
    /// value; asked, the flow added to those waiting; neither, a lease asked of the run host
    /// (an ask its lease ended is released first, its value wiped); or refused: a reference
    /// no grant of the run names, or a run that has ended.
    pub fn ask(&mut self, reference: &str, waiter: Waiter, now: Instant) -> Asked<'_> {
        if self.ended {
            return Asked::Refused(Refusal::Ended);
        }
        if !self.granted.iter().any(|g| g == reference) {
            return Asked::Refused(Refusal::Ungranted);
        }
        if self.each.get(reference).is_some_and(|l| l.lapsed(now)) {
            self.each.remove(reference);
        }
        if !self.each.contains_key(reference) {
            let id = self.fresh();
            self.unsent.push(encode_ask(id, reference));
            self.each.insert(
                reference.to_string(),
                Lease::Asked {
                    id,
                    waiting: Vec::new(),
                },
            );
        }
        match self.each.get_mut(reference) {
            Some(Lease::Held { value, .. }) => Asked::Held(value.as_slice()),
            Some(Lease::Asked { waiting, .. }) => {
                waiting.push(waiter);
                Asked::Waiting
            }
            None => Asked::Refused(Refusal::Ended),
        }
    }

    /// An id no live lease has: at most one more try than there are leases.
    fn fresh(&mut self) -> u32 {
        loop {
            let id = self.next;
            self.next = self.next.wrapping_add(1);
            if self.each.values().all(|l| l.id() != id) {
                return id;
            }
        }
    }

    /// The asks made since the last call, each a `NET_LEASE` payload for the run host.
    pub fn asks(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.unsent)
    }

    /// The run host's answer `payload` (`NET_LEASE`), taken at `now`: its lease held until
    /// its end, or refused; either way its waiting flows resumed. False for a payload that is
    /// no answer, or one to no ask of this process's, which breaks the run host's contract.
    /// After the run's end, an answer is let go of: nothing waits on it.
    pub fn answered(&mut self, payload: &[u8], now: Instant) -> bool {
        let Some((id, answer)) = decode_answer(payload) else {
            return false;
        };
        if self.ended {
            return true;
        }
        let Some(reference) = self
            .each
            .iter()
            .find(|(_, l)| matches!(l, Lease::Asked { id: asked, .. } if *asked == id))
            .map(|(r, _)| r.clone())
        else {
            return false;
        };
        let Some(Lease::Asked { waiting, .. }) = self.each.remove(&reference) else {
            return false;
        };
        let outcome = match answer {
            Ok((lives, value)) => {
                let until = now.checked_add(Duration::from_nanos(lives.get()));
                self.each.insert(
                    reference.clone(),
                    Lease::Held {
                        id,
                        value: Zeroizing::new(value.to_vec()),
                        until,
                    },
                );
                Ok(())
            }
            Err(refusal) => Err(refusal),
        };
        self.resume(&reference, waiting, &outcome);
        true
    }

    fn resume(&mut self, reference: &str, waiting: Vec<Waiter>, outcome: &Result<(), Refusal>) {
        self.resumed.extend(waiting.into_iter().map(|waiter| Resumed {
            waiter,
            reference: reference.to_string(),
            outcome: outcome.clone(),
        }));
    }

    /// The flows whose lease was answered or ended since the last call.
    pub fn resumed(&mut self) -> Vec<Resumed> {
        std::mem::take(&mut self.resumed)
    }

    /// Credential `reference`'s value, where its lease is held at `now`.
    pub fn held(&self, reference: &str, now: Instant) -> Option<&[u8]> {
        match self.each.get(reference) {
            Some(l @ Lease::Held { value, .. }) if !l.lapsed(now) => Some(value.as_slice()),
            _ => None,
        }
    }

    /// Releases each lease whose end is `now` or past, its value wiped; how many.
    pub fn lapse(&mut self, now: Instant) -> usize {
        let before = self.each.len();
        self.each.retain(|_, l| !l.lapsed(now));
        before.saturating_sub(self.each.len())
    }

    /// A credential the run's grants name from now on: a sign-in's access token, as its capture
    /// gave its reference (D242, step 8).
    pub fn grant(&mut self, reference: &str) {
        if !self.granted.iter().any(|g| g == reference) {
            self.granted.push(reference.to_string());
        }
    }

    /// Credential `reference`'s held lease released, its value wiped, as its value rotated
    /// outside (a sign-in's refresh): the next flow that needs it asks anew. An ask under way
    /// stays, its answer the broker's word on the value now.
    pub fn forget(&mut self, reference: &str) {
        if matches!(self.each.get(reference), Some(Lease::Held { .. })) {
            self.each.remove(reference);
        }
    }

    /// The end of the lease that ends first, for the loop's timers.
    pub fn next_end(&self) -> Option<Instant> {
        self.each
            .values()
            .filter_map(|l| match l {
                Lease::Held { until, .. } => *until,
                Lease::Asked { .. } => None,
            })
            .min()
    }

    /// The run's end, or its host's: every lease released, each value wiped, each waiting
    /// flow told the run has ended, and nothing asked after.
    pub fn end(&mut self) {
        self.ended = true;
        self.unsent.clear();
        for (reference, lease) in std::mem::take(&mut self.each) {
            if let Lease::Asked { waiting, .. } = lease {
                self.resume(&reference, waiting, &Err(Refusal::Ended));
            }
        }
    }
}

/// `NET_LEASE`'s ask: its id (4 bytes, big-endian), then the credential's reference.
pub fn encode_ask(id: u32, reference: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + reference.len());
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(reference.as_bytes());
    out
}

/// What [`encode_ask`] wrote; none for a payload cut short or a reference that is not UTF-8.
pub fn decode_ask(payload: &[u8]) -> Option<(u32, &str)> {
    let (id, reference) = payload.split_first_chunk::<4>()?;
    Some((u32::from_be_bytes(*id), std::str::from_utf8(reference).ok()?))
}

/// `NET_LEASE`'s answer, by its ask's id (4 bytes, big-endian): 0, how long its lease lives
/// past the answer in nanoseconds (8 bytes, never 0) and the value, last, so that the
/// buffer holding it is made once; or a refusal's tag and its fields, each text a 4-byte
/// length and its UTF-8, a key that may be none 0, or 1 and the key. The buffer is wiped as
/// it goes. None where a text passes what a 4-byte length counts.
pub fn encode_answer(id: u32, answer: Result<(NonZeroU64, &[u8]), &Refusal>) -> Option<Zeroizing<Vec<u8>>> {
    let value_len = answer.map_or(0, |(_, value)| value.len());
    let mut out = Zeroizing::new(Vec::with_capacity(value_len.saturating_add(4 + 1 + 8)));
    out.extend_from_slice(&id.to_be_bytes());
    match answer {
        Ok((lives, value)) => {
            out.push(0);
            out.extend_from_slice(&lives.get().to_be_bytes());
            out.extend_from_slice(value);
        }
        Err(Refusal::Ungranted) => out.push(1),
        Err(Refusal::Live) => out.push(2),
        Err(Refusal::Denied(why)) => {
            out.push(3);
            put_text(&mut out, why)?;
        }
        Err(Refusal::KeyReplaced { asked, current }) => {
            out.push(4);
            put_text(&mut out, asked)?;
            match current {
                None => out.push(0),
                Some(current) => {
                    out.push(1);
                    put_text(&mut out, current)?;
                }
            }
        }
        Err(Refusal::Unavailable(why)) => {
            out.push(5);
            put_text(&mut out, why)?;
        }
        Err(Refusal::Ended) => out.push(6),
        Err(Refusal::Lapsed) => out.push(7),
    }
    Some(out)
}

fn put_text(out: &mut Vec<u8>, s: &str) -> Option<()> {
    out.extend_from_slice(&u32::try_from(s.len()).ok()?.to_be_bytes());
    out.extend_from_slice(s.as_bytes());
    Some(())
}

fn take_text(rest: &mut &[u8]) -> Option<String> {
    let (len, after) = rest.split_first_chunk::<4>()?;
    let (s, after) = after.split_at_checked(usize::try_from(u32::from_be_bytes(*len)).ok()?)?;
    *rest = after;
    String::from_utf8(s.to_vec()).ok()
}

/// A lease's answer as [`decode_answer`] reads it: its ask's id, and the lease's life and
/// value, or its refusal.
pub type Answer<'a> = (u32, Result<(NonZeroU64, &'a [u8]), Refusal>);

/// What [`encode_answer`] wrote, the value borrowed from `payload`. None for a payload cut
/// short or padded, an unknown tag, a life of none, or a text that is not UTF-8.
pub fn decode_answer(payload: &[u8]) -> Option<Answer<'_>> {
    let (id, rest) = payload.split_first_chunk::<4>()?;
    let id = u32::from_be_bytes(*id);
    let (&tag, mut rest) = rest.split_first()?;
    let answer = match tag {
        0 => {
            let (lives, value) = rest.split_first_chunk::<8>()?;
            let lives = NonZeroU64::new(u64::from_be_bytes(*lives))?;
            return Some((id, Ok((lives, value))));
        }
        1 => Refusal::Ungranted,
        2 => Refusal::Live,
        3 => Refusal::Denied(take_text(&mut rest)?),
        4 => {
            let asked = take_text(&mut rest)?;
            let (&flag, after) = rest.split_first()?;
            rest = after;
            let current = match flag {
                0 => None,
                1 => Some(take_text(&mut rest)?),
                _ => return None,
            };
            Refusal::KeyReplaced { asked, current }
        }
        5 => Refusal::Unavailable(take_text(&mut rest)?),
        6 => Refusal::Ended,
        7 => Refusal::Lapsed,
        _ => return None,
    };
    rest.is_empty().then_some((id, Err(answer)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const CLAUDE: &str = "anthropic/claude-max";
    const VALUE: &[u8] = b"sk-ant-lease-test-value";

    fn leases() -> Leases {
        Leases::new(vec![CLAUDE.into(), "openai/chatgpt".into()])
    }

    fn lives(d: Duration) -> NonZeroU64 {
        NonZeroU64::new(u64::try_from(d.as_nanos()).unwrap()).unwrap()
    }

    /// The id of the one ask `asks` holds, for `reference`.
    fn the_ask(asks: &[Vec<u8>], reference: &str) -> u32 {
        assert_eq!(asks.len(), 1, "{} asks", asks.len());
        let (id, asked) = decode_ask(&asks[0]).unwrap();
        assert_eq!(asked, reference);
        id
    }

    /// Do: ask a granted credential for two flows, answer it, then ask it again before and
    /// after its end. Expect: no value before its first use and nothing asked before it; one
    /// ask for both flows (one lease at a time); both resumed once it is answered, its value
    /// then held; and past its end, released, nothing held, its next use asked anew.
    #[test]
    fn a_value_is_asked_on_first_use_and_held_while_its_lease_lives() {
        let mut l = leases();
        let now = Instant::now();
        assert_eq!(l.held(CLAUDE, now), None, "a value held before its first use");
        assert!(l.asks().is_empty(), "asked before its first use");
        assert!(matches!(l.ask(CLAUDE, 1, now), Asked::Waiting));
        assert!(matches!(l.ask(CLAUDE, 2, now), Asked::Waiting));
        let id = the_ask(&l.asks(), CLAUDE);
        assert!(l.asks().is_empty(), "an ask sent twice");
        let life = Duration::from_secs(60);
        let answer = encode_answer(id, Ok((lives(life), VALUE))).unwrap();
        assert!(l.answered(&answer, now));
        let resumed = l.resumed();
        assert_eq!(resumed.iter().map(|r| r.waiter).collect::<Vec<_>>(), [1, 2]);
        assert!(
            resumed
                .iter()
                .all(|r| r.reference == CLAUDE && r.outcome == Ok(()))
        );
        assert_eq!(l.held(CLAUDE, now), Some(VALUE));
        assert!(matches!(l.ask(CLAUDE, 3, now), Asked::Held(v) if v == VALUE));
        assert!(l.asks().is_empty(), "a held lease asked again");
        assert_eq!(l.next_end(), Some(now + life));
        // Its end: released, its value wiped, and its next use asks anew.
        let end = now + life;
        assert_eq!(l.lapse(end - Duration::from_nanos(1)), 0);
        assert_eq!(l.lapse(end), 1);
        assert_eq!(l.held(CLAUDE, end), None, "a value held past its lease");
        assert_eq!(l.next_end(), None);
        assert!(matches!(l.ask(CLAUDE, 4, end), Asked::Waiting));
        assert_ne!(the_ask(&l.asks(), CLAUDE), id, "a new lease, a new ask");
    }

    /// Do: ask a credential no grant names, of a run with grants and of one with none.
    /// Expect: refused here, nothing asked of the run host (default deny).
    #[test]
    fn a_run_with_no_grant_asks_nothing() {
        let now = Instant::now();
        for mut l in [leases(), Leases::new(Vec::new())] {
            assert!(matches!(
                l.ask("github/token", 1, now),
                Asked::Refused(Refusal::Ungranted)
            ));
            assert!(l.asks().is_empty());
            assert!(l.resumed().is_empty());
        }
    }

    /// Do: refuse an ask, typed, each way the run host refuses one. Expect: its flows told
    /// that refusal, nothing held, and the next use asked anew.
    #[test]
    fn a_refusal_reaches_its_flows_typed_and_holds_nothing() {
        let now = Instant::now();
        for refusal in [
            Refusal::Ungranted,
            Refusal::Live,
            Refusal::Denied("no such secret".into()),
            Refusal::KeyReplaced {
                asked: "a".repeat(64),
                current: Some("b".repeat(64)),
            },
            Refusal::KeyReplaced {
                asked: "a".repeat(64),
                current: None,
            },
            Refusal::Unavailable("its store is down".into()),
            Refusal::Ended,
            Refusal::Lapsed,
        ] {
            let mut l = leases();
            assert!(matches!(l.ask(CLAUDE, 7, now), Asked::Waiting));
            let id = the_ask(&l.asks(), CLAUDE);
            assert!(l.answered(&encode_answer(id, Err(&refusal)).unwrap(), now));
            assert_eq!(
                l.resumed(),
                [Resumed {
                    waiter: 7,
                    reference: CLAUDE.into(),
                    outcome: Err(refusal.clone())
                }]
            );
            assert_eq!(l.held(CLAUDE, now), None);
            assert!(matches!(l.ask(CLAUDE, 8, now), Asked::Waiting), "{refusal:?}");
        }
    }

    /// Do: two flows wait on a lease that is refused, one of them twice (two HTTP/2 streams),
    /// and on another that is held. Expect: each flow's resume carries the refusal once, the
    /// held one carries none, and a flow that waited on nothing refused carries none.
    #[test]
    fn a_resume_carries_each_flows_refusals_to_it() {
        let now = Instant::now();
        let mut l = Leases::new(vec![CLAUDE.into(), "other".into()]);
        for waiter in [7, 7, 8] {
            assert!(matches!(l.ask(CLAUDE, waiter, now), Asked::Waiting));
        }
        assert!(matches!(l.ask("other", 9, now), Asked::Waiting));
        let asks = l.asks();
        assert_eq!(asks.len(), 2, "one ask a credential");
        let id = |reference: &str| {
            let ask = asks.iter().find(|a| decode_ask(a).unwrap().1 == reference);
            decode_ask(ask.unwrap()).unwrap().0
        };
        let why = Refusal::Denied("its policy keeps it".into());
        assert!(l.answered(&encode_answer(id(CLAUDE), Err(&why)).unwrap(), now));
        let life = NonZeroU64::new(60_000_000_000).unwrap();
        assert!(l.answered(&encode_answer(id("other"), Ok((life, b"v"))).unwrap(), now));
        let resumed = l.resumed();
        assert_eq!(resumed.len(), 4);
        for waiter in [7, 8] {
            assert_eq!(refused(&resumed, waiter), [(CLAUDE.to_string(), why.clone())]);
        }
        assert_eq!(refused(&resumed, 9), []);
        assert_eq!(refused(&resumed, 10), []);
    }

    /// Do: end the run with one lease held and one asked, then answer the asked one and ask
    /// again. Expect: both released, nothing held, the waiting flow told the run ended, the
    /// late answer let go of, and every later ask refused, nothing asked.
    #[test]
    fn the_runs_end_releases_every_lease() {
        let now = Instant::now();
        let mut l = leases();
        assert!(matches!(l.ask(CLAUDE, 1, now), Asked::Waiting));
        let held = the_ask(&l.asks(), CLAUDE);
        let answer = encode_answer(held, Ok((lives(Duration::from_secs(60)), VALUE))).unwrap();
        assert!(l.answered(&answer, now));
        l.resumed();
        assert!(matches!(l.ask("openai/chatgpt", 2, now), Asked::Waiting));
        let asked = the_ask(&l.asks(), "openai/chatgpt");
        l.end();
        assert_eq!(l.held(CLAUDE, now), None, "a value held past the run's end");
        assert_eq!(
            l.resumed(),
            [Resumed {
                waiter: 2,
                reference: "openai/chatgpt".into(),
                outcome: Err(Refusal::Ended)
            }]
        );
        let late = encode_answer(asked, Ok((lives(Duration::from_secs(60)), VALUE))).unwrap();
        assert!(l.answered(&late, now), "a late answer breaks nothing");
        assert_eq!(l.held("openai/chatgpt", now), None);
        assert!(matches!(l.ask(CLAUDE, 3, now), Asked::Refused(Refusal::Ended)));
        assert!(l.asks().is_empty());
    }

    /// An answer to no ask of this process's, or one that is no answer, is refused: the run
    /// host broke its contract.
    #[test]
    fn an_answer_to_no_ask_is_refused() {
        let now = Instant::now();
        let mut l = leases();
        let stray = encode_answer(9, Ok((lives(Duration::from_secs(1)), VALUE))).unwrap();
        assert!(!l.answered(&stray, now));
        assert!(matches!(l.ask(CLAUDE, 1, now), Asked::Waiting));
        let id = the_ask(&l.asks(), CLAUDE);
        assert!(!l.answered(&[0, 0], now));
        assert!(!l.answered(
            &encode_answer(id.wrapping_add(1), Err(&Refusal::Ended)).unwrap(),
            now
        ));
        assert_eq!(l.held(CLAUDE, now), None);
    }

    /// An end past what this clock can name is held until the run's end, never refused nor
    /// wrapped.
    #[test]
    fn a_lease_past_the_clocks_reach_lives_until_the_runs_end() {
        let now = Instant::now();
        let mut l = leases();
        assert!(matches!(l.ask(CLAUDE, 1, now), Asked::Waiting));
        let id = the_ask(&l.asks(), CLAUDE);
        let answer = encode_answer(id, Ok((NonZeroU64::MAX, VALUE))).unwrap();
        assert!(l.answered(&answer, now));
        assert_eq!(l.held(CLAUDE, now + Duration::from_secs(86_400)), Some(VALUE));
        l.end();
        assert_eq!(l.held(CLAUDE, now), None);
    }

    /// Every answer reads back as written; one cut short, padded, of an unknown tag or of a
    /// life of none is refused. An ask reads back as written.
    #[test]
    fn answers_and_asks_round_trip_and_bad_ones_are_refused() {
        let life = NonZeroU64::new(3_600_000_000_000).unwrap();
        let held = encode_answer(5, Ok((life, VALUE))).unwrap();
        assert_eq!(decode_answer(&held), Some((5, Ok((life, VALUE)))));
        assert_eq!(
            decode_answer(&encode_answer(5, Ok((life, b""))).unwrap()),
            Some((5, Ok((life, &b""[..]))))
        );
        for refusal in [
            Refusal::Ungranted,
            Refusal::Live,
            Refusal::Denied(String::new()),
            Refusal::Denied("policy".into()),
            Refusal::KeyReplaced {
                asked: "0".repeat(64),
                current: Some("f".repeat(64)),
            },
            Refusal::KeyReplaced {
                asked: "0".repeat(64),
                current: None,
            },
            Refusal::Unavailable("down".into()),
            Refusal::Ended,
            Refusal::Lapsed,
        ] {
            let bytes = encode_answer(u32::MAX, Err(&refusal)).unwrap();
            assert_eq!(decode_answer(&bytes), Some((u32::MAX, Err(refusal.clone()))));
            assert_eq!(
                decode_answer(&[bytes.as_slice(), &[0]].concat()),
                None,
                "{refusal:?} padded"
            );
            assert_eq!(decode_answer(&bytes[..bytes.len() - 1]), None, "{refusal:?} cut");
        }
        let mut unknown = encode_answer(1, Err(&Refusal::Ended)).unwrap();
        unknown[4] = 8;
        assert_eq!(decode_answer(&unknown), None);
        let mut flag = encode_answer(
            1,
            Err(&Refusal::KeyReplaced {
                asked: "a".into(),
                current: None,
            }),
        )
        .unwrap();
        *flag.last_mut().unwrap() = 2;
        assert_eq!(decode_answer(&flag), None);
        let mut none = encode_answer(1, Ok((life, VALUE))).unwrap();
        none[5..13].fill(0);
        assert_eq!(decode_answer(&none), None, "a lease that lives no time");
        assert_eq!(decode_answer(&held[..12]), None);
        assert_eq!(decode_ask(&encode_ask(7, CLAUDE)), Some((7, CLAUDE)));
        assert_eq!(decode_ask(&[0, 0, 0]), None);
        assert_eq!(decode_ask(&[0, 0, 0, 1, 0xff]), None);
    }

    /// No value reaches a lease's `Debug`, nor a refusal's words.
    #[test]
    fn a_value_never_prints() {
        let now = Instant::now();
        let mut l = leases();
        assert!(matches!(l.ask(CLAUDE, 1, now), Asked::Waiting));
        let id = the_ask(&l.asks(), CLAUDE);
        let answer = encode_answer(id, Ok((lives(Duration::from_secs(60)), VALUE))).unwrap();
        assert!(l.answered(&answer, now));
        let shown = format!("{l:?}");
        let value = std::str::from_utf8(VALUE).unwrap();
        assert!(!shown.contains(value), "{shown}");
        assert!(shown.contains(CLAUDE), "{shown}");
        assert!(!shown.contains(&format!("{:?}", VALUE)), "{shown}");
    }

    #[test]
    fn a_captured_credential_is_granted_and_a_rotated_ones_held_lease_let_go() {
        let access = "sign-ins/c/access";
        let mut leases = Leases::new(Vec::new());
        let now = Instant::now();
        assert!(matches!(
            leases.ask(access, 1, now),
            Asked::Refused(Refusal::Ungranted)
        ));
        leases.grant(access);
        assert!(matches!(leases.ask(access, 1, now), Asked::Waiting));
        let (id, _) = decode_ask(&leases.asks()[0]).unwrap();
        let life = NonZeroU64::new(60_000_000_000).unwrap();
        assert!(leases.answered(&encode_answer(id, Ok((life, b"old"))).unwrap(), now));
        assert_eq!(leases.held(access, now), Some(&b"old"[..]));
        leases.forget(access);
        assert_eq!(leases.held(access, now), None, "a rotated value held still");
        assert!(matches!(leases.ask(access, 2, now), Asked::Waiting));
        leases.forget(access);
        assert!(
            matches!(leases.ask(access, 3, now), Asked::Waiting),
            "an ask under way was let go"
        );
        assert_eq!(leases.asks().len(), 1, "the rotated value asked twice");
    }
}
