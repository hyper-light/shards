//! Flattened devicetree (DTB) writer, per the Devicetree Specification v0.4 §5.
//!
//! Errors from user-supplied strings (e.g. an embedded NUL in bootargs) are deferred
//! and reported by [`Fdt::finish`], keeping construction code linear.

use std::collections::HashMap;
use std::fmt;

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_END: u32 = 9;
const HEADER_LEN: usize = 40;
const VERSION: u32 = 17;
const LAST_COMP_VERSION: u32 = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FdtError {
    InvalidName(String),
    InvalidString(&'static str),
    Unbalanced,
    TooLarge(usize),
}

impl fmt::Display for FdtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FdtError::InvalidName(n) => write!(f, "invalid devicetree node/property name {n:?}"),
            FdtError::InvalidString(p) => write!(f, "devicetree property {p} contains a NUL byte"),
            FdtError::Unbalanced => write!(f, "devicetree nodes are not balanced"),
            FdtError::TooLarge(n) => write!(f, "devicetree blob is {n} bytes, over the 2 MiB boot limit"),
        }
    }
}

impl std::error::Error for FdtError {}

#[derive(Debug, Default)]
pub struct Fdt {
    structure: Vec<u8>,
    strings: Vec<u8>,
    string_offsets: HashMap<&'static str, u32>,
    reserved: Vec<(u64, u64)>,
    depth: usize,
    error: Option<FdtError>,
}

impl Fdt {
    pub fn new() -> Fdt {
        Fdt::default()
    }

    fn fail(&mut self, e: FdtError) {
        self.error.get_or_insert(e);
    }

    fn put_u32(&mut self, v: u32) {
        self.structure.extend_from_slice(&v.to_be_bytes());
    }

    fn pad(&mut self) {
        while !self.structure.len().is_multiple_of(4) {
            self.structure.push(0);
        }
    }

    /// Opens a node. The root node's name is the empty string.
    pub fn begin_node(&mut self, name: &str) {
        let valid = if self.depth == 0 {
            name.is_empty()
        } else {
            !name.is_empty() && !name.contains('\0')
        };
        if !valid {
            self.fail(FdtError::InvalidName(name.to_string()));
        }
        self.put_u32(FDT_BEGIN_NODE);
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        self.pad();
        self.depth += 1;
    }

    pub fn end_node(&mut self) {
        if self.depth == 0 {
            self.fail(FdtError::Unbalanced);
            return;
        }
        self.put_u32(FDT_END_NODE);
        self.depth -= 1;
    }

    fn name_offset(&mut self, name: &'static str) -> u32 {
        if let Some(&off) = self.string_offsets.get(name) {
            return off;
        }
        let off = self.strings.len() as u32;
        self.strings.extend_from_slice(name.as_bytes());
        self.strings.push(0);
        self.string_offsets.insert(name, off);
        off
    }

    pub fn prop(&mut self, name: &'static str, value: &[u8]) {
        if name.is_empty() || name.contains('\0') || self.depth == 0 {
            self.fail(FdtError::InvalidName(name.to_string()));
        }
        let off = self.name_offset(name);
        self.put_u32(FDT_PROP);
        self.put_u32(value.len() as u32);
        self.put_u32(off);
        self.structure.extend_from_slice(value);
        self.pad();
    }

    pub fn prop_null(&mut self, name: &'static str) {
        self.prop(name, &[]);
    }

    pub fn prop_u32(&mut self, name: &'static str, v: u32) {
        self.prop(name, &v.to_be_bytes());
    }

    pub fn prop_cells(&mut self, name: &'static str, cells: &[u32]) {
        let bytes: Vec<u8> = cells.iter().flat_map(|c| c.to_be_bytes()).collect();
        self.prop(name, &bytes);
    }

    /// Each value is written as two cells (`#address-cells`/`#size-cells` = 2).
    pub fn prop_u64s(&mut self, name: &'static str, values: &[u64]) {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_be_bytes()).collect();
        self.prop(name, &bytes);
    }

    pub fn prop_str(&mut self, name: &'static str, s: &str) {
        self.prop_strs(name, &[s]);
    }

    pub fn prop_strs(&mut self, name: &'static str, list: &[&str]) {
        let mut bytes = Vec::new();
        for s in list {
            if s.contains('\0') {
                self.fail(FdtError::InvalidString(name));
            }
            bytes.extend_from_slice(s.as_bytes());
            bytes.push(0);
        }
        self.prop(name, &bytes);
    }

    /// Adds a memory reservation block entry.
    pub fn reserve(&mut self, addr: u64, size: u64) {
        self.reserved.push((addr, size));
    }

    /// Serializes the tree. `boot_cpuid` is the physical ID of the boot CPU.
    pub fn finish(mut self, boot_cpuid: u32) -> Result<Vec<u8>, FdtError> {
        if self.depth != 0 {
            self.fail(FdtError::Unbalanced);
        }
        if let Some(e) = self.error {
            return Err(e);
        }
        self.put_u32(FDT_END);

        let rsv_off = HEADER_LEN; // 8-byte aligned
        let rsv_len = (self.reserved.len() + 1) * 16;
        let struct_off = rsv_off + rsv_len;
        let strings_off = struct_off + self.structure.len();
        let total = strings_off + self.strings.len();
        if total > 2 << 20 {
            return Err(FdtError::TooLarge(total));
        }

        let mut out = Vec::with_capacity(total);
        for v in [
            FDT_MAGIC,
            total as u32,
            struct_off as u32,
            strings_off as u32,
            rsv_off as u32,
            VERSION,
            LAST_COMP_VERSION,
            boot_cpuid,
            self.strings.len() as u32,
            self.structure.len() as u32,
        ] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        for (addr, size) in self.reserved.iter().copied().chain([(0, 0)]) {
            out.extend_from_slice(&addr.to_be_bytes());
            out.extend_from_slice(&size.to_be_bytes());
        }
        out.extend_from_slice(&self.structure);
        out.extend_from_slice(&self.strings);
        Ok(out)
    }
}

/// Minimal DTB decoder, used to verify generated trees in tests.
#[cfg(test)]
pub mod decode {
    use std::collections::BTreeMap;

    #[derive(Debug, Default, PartialEq)]
    pub struct Node {
        pub props: BTreeMap<String, Vec<u8>>,
        pub children: BTreeMap<String, Node>,
    }

    impl Node {
        pub fn path(&self, path: &str) -> &Node {
            path.split('/')
                .filter(|s| !s.is_empty())
                .fold(self, |n, c| &n.children[c])
        }
        pub fn u32(&self, prop: &str) -> u32 {
            u32::from_be_bytes(self.props[prop][..4].try_into().unwrap())
        }
        pub fn cells(&self, prop: &str) -> Vec<u32> {
            self.props[prop]
                .chunks(4)
                .map(|c| u32::from_be_bytes(c.try_into().unwrap()))
                .collect()
        }
        pub fn str(&self, prop: &str) -> &str {
            let v = &self.props[prop];
            std::str::from_utf8(&v[..v.len() - 1]).unwrap()
        }
    }

    fn be32(b: &[u8], off: usize) -> u32 {
        u32::from_be_bytes(b[off..off + 4].try_into().unwrap())
    }

    pub fn parse(blob: &[u8]) -> Node {
        assert_eq!(be32(blob, 0), super::FDT_MAGIC);
        assert_eq!(be32(blob, 4) as usize, blob.len());
        let (so, st) = (be32(blob, 8) as usize, be32(blob, 12) as usize);
        let name_at = |off: usize| {
            let s = &blob[st + off..];
            std::str::from_utf8(&s[..s.iter().position(|&c| c == 0).unwrap()])
                .unwrap()
                .to_string()
        };
        let mut stack: Vec<(String, Node)> = Vec::new();
        let mut pos = so;
        loop {
            let tok = be32(blob, pos);
            pos += 4;
            match tok {
                1 => {
                    let end = pos + blob[pos..].iter().position(|&c| c == 0).unwrap();
                    let name = std::str::from_utf8(&blob[pos..end]).unwrap().to_string();
                    pos = (end + 1).next_multiple_of(4);
                    stack.push((name, Node::default()));
                }
                2 => {
                    let (name, node) = stack.pop().unwrap();
                    match stack.last_mut() {
                        Some(parent) => {
                            parent.1.children.insert(name, node);
                        }
                        None => return node,
                    }
                }
                3 => {
                    let (len, nameoff) = (be32(blob, pos) as usize, be32(blob, pos + 4) as usize);
                    let value = blob[pos + 8..pos + 8 + len].to_vec();
                    stack.last_mut().unwrap().1.props.insert(name_at(nameoff), value);
                    pos = (pos + 8 + len).next_multiple_of(4);
                }
                t => panic!("unexpected token {t}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut f = Fdt::new();
        f.reserve(0x1000, 0x2000);
        f.begin_node("");
        f.prop_u32("#address-cells", 2);
        f.prop_strs("compatible", &["a,b", "c"]);
        f.begin_node("memory@80000000");
        f.prop_str("device_type", "memory");
        f.prop_u64s("reg", &[0x8000_0000, 0x1000_0000]);
        f.end_node();
        f.begin_node("chosen");
        f.prop_null("empty");
        f.end_node();
        f.end_node();
        let blob = f.finish(3).unwrap();

        assert_eq!(blob.len() % 4, 0);
        assert_eq!(u32::from_be_bytes(blob[28..32].try_into().unwrap()), 3); // boot_cpuid_phys
        assert_eq!(u64::from_be_bytes(blob[40..48].try_into().unwrap()), 0x1000); // rsvmap
        let root = decode::parse(&blob);
        assert_eq!(root.u32("#address-cells"), 2);
        assert_eq!(root.props["compatible"], b"a,b\0c\0");
        let mem = root.path("memory@80000000");
        assert_eq!(mem.str("device_type"), "memory");
        assert_eq!(mem.cells("reg"), vec![0, 0x8000_0000, 0, 0x1000_0000]);
        assert!(root.path("chosen").props["empty"].is_empty());
    }

    #[test]
    fn rejects_bad_input() {
        let mut f = Fdt::new();
        f.begin_node("");
        f.begin_node("chosen");
        f.prop_str("bootargs", "console=ttyS0\0evil");
        f.end_node();
        f.end_node();
        assert_eq!(f.finish(0), Err(FdtError::InvalidString("bootargs")));

        let mut f = Fdt::new();
        f.begin_node("");
        assert_eq!(f.finish(0), Err(FdtError::Unbalanced));

        let mut f = Fdt::new();
        f.begin_node("not-root");
        f.end_node();
        assert!(matches!(f.finish(0), Err(FdtError::InvalidName(_))));
    }
}
