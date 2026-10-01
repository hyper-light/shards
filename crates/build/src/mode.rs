//! Symbolic and octal modes as `chmod(1)` reads them, ported from
//! tonistiigi/dchapes-mode (mode.go and bits.go, vendored by BuildKit for `COPY --chmod`),
//! itself a translation of BSD's setmode.c: parsed with a umask of 0, as fsutil parses
//! them, and applied to Go FileModes.

use crate::copy::fm;

const IS_UID: u16 = 0o4000;
const IS_GID: u16 = 0o2000;
const IS_TXT: u16 = 0o1000;
const IRWXU: u16 = 0o700;
const IRUSER: u16 = 0o400;
const IWUSER: u16 = 0o200;
const IXUSER: u16 = 0o100;
const IRWXG: u16 = 0o070;
const IRGROUP: u16 = 0o040;
const IWGROUP: u16 = 0o020;
const IXGROUP: u16 = 0o010;
const IRWXO: u16 = 0o007;
const IROTHER: u16 = 0o004;
const IWOTHER: u16 = 0o002;
const IXOTHER: u16 = 0o001;
const STANDARD: u16 = IS_UID | IS_GID | IRWXU | IRWXG | IRWXO;

const CLEAR: u8 = 1;
const SET: u8 = 2;
const GBITS: u8 = 4;
const OBITS: u8 = 8;
const UBITS: u8 = 16;

const SYNTAX: &str = "invalid syntax";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BitCmd {
    cmd: u8,
    cmd2: u8,
    bits: u16,
}

/// A parsed mode: what to do to a file's mode bits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Set {
    cmds: Vec<BitCmd>,
}

/// `strconv.ParseInt(s, 8, 16)` for what starts with a digit, with its errors.
fn parse_octal16(s: &[u8]) -> Result<u16, String> {
    let quoted = format!("{:?}", String::from_utf8_lossy(s));
    let mut v: u32 = 0;
    for &c in s {
        if !(b'0'..=b'7').contains(&c) {
            return Err(format!("strconv.ParseInt: parsing {quoted}: invalid syntax"));
        }
        v = v.saturating_mul(8).saturating_add(u32::from(c - b'0'));
    }
    match u16::try_from(v) {
        Ok(v) if v <= 0x7fff => Ok(v),
        _ => Err(format!("strconv.ParseInt: parsing {quoted}: value out of range")),
    }
}

/// `ParseWithUmask(s, 0)`.
pub fn parse(s: &[u8]) -> Result<Set, String> {
    let mut m = Set::default();
    let Some(&first) = s.first() else {
        return Err(SYNTAX.into());
    };
    if first.is_ascii_digit() {
        let v = parse_octal16(s)?;
        if v & !(STANDARD | IS_TXT) != 0 {
            return Err(SYNTAX.into());
        }
        m.add(b'=', STANDARD | IS_TXT, v, 0);
        return Ok(m);
    }
    let mask: u16 = !0;
    let mut equal_done = false;
    let mut s = s;
    'clauses: loop {
        let mut who: u16 = 0;
        loop {
            match s.first() {
                None => return Err(SYNTAX.into()),
                Some(b'a') => who |= STANDARD,
                Some(b'u') => who |= IS_UID | IRWXU,
                Some(b'g') => who |= IS_GID | IRWXG,
                Some(b'o') => who |= IRWXO,
                Some(_) => break,
            }
            s = s.get(1..).unwrap_or_default();
        }
        // getop: one operator and its permissions, as often as they follow each other.
        loop {
            let Some((&op, rest)) = s.split_first() else {
                return Err(SYNTAX.into());
            };
            s = rest;
            match op {
                b'+' | b'-' => {}
                b'=' => equal_done = false,
                _ => return Err(SYNTAX.into()),
            }
            who &= !IS_TXT;
            let (mut perm, mut perm_x) = (0u16, 0u16);
            loop {
                let b = s.first().copied().unwrap_or(0);
                match b {
                    b'r' => perm |= IRUSER | IRGROUP | IROTHER,
                    b's' => {
                        if who == 0 || who & !IRWXO != 0 {
                            perm |= IS_UID | IS_GID;
                        }
                    }
                    b't' => {
                        if who == 0 || who & !IRWXO != 0 {
                            who |= IS_TXT;
                            perm |= IS_TXT;
                        }
                    }
                    b'w' => perm |= IWUSER | IWGROUP | IWOTHER,
                    b'X' => {
                        if op != b'-' {
                            perm_x = IXUSER | IXGROUP | IXOTHER;
                        } else {
                            perm |= IXUSER | IXGROUP | IXOTHER;
                        }
                    }
                    b'x' => perm |= IXUSER | IXGROUP | IXOTHER,
                    b'u' | b'g' | b'o' => {
                        if perm != 0 {
                            m.add(op, who, perm, mask);
                            perm = 0;
                        }
                        if op == b'=' {
                            equal_done = true;
                        }
                        if perm_x != 0 {
                            m.add(b'X', who, perm_x, mask);
                            perm_x = 0;
                        }
                        m.add(b, who, u16::from(op), mask);
                    }
                    _ => {
                        if perm != 0 || (op == b'=' && !equal_done) {
                            if op == b'=' {
                                equal_done = true;
                            }
                            m.add(op, who, perm, mask);
                        }
                        if perm_x != 0 {
                            m.add(b'X', who, perm_x, mask);
                        }
                        break;
                    }
                }
                s = s.get(1..).unwrap_or_default();
            }
            match s.first() {
                None => break 'clauses,
                Some(b',') => {
                    s = s.get(1..).unwrap_or_default();
                    continue 'clauses;
                }
                Some(_) => {}
            }
        }
    }
    m.compress();
    Ok(m)
}

impl Set {
    fn add(&mut self, op: u8, who: u16, oparg: u16, mask: u16) {
        let mut c = BitCmd {
            cmd: 0,
            cmd2: 0,
            bits: 0,
        };
        let mut op = op;
        if op == b'=' {
            c.cmd = b'-';
            c.bits = if who != 0 { who } else { STANDARD };
            self.cmds.push(c);
            op = b'+';
        }
        match op {
            b'+' | b'-' | b'X' => {
                c.cmd = op;
                c.bits = if who != 0 { who & oparg } else { mask & oparg };
            }
            _ => {
                c.cmd = op;
                if who != 0 {
                    if who & IRUSER != 0 {
                        c.cmd2 |= UBITS;
                    }
                    if who & IRGROUP != 0 {
                        c.cmd2 |= GBITS;
                    }
                    if who & IROTHER != 0 {
                        c.cmd2 |= OBITS;
                    }
                    c.bits = !0;
                } else {
                    c.cmd2 = UBITS | GBITS | OBITS;
                    c.bits = mask;
                }
                match oparg {
                    x if x == u16::from(b'+') => c.cmd2 |= SET,
                    x if x == u16::from(b'-') => c.cmd2 |= CLEAR,
                    x if x == u16::from(b'=') => c.cmd2 |= SET | CLEAR,
                    _ => {}
                }
            }
        }
        self.cmds.push(c);
    }

    /// Runs of `+`, `-` and `X` merged into at most one of each, as setmode's compress.
    fn compress(&mut self) {
        let n = self.cmds.len();
        let mut j = 0;
        let mut i = 0;
        while i < n {
            let Some(&c) = self.cmds.get(i) else { break };
            if !matches!(c.cmd, b'+' | b'-' | b'X') {
                if let Some(slot) = self.cmds.get_mut(j) {
                    *slot = c;
                }
                j += 1;
                i += 1;
                continue;
            }
            let (mut set, mut clr, mut x) = (0u16, 0u16, 0u16);
            while let Some(&c) = self.cmds.get(i) {
                match c.cmd {
                    b'-' => {
                        clr |= c.bits;
                        set &= !c.bits;
                        x &= !c.bits;
                    }
                    b'+' => {
                        set |= c.bits;
                        clr &= !c.bits;
                        x &= !c.bits;
                    }
                    b'X' => x |= c.bits & !set,
                    _ => break,
                }
                i += 1;
            }
            for (cmd, bits) in [(b'-', clr), (b'+', set), (b'X', x)] {
                if bits != 0 {
                    if let Some(slot) = self.cmds.get_mut(j) {
                        *slot = BitCmd { cmd, cmd2: 0, bits };
                    }
                    j += 1;
                }
            }
        }
        self.cmds.truncate(j);
    }

    /// `Set.Apply` to a Go FileMode.
    pub fn apply(&self, perm: u32) -> u32 {
        let omode = to_bits(perm);
        let mut new = omode;
        for c in &self.cmds {
            let value = match c.cmd {
                b'u' => (new & IRWXU) >> 6,
                b'g' => (new & IRWXG) >> 3,
                b'o' => new & IRWXO,
                _ => 0,
            };
            match c.cmd {
                b'u' | b'g' | b'o' => {
                    if c.cmd2 & CLEAR != 0 {
                        let clr = if c.cmd2 & SET != 0 { IRWXO } else { value };
                        if c.cmd2 & UBITS != 0 {
                            new &= !((clr << 6) & c.bits);
                        }
                        if c.cmd2 & GBITS != 0 {
                            new &= !((clr << 3) & c.bits);
                        }
                        if c.cmd2 & OBITS != 0 {
                            new &= !(clr & c.bits);
                        }
                    }
                    if c.cmd2 & SET != 0 {
                        if c.cmd2 & UBITS != 0 {
                            new |= (value << 6) & c.bits;
                        }
                        if c.cmd2 & GBITS != 0 {
                            new |= (value << 3) & c.bits;
                        }
                        if c.cmd2 & OBITS != 0 {
                            new |= value & c.bits;
                        }
                    }
                }
                b'+' => new |= c.bits,
                b'-' => new &= !c.bits,
                b'X' if omode & (IXUSER | IXGROUP | IXOTHER) != 0 || perm & fm::DIR != 0 => {
                    new |= c.bits;
                }
                _ => {}
            }
        }
        from_bits(perm, new)
    }
}

/// bits.go fileModeToBits.
fn to_bits(m: u32) -> u16 {
    let mut b = (m & fm::PERM) as u16;
    b |= (((m & (fm::SETUID | fm::SETGID)) >> 12) & 0xffff) as u16;
    b |= (((m & fm::STICKY) >> 11) & 0xffff) as u16;
    b
}

/// bits.go bitsToFileMode.
fn from_bits(old: u32, m: u16) -> u32 {
    let m = u32::from(m);
    let mut fm_ = old & !(fm::SETUID | fm::SETGID | fm::STICKY | fm::PERM);
    fm_ |= m & fm::PERM;
    fm_ |= (m & u32::from(IS_UID | IS_GID)) << 12;
    fm_ |= (m & u32::from(IS_TXT)) << 11;
    fm_
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Cases from dchapes-mode's mode_test.go, which BSD's chmod(1) agrees with.
    #[test]
    fn modes_apply_as_chmod_applies_them() {
        let cases: &[(&str, u32, u32)] = &[
            ("0", 0o777, 0),
            ("644", 0o777, 0o644),
            ("u+x", 0o644, 0o744),
            ("go-w", 0o666, 0o644),
            ("a=r", 0o777, 0o444),
            ("u=rwx,g=rx,o=", 0, 0o750),
            ("+X", 0o644, 0o644),
            ("+X", 0o744, 0o755),
            ("u+s", 0o755, 0o755 | fm::SETUID),
            ("g+s", 0o755, 0o755 | fm::SETGID),
            ("+t", 0o755, 0o755 | fm::STICKY),
            ("o=u", 0o750, 0o757),
            ("g=u-w", 0o750, 0o750),
            ("u-w,g+w", 0o644, 0o464),
        ];
        for &(s, from, want) in cases {
            let set = parse(s.as_bytes()).unwrap();
            assert_eq!(set.apply(from), want, "{s} on {from:o}");
        }
        assert_eq!(parse(b"+X").unwrap().apply(fm::DIR | 0o644), fm::DIR | 0o755);
    }

    #[test]
    fn bad_modes_fail_as_go_fails() {
        assert_eq!(parse(b"").unwrap_err(), "invalid syntax");
        assert_eq!(parse(b"u").unwrap_err(), "invalid syntax");
        assert_eq!(parse(b"u!x").unwrap_err(), "invalid syntax");
        assert_eq!(
            parse(b"999").unwrap_err(),
            "strconv.ParseInt: parsing \"999\": invalid syntax"
        );
        assert_eq!(
            parse(b"100000").unwrap_err(),
            "strconv.ParseInt: parsing \"100000\": value out of range"
        );
        assert_eq!(parse(b"17777").unwrap_err(), "invalid syntax");
    }
}
