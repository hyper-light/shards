//! The orders init sends its standby, and the exec arguments the standby makes of them.
//! Each string is copied once, as it is decoded, into a buffer with room for its NUL: the
//! standby's C strings are those buffers, and its pointer lists have room for their
//! terminators, so nothing is copied again or regrown (audit D10).

use std::ffi::{CString, c_char};

/// What the standby execs, and as whom: built by init, which reports any error in it.
pub struct Orders {
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
    pub cwd: Vec<u8>,
    pub explicit: bool,
    pub candidates: Vec<Vec<u8>>,
    pub argv: Vec<Vec<u8>>,
    pub env: Vec<Vec<u8>>,
    /// The terminal the workload opens as its stdio, or empty for the standby's pipes.
    pub tty: Vec<u8>,
}

impl Orders {
    /// Counts are big-endian u32s, as are lengths, each string's before its bytes. The
    /// buffer is allocated once, at the orders' length.
    pub fn encode(&self) -> Vec<u8> {
        fn len(items: &[Vec<u8>]) -> usize {
            items
                .iter()
                .fold(4, |n, item| n.saturating_add(4).saturating_add(item.len()))
        }
        fn list(w: &mut Vec<u8>, items: &[Vec<u8>]) {
            w.extend_from_slice(&u32::try_from(items.len()).unwrap_or(u32::MAX).to_be_bytes());
            for item in items {
                w.extend_from_slice(&u32::try_from(item.len()).unwrap_or(u32::MAX).to_be_bytes());
                w.extend_from_slice(item);
            }
        }
        let singles = [std::slice::from_ref(&self.cwd), std::slice::from_ref(&self.tty)];
        let total = [&self.candidates[..], &self.argv, &self.env]
            .into_iter()
            .chain(singles)
            .map(len)
            .fold(
                13usize.saturating_add(self.groups.len().saturating_mul(4)),
                usize::saturating_add,
            );
        let mut w = Vec::with_capacity(total);
        w.extend_from_slice(&self.uid.to_be_bytes());
        w.extend_from_slice(&self.gid.to_be_bytes());
        w.extend_from_slice(&u32::try_from(self.groups.len()).unwrap_or(u32::MAX).to_be_bytes());
        for g in &self.groups {
            w.extend_from_slice(&g.to_be_bytes());
        }
        list(&mut w, std::slice::from_ref(&self.cwd));
        w.push(u8::from(self.explicit));
        list(&mut w, &self.candidates);
        list(&mut w, &self.argv);
        list(&mut w, &self.env);
        list(&mut w, std::slice::from_ref(&self.tty));
        w
    }

    /// `None` for anything but a whole message `encode` wrote.
    pub fn decode(mut r: &[u8]) -> Option<Orders> {
        fn u32(r: &mut &[u8]) -> Option<u32> {
            let (head, rest) = r.split_first_chunk::<4>()?;
            *r = rest;
            Some(u32::from_be_bytes(*head))
        }
        fn list(r: &mut &[u8]) -> Option<Vec<Vec<u8>>> {
            let n = u32(r)? as usize;
            // Each item takes at least its 4-byte length.
            if n > r.len() / 4 {
                return None;
            }
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                let len = u32(r)? as usize;
                let (item, rest) = r.split_at_checked(len)?;
                *r = rest;
                // Room for the NUL its C string ends with.
                let mut owned = Vec::with_capacity(len.checked_add(1)?);
                owned.extend_from_slice(item);
                items.push(owned);
            }
            Some(items)
        }
        let uid = u32(&mut r)?;
        let gid = u32(&mut r)?;
        let n = u32(&mut r)? as usize;
        if n > r.len() / 4 {
            return None;
        }
        let mut groups = Vec::with_capacity(n);
        for _ in 0..n {
            groups.push(u32(&mut r)?);
        }
        let cwd = list(&mut r)?.pop()?;
        let (&explicit, rest) = r.split_first()?;
        r = rest;
        let orders = Orders {
            uid,
            gid,
            groups,
            cwd,
            explicit: explicit != 0,
            candidates: list(&mut r)?,
            argv: list(&mut r)?,
            env: list(&mut r)?,
            tty: list(&mut r)?.pop()?,
        };
        r.is_empty().then_some(orders)
    }
}

/// `items` as C strings, each in its own buffer; `None` if one holds a NUL.
pub fn cstrings(items: Vec<Vec<u8>>) -> Option<Vec<CString>> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(CString::new(item).ok()?);
    }
    Some(out)
}

/// A null-terminated list of `items`' pointers, as `execve(2)` takes one.
pub fn pointers(items: &[CString]) -> Vec<*const c_char> {
    let mut out = Vec::with_capacity(items.len().saturating_add(1));
    out.extend(items.iter().map(|s| s.as_ptr()));
    out.push(std::ptr::null());
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn launch() -> Orders {
        Orders {
            uid: 1000,
            gid: 1000,
            groups: vec![1000, 27],
            cwd: b"/work".to_vec(),
            explicit: false,
            candidates: vec![b"/usr/bin/sh".to_vec(), b"/bin/sh".to_vec()],
            argv: vec![b"sh".to_vec(), b"-c".to_vec(), b"exec \"$@\"".to_vec()],
            env: (0..64)
                .map(|i| format!("VARIABLE_{i:02}={}", "v".repeat(40)).into_bytes())
                .collect(),
            tty: b"/dev/pts/0".to_vec(),
        }
    }

    /// Orders are encoded into one buffer of their length, and come back whole; a message
    /// cut short, or with bytes past its end, is none.
    #[test]
    fn orders_encode_once_and_round_trip() {
        let orders = launch();
        let bytes = orders.encode();
        assert_eq!(bytes.capacity(), bytes.len());
        let back = Orders::decode(&bytes).unwrap();
        assert_eq!(
            (back.uid, back.gid, &back.groups, &back.cwd, back.explicit),
            (1000, 1000, &vec![1000, 27], &b"/work".to_vec(), false)
        );
        assert_eq!(
            (&back.candidates, &back.argv, &back.env, &back.tty),
            (&orders.candidates, &orders.argv, &orders.env, &orders.tty)
        );
        assert!(Orders::decode(&bytes[..bytes.len() - 1]).is_none());
        assert!(Orders::decode(&[&bytes[..], &[0]].concat()).is_none());
    }

    /// The standby's C strings are the buffers decoding made, and its pointer lists are
    /// allocated once, their terminators included.
    #[test]
    fn exec_strings_are_the_decoded_buffers() {
        let back = Orders::decode(&launch().encode()).unwrap();
        // Room for the NUL: a regrow may stay in place, so the pointers alone miss it.
        assert!(back.env.iter().all(|e| e.capacity() == e.len() + 1));
        let buffers: Vec<*const u8> = back.env.iter().map(|e| e.as_ptr()).collect();
        let env = cstrings(back.env).unwrap();
        assert_eq!(env.capacity(), env.len());
        for (s, buffer) in env.iter().zip(buffers) {
            assert_eq!(s.as_ptr().cast::<u8>(), buffer, "copied or regrown");
        }
        let envp = pointers(&env);
        assert_eq!((envp.len(), envp.capacity()), (65, 65));
        assert!(envp[64].is_null());
        assert!(cstrings(vec![b"a\0b".to_vec()]).is_none());
    }
}
