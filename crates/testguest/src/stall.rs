//! The storm's watch over its main thread after a snapshot (tests/snapshot.rs): what a
//! watcher makes of each look at the main thread's count and the host's stream.

/// A count a watcher looks at once a look: how many looks it has stood still for since it
/// last moved, and the most it stood still for before it moved on.
#[derive(Clone, Copy, Debug, Default)]
struct Count {
    last: u64,
    still: u64,
    longest: Option<u64>,
}

impl Count {
    /// Takes a look at `now`; whether the count moved since the last.
    fn look(&mut self, now: u64) -> bool {
        if now != self.last {
            self.longest = Some(self.longest.map_or(self.still, |l| l.max(self.still)));
            (self.last, self.still) = (now, 0);
            return true;
        }
        self.still = self.still.saturating_add(1);
        false
    }
}

/// What a look found of the main thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Its count moved since the last look.
    Moved,
    /// It stands still, for no more looks than it ever did before it moved on.
    Within,
    /// It stands still for more looks than it ever did before: said once a beat.
    PastItsPace,
    /// Its stand-still is a stall: the kernel's account of it is due.
    Stalled,
}

/// One watcher's view.
#[derive(Debug)]
pub struct Watch {
    main: Count,
    echo: Count,
    quiet: u64,
}

impl Watch {
    /// A watch from a look at the main thread's `beat` and the echo's `echoed` bytes, with
    /// the host's quiet in looks.
    pub fn new(beat: u64, echoed: u64, quiet: u64) -> Watch {
        Watch {
            main: Count {
                last: beat,
                ..Count::default()
            },
            echo: Count {
                last: echoed,
                ..Count::default()
            },
            quiet,
        }
    }

    /// A look at the main thread's `beat` and the echo's `echoed` bytes.
    pub fn look(&mut self, beat: u64, echoed: u64) -> Verdict {
        self.echo.look(echoed);
        if self.main.look(beat) {
            return Verdict::Moved;
        }
        if self.main.still >= self.quiet {
            return Verdict::Stalled;
        }
        if self.main.still > self.main.longest.unwrap_or(0) {
            return Verdict::PastItsPace;
        }
        Verdict::Within
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    /// The echo's bytes at each look: one more each `every` looks, none for 0.
    fn echo(every: u64, looks: u64, echoed: &mut u64) {
        if every != 0 && looks.is_multiple_of(every) {
            *echoed += 1;
        }
    }

    /// The verdicts of a main thread's last stand-still of `last` looks, after it moved on
    /// from each of `stands` (its stand-stills, in looks), while the echo carried a byte
    /// each `every` looks before the last stand and each `then` looks during it.
    fn last_stand(stands: &[u64], every: u64, last: u64, then: u64) -> Vec<Verdict> {
        let (mut beat, mut echoed, mut looks) = (0u64, 0u64, 0u64);
        let mut watch = Watch::new(beat, echoed, 60);
        for &stand in stands {
            for _ in 0..stand {
                looks += 1;
                echo(every, looks, &mut echoed);
                watch.look(beat, echoed);
            }
            looks += 1;
            echo(every, looks, &mut echoed);
            beat += 1;
            assert_eq!(watch.look(beat, echoed), Verdict::Moved);
        }
        (0..last)
            .map(|_| {
                looks += 1;
                echo(then, looks, &mut echoed);
                watch.look(beat, echoed)
            })
            .collect()
    }

    /// PM M427's shape: a main thread that stood 20 looks at its clock check before stands
    /// 56 and more, on a host slow enough that each step takes seconds, while the echo
    /// carries the host's stream at each look. Its stand is past its pace, said, but no
    /// stall: the kernel's account (some 6,000 lines from a 64-vCPU guest, at milliseconds
    /// a byte) would hold CPU 0, which the echo's interrupts go to.
    #[test]
    fn a_slow_main_thread_while_the_stream_carries_is_no_stall() {
        let verdicts = last_stand(&[1, 20, 3], 1, 90, 1);
        assert_eq!(verdicts.get(..20), Some(&[Verdict::Within; 20][..]));
        assert!(verdicts.get(20..).is_some_and(|v| v.iter().all(|&v| v == Verdict::PastItsPace)));
    }

    /// A main thread past its pace, and a stream that has gone without bytes for longer
    /// than it ever did while it carried: a stall, at the first look where both hold.
    #[test]
    fn a_stall_is_both_past_their_pace() {
        // The main thread stood 3 looks at most; the echo moved each 2, still 1 look at
        // most between its bytes. While it goes on so, the stand is past its pace alone.
        let verdicts = last_stand(&[3, 1, 2], 2, 10, 2);
        assert_eq!(verdicts.get(..3), Some(&[Verdict::Within; 3][..]));
        assert!(verdicts.get(3..).is_some_and(|v| v.iter().all(|&v| v == Verdict::PastItsPace)));
        // The same stand with the stream stopped at its start.
        let verdicts = last_stand(&[3, 1, 2], 2, 10, 0);
        assert_eq!(verdicts.get(..3), Some(&[Verdict::Within; 3][..]));
        assert_eq!(verdicts.get(3), Some(&Verdict::Stalled));
    }

    /// Before the echo has carried a byte since the watch began (a restore whose host has
    /// not streamed again yet), its quiet has no pace to be past: no stall, however long
    /// the main thread stands. The host, which reads its stream byte by byte, says where
    /// it stopped.
    #[test]
    fn a_stream_not_yet_carried_makes_no_stall() {
        let verdicts = last_stand(&[3], 0, 200, 0);
        assert!(verdicts.get(3..).is_some_and(|v| v.iter().all(|&v| v == Verdict::PastItsPace)));
    }
}
