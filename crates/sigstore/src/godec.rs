//! Go's encoding/json decoding into typed values (decode.go, as Go 1.26 has it), for the
//! documents BuildKit's policy helpers read into Go types: a value is decoded into what
//! the target already holds, so a repeated member merges into a struct, a map or a
//! slice's elements and `null` leaves a struct, string or number as it was; a slice's
//! elements are decoded in place, past its length into the capacity Go's runtime gave it,
//! where an earlier, longer array left elements a shorter one did not overwrite; and
//! the first type mismatch is kept, worded with the struct and field path Go reports,
//! while the rest of the document is still read.

use std::collections::BTreeMap;

use crate::tlog::gojson::{self, JValue};

/// A decoding: the first error, and where in the target it is (errorContext).
#[derive(Debug, Default)]
pub struct Dec {
    err: Option<String>,
    strukt: Option<String>,
    stack: Vec<String>,
}

impl Dec {
    pub fn new() -> Dec {
        Dec::default()
    }

    /// An UnmarshalTypeError, kept if it is the first error.
    pub fn save(&mut self, value: &str, ty: &str) {
        if self.err.is_some() {
            return;
        }
        self.err = Some(if self.strukt.is_some() || !self.stack.is_empty() {
            format!(
                "json: cannot unmarshal {value} into Go struct field {}.{} of type {ty}",
                self.strukt.as_deref().unwrap_or_default(),
                self.stack.join(".")
            )
        } else {
            format!("json: cannot unmarshal {value} into Go value of type {ty}")
        });
    }

    /// Any other error, kept as it is if it is the first.
    pub fn save_error(&mut self, e: String) {
        if self.err.is_none() {
            self.err = Some(e);
        }
    }

    pub fn ok(&self) -> bool {
        self.err.is_none()
    }

    pub fn done<T>(self, t: T) -> Result<T, String> {
        match self.err {
            Some(e) => Err(e),
            None => Ok(t),
        }
    }

    /// A struct named `name` (empty for an anonymous one) of type `ty`: each member
    /// matching one of `fields` (its JSON name, and its path in errors) given to `f`.
    pub fn object(
        &mut self,
        v: &JValue,
        name: &str,
        ty: &str,
        fields: &[(&str, &str)],
        mut f: impl FnMut(&mut Dec, usize, &JValue),
    ) {
        match v {
            JValue::Object(members) => {
                let names: Vec<&str> = fields.iter().map(|(json, _)| *json).collect();
                for (i, x) in gojson::struct_members(members, &names) {
                    let (strukt, depth) = (self.strukt.take(), self.stack.len());
                    let path = fields.get(i).map(|(_, p)| *p).unwrap_or_default();
                    self.stack.push(path.to_string());
                    self.strukt = Some(name.to_string());
                    f(self, i, x);
                    self.stack.truncate(depth);
                    self.strukt = strukt;
                }
            }
            JValue::Null => {}
            v => self.save(v.kind(), ty),
        }
    }

    /// A pointer to a struct: `null` makes it nil, anything else is decoded into what it
    /// points to, allocated first.
    pub fn pointer<T: Default>(
        &mut self,
        v: &JValue,
        dst: &mut Option<T>,
        f: impl FnOnce(&mut Dec, &JValue, &mut T),
    ) {
        if matches!(v, JValue::Null) {
            *dst = None;
            return;
        }
        let t = dst.get_or_insert_with(T::default);
        f(self, v, t);
    }

    pub fn string(&mut self, v: &JValue, dst: &mut String, ty: &str) {
        match v {
            JValue::Str(s) => s.clone_into(dst),
            JValue::Null => {}
            v => self.save(v.kind(), ty),
        }
    }

    pub fn int64(&mut self, v: &JValue, dst: &mut i64, ty: &str) {
        match v {
            JValue::Number(n) => match n.parse::<i64>() {
                Ok(i) => *dst = i,
                Err(_) => self.save(&format!("number {n}"), ty),
            },
            JValue::Null => {}
            v => self.save(v.kind(), ty),
        }
    }

    fn uint8(&mut self, v: &JValue, dst: &mut u8) {
        match v {
            JValue::Number(n) => match n.parse::<u8>() {
                Ok(i) => *dst = i,
                Err(_) => self.save(&format!("number {n}"), "uint8"),
            },
            JValue::Null => {}
            v => self.save(v.kind(), "uint8"),
        }
    }

    /// A slice of `ty` whose elements are `elem` bytes, holding pointers unless `noscan`.
    pub fn slice<T: Default>(
        &mut self,
        v: &JValue,
        dst: &mut GoSlice<T>,
        ty: &str,
        elem: Elem,
        mut f: impl FnMut(&mut Dec, &JValue, &mut T),
    ) {
        match v {
            JValue::Array(items) => {
                let mut i = 0;
                for x in items {
                    if i >= dst.buf.len() {
                        dst.grow(elem);
                    }
                    if i >= dst.len {
                        dst.len = i.saturating_add(1);
                    }
                    if let Some(t) = dst.buf.get_mut(i) {
                        f(self, x, t);
                    }
                    i = i.saturating_add(1);
                }
                if i < dst.len {
                    dst.len = i;
                }
                // An empty array leaves a new, empty slice.
                if i == 0 {
                    *dst = GoSlice::default();
                }
            }
            JValue::Null => *dst = GoSlice::default(),
            v => self.save(v.kind(), ty),
        }
    }

    pub fn strings(&mut self, v: &JValue, dst: &mut GoSlice<String>) {
        self.slice(v, dst, "[]string", STRING, |d, x, s| d.string(x, s, "string"));
    }

    /// A []byte: a string's base64, or an array of bytes.
    pub fn bytes(&mut self, v: &JValue, dst: &mut GoSlice<u8>) {
        match v {
            JValue::Str(s) => match crate::gobase64::decode(s.as_bytes(), false, true) {
                // Go keeps make([]byte, DecodedLen)'s spare capacity, but it holds only
                // zeros, as fresh growth would: it is never seen.
                Ok(buf) => *dst = GoSlice { len: buf.len(), buf },
                Err(o) => self.save_error(crate::gobase64::error_text(o)),
            },
            v => self.slice(v, dst, "[]uint8", BYTE, |d, x, b| d.uint8(x, b)),
        }
    }

    /// A map[string]string: members added to what it holds.
    pub fn map(&mut self, v: &JValue, dst: &mut BTreeMap<String, String>) {
        match v {
            JValue::Object(members) => {
                for (k, x) in members {
                    let mut e = String::new();
                    self.string(x, &mut e, "string");
                    dst.insert(k.clone(), e);
                }
            }
            JValue::Null => dst.clear(),
            v => self.save(v.kind(), "map[string]string"),
        }
    }

    /// An interface{}: what encoding/json makes of any value, numbers as float64.
    pub fn any(&mut self, v: &JValue, dst: &mut Any) {
        match v {
            JValue::Null => *dst = Any::Nil,
            JValue::Bool(_) => *dst = Any::Other,
            JValue::Str(s) => *dst = Any::Str(s.clone()),
            JValue::Number(n) => {
                if let Some(f) = self.float(n) {
                    *dst = Any::F64(f);
                }
            }
            v => {
                self.numbers(v);
                *dst = Any::Other;
            }
        }
    }

    /// A map[string]interface{}.
    pub fn any_map(&mut self, v: &JValue) {
        match v {
            JValue::Object(_) => self.numbers(v),
            JValue::Null => {}
            v => self.save(v.kind(), "map[string]interface {}"),
        }
    }

    /// convertNumber: a float64, or an error where it is out of range.
    fn float(&mut self, n: &str) -> Option<f64> {
        match n.parse::<f64>() {
            Ok(f) if f.is_finite() => Some(f),
            _ => {
                self.save(&format!("number {n}"), "float64");
                None
            }
        }
    }

    /// The numbers inside an interface{}'s array or object.
    fn numbers(&mut self, v: &JValue) {
        match v {
            JValue::Number(n) => {
                self.float(n);
            }
            JValue::Array(a) => a.iter().for_each(|x| self.numbers(x)),
            JValue::Object(m) => m.iter().for_each(|(_, x)| self.numbers(x)),
            _ => {}
        }
    }
}

/// An interface{}'s value, as far as it is read.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Any {
    #[default]
    Nil,
    F64(f64),
    Str(String),
    Other,
}

impl Any {
    /// policy-helpers' anyToInt64.
    pub fn to_int64(&self) -> Option<i64> {
        match self {
            // Go's int64(f): toward zero; out of range, as arm64 converts (saturating).
            Any::F64(f) => Some(*f as i64),
            Any::Str(s) => s.parse::<i64>().ok(),
            _ => None,
        }
    }
}

/// A Go slice: its elements up to its capacity, and its length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoSlice<T> {
    buf: Vec<T>,
    len: usize,
}

impl<T> Default for GoSlice<T> {
    fn default() -> Self {
        GoSlice {
            buf: Vec::new(),
            len: 0,
        }
    }
}

impl<T: Default> GoSlice<T> {
    pub fn items(&self) -> &[T] {
        self.buf.get(..self.len).unwrap_or_default()
    }

    pub fn into_vec(mut self) -> Vec<T> {
        self.buf.truncate(self.len);
        self.buf
    }

    /// reflect.Value.Grow(1) where the slice is full: growslice's new capacity.
    fn grow(&mut self, elem: Elem) {
        let cap = self.buf.len();
        let new_len = cap.saturating_add(1);
        let double = cap.saturating_mul(2);
        let mut newcap = if new_len > double {
            new_len
        } else if cap < 256 {
            double
        } else {
            let mut c = cap;
            while c < new_len {
                c = c.saturating_add(c.saturating_add(3 * 256) >> 2);
            }
            c
        };
        let size = elem.size.max(1);
        newcap = roundupsize(newcap.saturating_mul(size), elem.noscan) / size;
        self.buf.resize_with(newcap.max(new_len), T::default);
    }
}

/// A slice element's size in bytes on a 64-bit Go, and whether it holds no pointers.
#[derive(Debug, Clone, Copy)]
pub struct Elem {
    pub size: usize,
    pub noscan: bool,
}

pub const BYTE: Elem = Elem {
    size: 1,
    noscan: true,
};
pub const STRING: Elem = Elem {
    size: 16,
    noscan: false,
};

/// The runtime's size classes (internal/runtime/gc/sizeclasses.go).
const SIZE_CLASSES: [usize; 68] = [
    0, 8, 16, 24, 32, 48, 64, 80, 96, 112, 128, 144, 160, 176, 192, 208, 224, 240, 256, 288, 320, 352, 384,
    416, 448, 480, 512, 576, 640, 704, 768, 896, 1024, 1152, 1280, 1408, 1536, 1792, 2048, 2304, 2688, 3072,
    3200, 3456, 4096, 4864, 5376, 6144, 6528, 6784, 6912, 8192, 9472, 9728, 10240, 10880, 12288, 13568,
    14336, 16384, 18432, 19072, 20480, 21760, 24576, 27264, 28672, 32768,
];

/// roundupsize (runtime/msize.go): what mallocgc allocates for `size`, less the malloc
/// header a small object with pointers carries past 512 bytes.
fn roundupsize(size: usize, noscan: bool) -> usize {
    const MAX_SMALL: usize = 32768;
    const HEADER: usize = 8;
    const PAGE: usize = 8192;
    if size <= MAX_SMALL - HEADER {
        let req = if !noscan && size > 512 {
            size + HEADER
        } else {
            size
        };
        let class = SIZE_CLASSES
            .iter()
            .copied()
            .find(|&c| c >= req && c > 0)
            .unwrap_or(MAX_SMALL);
        return class - (req - size);
    }
    match size.checked_add(PAGE - 1) {
        Some(r) => r & !(PAGE - 1),
        None => size,
    }
}

/// A Go struct type's name as reflect prints an anonymous one: its fields, types and
/// JSON tags.
pub fn anonymous(fields: &[(&str, &str, &str)]) -> String {
    let parts: Vec<String> = fields
        .iter()
        .map(|(name, ty, json)| format!("{name} {ty} \"json:\\\"{json}\\\"\""))
        .collect();
    format!("struct {{ {} }}", parts.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(elem: Elem, n: usize) -> Vec<usize> {
        let mut s: GoSlice<u8> = GoSlice::default();
        let mut out = Vec::new();
        for _ in 0..n {
            s.grow(elem);
            s.len = s.buf.len();
            out.push(s.buf.len());
        }
        out
    }

    #[test]
    fn slices_grow_as_go_s_runtime_grows_them() {
        // reflect.Value.Grow(1)'s capacities on Go 1.26.1, darwin/arm64, measured: for
        // v1.Descriptor (120 bytes), string and byte, and one 600-byte object.
        assert_eq!(
            caps(
                Elem {
                    size: 120,
                    noscan: false
                },
                6
            ),
            [1, 2, 4, 8, 17, 34]
        );
        assert_eq!(caps(STRING, 7), [1, 2, 4, 8, 16, 32, 71]);
        assert_eq!(caps(BYTE, 6), [8, 16, 32, 64, 128, 256]);
        assert_eq!(
            caps(
                Elem {
                    size: 600,
                    noscan: false
                },
                1
            ),
            [1]
        );
        // A large object: whole pages (AppendSlice of 40000 bytes to an empty []byte).
        assert_eq!(roundupsize(40000, true), 40960);
    }

    #[test]
    fn anonymous_structs_print_as_reflect_prints_them() {
        assert_eq!(
            anonymous(&[("DockerReference", "string", "docker-reference")]),
            r#"struct { DockerReference string "json:\"docker-reference\"" }"#
        );
    }
}
