//! What the Docker CLI does with a terminal's input, from moby/term v0.5.2, which docker/cli
//! v29.8.1 vendors: detach keys as `--detach-keys` spells them (ascii.go, `ToBytes`), and
//! the proxy that finds them in what the user types (proxy.go, `escapeProxy`).

/// The default detach keys, ctrl-p ctrl-q (docker/cli cli/command/container/hijack.go).
pub const DETACH_KEYS: &[u8] = &[16, 17];

/// The bytes `keys` names, as `ToBytes` reads them: comma-separated items, each one
/// character, `ctrl-@` to `ctrl-_` (bytes 0 to 31), or `DEL`.
pub fn to_bytes(keys: &str) -> Result<Vec<u8>, String> {
    const CTRL: &[u8] = b"@abcdefghijklmnopqrstuvwxyz[\\]^_";
    keys.split(',')
        .map(|key| match key.as_bytes() {
            [one] => Ok(*one),
            _ if key == "DEL" => Ok(127),
            _ => key
                .strip_prefix("ctrl-")
                .and_then(|c| match c.as_bytes() {
                    [c] => CTRL.iter().position(|k| k == c),
                    _ => None,
                })
                .and_then(|code| u8::try_from(code).ok())
                .ok_or_else(|| format!("Unknown character: '{key}'")),
        })
        .collect()
}

/// Finds the detach keys in input as it is read, as `escapeProxy` does: bytes that could
/// begin them are held back until the next ones show whether they do, and the keys
/// themselves never pass. A byte that breaks a partial match passes after the bytes held
/// back, and is not taken as the start of a new one.
#[derive(Debug, Clone)]
pub struct EscapeProxy {
    keys: Vec<u8>,
    /// How many of the keys the held-back input matched.
    matched: usize,
}

/// What a read of input comes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    /// Bytes for the command's input.
    Input(Vec<u8>),
    /// The detach keys, and the bytes before them in this read, which pass first.
    Detach(Vec<u8>),
}

impl EscapeProxy {
    /// A proxy for `keys`; none detaches.
    pub fn new(keys: &[u8]) -> EscapeProxy {
        EscapeProxy {
            keys: keys.to_vec(),
            matched: 0,
        }
    }

    /// Passes a read of `input` through.
    pub fn read(&mut self, input: &[u8]) -> Read {
        let mut out = Vec::with_capacity(input.len());
        for &b in input {
            if self.keys.get(self.matched) == Some(&b) {
                self.matched += 1;
                if self.matched == self.keys.len() {
                    return Read::Detach(out);
                }
                continue;
            }
            out.extend_from_slice(self.keys.get(..self.matched).unwrap_or_default());
            out.push(b);
            self.matched = 0;
        }
        Read::Input(out)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn detach_keys_are_spelled_as_to_bytes_spells_them() {
        assert_eq!(to_bytes("ctrl-p,ctrl-q").unwrap(), DETACH_KEYS);
        assert_eq!(
            to_bytes("a,ctrl-@,ctrl-z,ctrl-_,DEL").unwrap(),
            [b'a', 0, 26, 31, 127]
        );
        assert_eq!(to_bytes("ctrl-\\,ctrl-]").unwrap(), [28, 29]);
        for (keys, item) in [
            ("ctrl-A", "ctrl-A"),
            ("ctrl-p,", ""),
            ("", ""),
            ("ab", "ab"),
            ("é", "é"),
            ("del", "del"),
        ] {
            assert_eq!(to_bytes(keys), Err(format!("Unknown character: '{item}'")));
        }
    }

    #[test]
    fn the_proxy_holds_back_what_may_be_the_keys() {
        let mut p = EscapeProxy::new(DETACH_KEYS);
        assert_eq!(p.read(b"ab"), Read::Input(b"ab".to_vec()));
        // A lone ctrl-p waits for the next byte, and goes with it.
        assert_eq!(p.read(&[16]), Read::Input(Vec::new()));
        assert_eq!(p.read(b"x"), Read::Input(vec![16, b'x']));
        // Within one read, and across reads, the keys detach; what came first passes.
        assert_eq!(p.read(&[b'a', 16, 17, b'b']), Read::Detach(b"a".to_vec()));
        let mut p = EscapeProxy::new(DETACH_KEYS);
        assert_eq!(p.read(&[b'a', 16]), Read::Input(b"a".to_vec()));
        assert_eq!(p.read(&[17]), Read::Detach(Vec::new()));
        // The byte that breaks a match is not the start of another.
        let mut p = EscapeProxy::new(DETACH_KEYS);
        assert_eq!(p.read(&[16, 16, 17]), Read::Input(vec![16, 16, 17]));
        let mut p = EscapeProxy::new(&[]);
        assert_eq!(p.read(&[16, 17]), Read::Input(vec![16, 17]));
    }
}
