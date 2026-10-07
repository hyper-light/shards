//! The relay's frames and the bytes it has yet to write (audit D07). Frames are parsed
//! from a cursor, and what they took is removed once a batch; bytes are written from
//! where the last write stopped, and moved to the front only once what was written is
//! at least half of what is held. So a buffer is moved O(n) in all, where removing each
//! frame's or write's prefix moved its whole remainder every time.

use shards_abi::run;

/// Calls `f` with each complete frame in `buf`, then removes them. Returns false if the
/// stream is malformed, which cannot be resynchronized: `buf` is emptied.
/// Each whole line `data` completes, prefixed `[<label>] `, into `emit`; the rest kept in
/// `partial`, and at `eof` emitted with a newline: a domain's output (D59).
pub fn prefixed_lines(
    label: &str,
    partial: &mut Vec<u8>,
    data: &[u8],
    eof: bool,
    mut emit: impl FnMut(&[u8]),
) {
    partial.extend_from_slice(data);
    let mut start = 0;
    while let Some(i) = partial
        .get(start..)
        .and_then(|r| r.iter().position(|&b| b == b'\n'))
    {
        let line = partial.get(start..start + i + 1).unwrap_or_default();
        emit(&[b"[", label.as_bytes(), b"] ", line].concat());
        start += i + 1;
    }
    partial.drain(..start);
    if eof && !partial.is_empty() {
        emit(&[b"[", label.as_bytes(), b"] ", partial.as_slice(), b"\n"].concat());
        partial.clear();
    }
}

pub fn each_frame(buf: &mut Vec<u8>, mut f: impl FnMut(u8, &[u8])) -> bool {
    let mut at = 0;
    let whole = loop {
        let Some(h) = buf.get(at..).and_then(<[u8]>::first_chunk::<{ run::HEADER }>) else {
            break true;
        };
        let Some((which, len)) = run::parse_header(*h) else {
            break false;
        };
        let start = at + run::HEADER;
        let Some(end) = start.checked_add(len as usize) else {
            break false;
        };
        let Some(payload) = buf.get(start..end) else {
            break true;
        };
        f(which, payload);
        at = end;
    };
    if whole {
        buf.drain(..at);
    } else {
        buf.clear();
    }
    whole
}

/// Bytes waiting to be written, in order.
#[derive(Debug, Default)]
pub struct Outbox {
    buf: Vec<u8>,
    /// How much of `buf` is written.
    at: usize,
}

impl Outbox {
    /// What is left to write.
    pub fn pending(&self) -> &[u8] {
        self.buf.get(self.at..).unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.buf.len().saturating_sub(self.at)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.at = 0;
    }

    /// Adds `parts` after what is pending, reserving for all of them at once.
    pub fn extend(&mut self, parts: &[&[u8]]) {
        // Moving the pending bytes forward costs no more than what was written since
        // they last moved.
        if self.at > 0 && self.at >= self.buf.len() / 2 {
            self.buf.drain(..self.at);
            self.at = 0;
        }
        self.buf.reserve(parts.iter().map(|p| p.len()).sum());
        for part in parts {
            self.buf.extend_from_slice(part);
        }
    }

    /// Marks the next `n` pending bytes written.
    pub fn written(&mut self, n: usize) {
        self.at = self.at.saturating_add(n).min(self.buf.len());
        if self.at == self.buf.len() {
            self.clear();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn frame(which: u8, payload: &[u8]) -> Vec<u8> {
        [&run::header(which, payload.len() as u32)[..], payload].concat()
    }

    /// A stream of frames arrives whole however it is split: every split of it into two
    /// reads, and a byte at a time, gives the same frames, in order, and leaves nothing.
    #[test]
    fn frames_arrive_whole_however_the_stream_is_split() {
        let frames: Vec<(u8, Vec<u8>)> = vec![
            (run::kind::STDIN, b"hello".to_vec()),
            (run::kind::SIGNAL, 15u32.to_be_bytes().to_vec()),
            (run::kind::STDIN, Vec::new()),
            (run::kind::RESIZE, vec![0, 24, 0, 80]),
            (run::kind::STDIN, vec![7; 300]),
        ];
        let stream: Vec<u8> = frames.iter().flat_map(|(w, p)| frame(*w, p)).collect();
        let mut splits: Vec<Vec<usize>> = (0..=stream.len()).map(|i| vec![i]).collect();
        splits.push((1..stream.len()).collect());
        for cuts in splits {
            let mut buf = Vec::new();
            let mut got = Vec::new();
            let mut from = 0;
            for to in cuts.iter().copied().chain([stream.len()]) {
                buf.extend_from_slice(&stream[from..to]);
                assert!(each_frame(&mut buf, |w, p| got.push((w, p.to_vec()))));
                from = to;
            }
            assert_eq!(got, frames, "cut at {cuts:?}");
            assert!(buf.is_empty());
        }
    }

    /// Many frames in one read, then a partial header: every frame is delivered and the
    /// partial header kept; a malformed header ends the stream and empties the buffer.
    #[test]
    fn batches_keep_their_partial_tail_and_malformed_streams_end() {
        let mut buf: Vec<u8> = (0..5461u32)
            .flat_map(|i| frame(run::kind::SIGNAL, &i.to_be_bytes()))
            .collect();
        buf.extend_from_slice(&run::header(run::kind::SIGNAL, 4)[..3]);
        let mut n = 0u32;
        assert!(each_frame(&mut buf, |_, p| {
            assert_eq!(p, n.to_be_bytes());
            n += 1;
        }));
        assert_eq!((n, &buf[..]), (5461, &run::header(run::kind::SIGNAL, 4)[..3]));
        let mut bad = frame(run::kind::STDIN, b"ok");
        bad.extend_from_slice(&[run::kind::STDIN, 1, 0, 0, 0, 0, 0, 1, 9]);
        let mut seen = 0;
        assert!(!each_frame(&mut bad, |_, _| seen += 1));
        assert_eq!((seen, bad.len()), (1, 0));
    }

    /// The audit's batch, 5,461 signal frames and a partial header in 65,535 bytes,
    /// parsed n = 500 times by the parser that removed each frame's prefix and by
    /// `each_frame` (PM M59):
    ///
    ///     cargo test --release -p shards-init frames_cost -- --ignored --nocapture
    #[test]
    #[ignore = "a measurement"]
    fn frames_cost() {
        fn draining(buf: &mut Vec<u8>, mut f: impl FnMut(u8, &[u8])) -> bool {
            loop {
                let Some(h) = buf.first_chunk::<{ run::HEADER }>() else {
                    return true;
                };
                let Some((which, len)) = run::parse_header(*h) else {
                    buf.clear();
                    return false;
                };
                let end = run::HEADER + len as usize;
                let Some(payload) = buf.get(run::HEADER..end) else {
                    return true;
                };
                f(which, payload);
                buf.drain(..end);
            }
        }
        let mut batch: Vec<u8> = (0..5461u32)
            .flat_map(|i| frame(run::kind::SIGNAL, &i.to_be_bytes()))
            .collect();
        batch.extend_from_slice(&run::header(run::kind::SIGNAL, 4)[..3]);
        assert_eq!(batch.len(), 65_535);
        let summary = |mut v: Vec<f64>| {
            v.sort_by(f64::total_cmp);
            let at = |p: f64| v[((p * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1];
            format!(
                "n {} | p50 {:.1} | p90 {:.1} | p99 {:.1} | max {:.1} us",
                v.len(),
                at(0.5),
                at(0.9),
                at(0.99),
                v[v.len() - 1]
            )
        };
        let (mut old, mut new) = (Vec::new(), Vec::new());
        for _ in 0..500 {
            for (times, cursor) in [(&mut old, false), (&mut new, true)] {
                let mut buf = batch.clone();
                let mut n = 0;
                let t = std::time::Instant::now();
                let whole = if cursor {
                    each_frame(&mut buf, |_, _| n += 1)
                } else {
                    draining(&mut buf, |_, _| n += 1)
                };
                times.push(t.elapsed().as_secs_f64() * 1e6);
                assert!(whole && n == 5461 && buf.len() == 3);
            }
        }
        println!("prefix removed per frame: {}", summary(old));
        println!("cursor, removed per batch: {}", summary(new));
    }

    /// Bytes written in any pieces, with more added between, go out in order, and what
    /// is held is moved forward only once what is written is half of it.
    #[test]
    fn an_outbox_writes_in_order_from_where_it_stopped() {
        let mut out = Outbox::default();
        let mut sent = Vec::new();
        let mut expect = Vec::new();
        for round in 0u8..50 {
            let add: Vec<u8> = (0..(round as usize * 37 % 500))
                .map(|i| i as u8 ^ round)
                .collect();
            out.extend(&[&[round], &add]);
            expect.push(round);
            expect.extend_from_slice(&add);
            let n = (round as usize * 53) % (out.len() + 1);
            sent.extend_from_slice(&out.pending()[..n]);
            out.written(n);
            assert!(out.at < out.buf.len() || out.buf.is_empty());
        }
        sent.extend_from_slice(out.pending());
        let rest = out.len();
        out.written(rest);
        assert_eq!(sent, expect);
        assert!(out.is_empty() && out.buf.is_empty());

        // Written a byte at a time, 64 KiB is moved forward rarely: at most what was written.
        let mut out = Outbox::default();
        out.extend(&[&[1u8; 1 << 16]]);
        let (mut moved, mut total) = (0, 0);
        for _ in 0..(1 << 16) - 1 {
            out.written(1);
            let before = out.at;
            out.extend(&[]);
            if out.at == 0 && before > 0 {
                moved += out.buf.len();
            }
            total += 1;
        }
        assert!(moved <= total, "moved {moved} bytes for {total} written");
    }
}
