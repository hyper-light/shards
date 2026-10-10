//! TUF metadata as go-tuf v2's types read it (metadata/types.go, marshal.go): each field
//! decoded as encoding/json decodes it (names matched without regard to case, the last of
//! a name winning, `null` leaving a value as it was), unrecognized fields kept, and each
//! written back as go-tuf writes it, which is what its signatures are checked over
//! (cjson of the re-encoding).

use shards_dockerfile::go::{Time, parse_rfc3339};

use crate::Error;
use crate::gojson::{Canon, Value, int};

/// A Go map of pointers by name, in document order: a null entry is a nil pointer.
pub type Named<T> = Vec<(String, Option<T>)>;
/// Hashes: each algorithm's digest.
pub type Hashes = Vec<(String, Vec<u8>)>;

pub const ROOT: &str = "root";
pub const SNAPSHOT: &str = "snapshot";
pub const TARGETS: &str = "targets";
pub const TIMESTAMP: &str = "timestamp";

/// encoding/json's matching of a JSON name to a field's: equal, or equal once folded
/// (ASCII case, and the Kelvin sign and long s that fold to `k` and `s`).
fn fold_eq(key: &str, field: &str) -> bool {
    let fold = |c: char| match c {
        '\u{212A}' => 'k',
        '\u{017F}' => 's',
        c => c.to_ascii_lowercase(),
    };
    key.chars().map(fold).eq(field.chars().map(fold))
}

/// An object's members, for a struct's decoding.
struct Obj<'a> {
    members: &'a [(String, Value)],
    /// The Go struct's name, for type errors.
    name: &'static str,
}

/// json.UnmarshalTypeError's text.
fn type_error(value: &Value, owner: &str, field: &str, ty: &str) -> Error {
    Error::Json(format!(
        "json: cannot unmarshal {} into Go struct field {owner}.{field} of type {ty}",
        value.kind()
    ))
}

impl<'a> Obj<'a> {
    fn of(v: &'a Value, name: &'static str, ty: &str) -> Result<Option<Obj<'a>>, Error> {
        match v {
            Value::Object(m) => Ok(Some(Obj { members: m, name })),
            Value::Null => Ok(None),
            other => Err(Error::Json(format!(
                "json: cannot unmarshal {} into Go value of type {ty}",
                other.kind()
            ))),
        }
    }

    /// The value for `field`: the last member whose name matches it.
    fn field(&self, field: &str) -> Option<&'a Value> {
        self.members
            .iter()
            .rev()
            .find(|(k, _)| fold_eq(k, field))
            .map(|(_, v)| v)
    }

    /// The members whose names are none of `known` exactly: UnrecognizedFields.
    fn extra(&self, known: &[&str]) -> Vec<(String, Value)> {
        self.members
            .iter()
            .filter(|(k, _)| !known.contains(&k.as_str()))
            .cloned()
            .collect()
    }

    fn string(&self, field: &str) -> Result<String, Error> {
        match self.field(field) {
            None | Some(Value::Null) => Ok(String::new()),
            Some(Value::String(s)) => Ok(s.clone()),
            Some(v) => Err(type_error(v, self.name, field, "string")),
        }
    }

    fn bool(&self, field: &str) -> Result<bool, Error> {
        match self.field(field) {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            Some(v) => Err(type_error(v, self.name, field, "bool")),
        }
    }

    fn int(&self, field: &str, ty: &str) -> Result<i64, Error> {
        match self.field(field) {
            None | Some(Value::Null) => Ok(0),
            Some(Value::Number(n)) => n.parse::<i64>().map_err(|_| {
                Error::Json(format!(
                    "json: cannot unmarshal number {n} into Go struct field {}.{field} of type {ty}",
                    self.name
                ))
            }),
            Some(v) => Err(type_error(v, self.name, field, ty)),
        }
    }

    fn strings(&self, field: &str) -> Result<Option<Vec<String>>, Error> {
        match self.field(field) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| match v {
                    Value::String(s) => Ok(s.clone()),
                    Value::Null => Ok(String::new()),
                    v => Err(type_error(v, self.name, field, "string")),
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Some),
            Some(v) => Err(type_error(v, self.name, field, "[]string")),
        }
    }

    fn time(&self, field: &str) -> Result<Option<Time>, Error> {
        match self.field(field) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => parse_rfc3339(s.as_bytes())
                .map(Some)
                .map_err(|e| Error::Json(String::from_utf8_lossy(&e).into_owned())),
            Some(_) => Err(Error::Json(
                "Time.UnmarshalJSON: input is not a JSON string".into(),
            )),
        }
    }
}

/// HexBytes.UnmarshalJSON: a quoted string of an even number of hex digits.
fn hex_bytes(v: &Value) -> Result<Vec<u8>, Error> {
    let bad = || Error::Json("tuf: invalid JSON hex bytes".into());
    let Value::String(s) = v else {
        return Err(bad());
    };
    if !s.len().is_multiple_of(2) {
        return Err(bad());
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    for i in (0..b.len()).step_by(2) {
        let d = |c: u8| (c as char).to_digit(16);
        match (b.get(i).copied().and_then(d), b.get(i + 1).copied().and_then(d)) {
            (Some(h), Some(l)) => out.push((h * 16 + l) as u8),
            _ => {
                let bad = b.get(i..i + 2).unwrap_or_default();
                let c = bad.iter().find(|c| !c.is_ascii_hexdigit()).copied().unwrap_or(0);
                return Err(Error::Json(format!(
                    "encoding/hex: invalid byte: U+{:04X} {:?}",
                    c, c as char
                )));
            }
        }
    }
    Ok(out)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn time_canon(t: &Option<Time>) -> Result<Canon, Error> {
    let zero = Time {
        year: 1,
        month: 1,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0,
        nanosecond: 0,
        offset: 0,
    };
    t.as_ref()
        .unwrap_or(&zero)
        .rfc3339_nano()
        .map(Canon::String)
        .map_err(|e| {
            Error::Json(format!(
                "json: error calling MarshalJSON for type time.Time: {}",
                String::from_utf8_lossy(&e)
            ))
        })
}

/// An object written as go-tuf writes it: the unrecognized fields first, then its own,
/// which replace any of the same name (of a name, the last is the one written).
fn object(extra: &[(String, Value)], own: Vec<(&str, Canon)>) -> Canon {
    let mut m = Vec::with_capacity(extra.len() + own.len());
    m.extend(extra.iter().map(|(k, v)| (k.clone(), Canon::from_any(v))));
    m.extend(own.into_iter().map(|(k, v)| (k.to_string(), v)));
    Canon::Object(m)
}

/// A Go map decoded from an object's members: the last of a name winning, at the first's
/// place, as [`Named`] keeps them.
fn map_of<T>(
    m: &[(String, Value)],
    mut f: impl FnMut(&str, &Value) -> Result<T, Error>,
) -> Result<Vec<(String, T)>, Error> {
    let mut out: Vec<(String, T)> = Vec::with_capacity(m.len());
    let mut at: std::collections::HashMap<&str, usize> = std::collections::HashMap::with_capacity(m.len());
    for (k, v) in m {
        let item = f(k, v)?;
        match at.get(k.as_str()).and_then(|&i| out.get_mut(i)) {
            Some(slot) => slot.1 = item,
            None => {
                at.insert(k.as_str(), out.len());
                out.push((k.clone(), item));
            }
        }
    }
    Ok(out)
}

fn strings_canon(v: &Option<Vec<String>>) -> Canon {
    match v {
        None => Canon::Null,
        Some(v) => Canon::Array(v.iter().map(|s| Canon::String(s.clone())).collect()),
    }
}

/// A key (Key), by its ID.
#[derive(Debug, Clone, PartialEq)]
pub struct Key {
    pub keytype: String,
    pub scheme: String,
    pub public: String,
    extra: Vec<(String, Value)>,
    val_extra: Vec<(String, Value)>,
}

impl Key {
    fn decode(v: &Value) -> Result<Option<Key>, Error> {
        let Some(o) = Obj::of(v, "Alias", "metadata.Key")? else {
            return Ok(match v {
                Value::Null => None,
                _ => Some(Key::zero()),
            });
        };
        let (public, val_extra) = match o.field("keyval") {
            None | Some(Value::Null) => (String::new(), Vec::new()),
            Some(kv) => match Obj::of(kv, "Alias", "metadata.KeyVal")? {
                None => (String::new(), Vec::new()),
                Some(kvo) => (kvo.string("public")?, kvo.extra(&["public"])),
            },
        };
        Ok(Some(Key {
            keytype: o.string("keytype")?,
            scheme: o.string("scheme")?,
            public,
            extra: o.extra(&["keytype", "scheme", "keyval"]),
            val_extra,
        }))
    }

    fn zero() -> Key {
        Key {
            keytype: String::new(),
            scheme: String::new(),
            public: String::new(),
            extra: Vec::new(),
            val_extra: Vec::new(),
        }
    }

    fn canon(&self) -> Canon {
        object(
            &self.extra,
            vec![
                ("keytype", Canon::String(self.keytype.clone())),
                ("scheme", Canon::String(self.scheme.clone())),
                (
                    "keyval",
                    object(
                        &self.val_extra,
                        vec![("public", Canon::String(self.public.clone()))],
                    ),
                ),
            ],
        )
    }
}

/// map[string]*Key: an absent or null map is nil; a null entry a nil key.
fn keys(o: &Obj<'_>, field: &str) -> Result<Option<Named<Key>>, Error> {
    match o.field(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(m)) => map_of(m, |_, v| Key::decode(v)).map(Some),
        Some(v) => Err(type_error(v, o.name, field, "map[string]*metadata.Key")),
    }
}

fn keys_canon(keys: &Option<Named<Key>>) -> Canon {
    match keys {
        None => Canon::Null,
        Some(m) => Canon::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_ref().map_or(Canon::Null, Key::canon)))
                .collect(),
        ),
    }
}

/// A role's keys and threshold (Role).
#[derive(Debug, Clone, PartialEq)]
pub struct Role {
    pub keyids: Option<Vec<String>>,
    pub threshold: i64,
    extra: Vec<(String, Value)>,
}

impl Role {
    fn decode(v: &Value) -> Result<Option<Role>, Error> {
        if matches!(v, Value::Null) {
            return Ok(None);
        }
        let Some(o) = Obj::of(v, "Alias", "metadata.Role")? else {
            return Ok(None);
        };
        Ok(Some(Role {
            keyids: o.strings("keyids")?,
            threshold: o.int("threshold", "int")?,
            extra: o.extra(&["keyids", "threshold"]),
        }))
    }

    fn canon(&self) -> Canon {
        object(
            &self.extra,
            vec![
                ("keyids", strings_canon(&self.keyids)),
                ("threshold", int(self.threshold)),
            ],
        )
    }
}

/// What metadata says of another metadata file (MetaFiles).
#[derive(Debug, Clone, PartialEq)]
pub struct MetaFile {
    pub length: i64,
    pub hashes: Option<Hashes>,
    pub version: i64,
    extra: Vec<(String, Value)>,
}

fn hashes(o: &Obj<'_>, field: &str) -> Result<Option<Hashes>, Error> {
    match o.field(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(m)) => map_of(m, |_, v| hex_bytes(v)).map(Some),
        Some(v) => Err(type_error(v, o.name, field, "metadata.Hashes")),
    }
}

fn hashes_canon(h: &[(String, Vec<u8>)]) -> Canon {
    Canon::Object(
        h.iter()
            .map(|(k, v)| (k.clone(), Canon::String(hex(v))))
            .collect(),
    )
}

impl MetaFile {
    fn decode(v: &Value) -> Result<Option<MetaFile>, Error> {
        if matches!(v, Value::Null) {
            return Ok(None);
        }
        let Some(o) = Obj::of(v, "Alias", "metadata.MetaFiles")? else {
            return Ok(None);
        };
        Ok(Some(MetaFile {
            length: o.int("length", "int64")?,
            hashes: hashes(&o, "hashes")?,
            version: o.int("version", "int64")?,
            extra: o.extra(&["length", "hashes", "version"]),
        }))
    }

    fn canon(&self) -> Canon {
        let mut own = Vec::new();
        if self.length != 0 {
            own.push(("length", int(self.length)));
        }
        if let Some(h) = self.hashes.as_ref().filter(|h| !h.is_empty()) {
            own.push(("hashes", hashes_canon(h)));
        }
        own.push(("version", int(self.version)));
        object(&self.extra, own)
    }

    /// MetaFiles.VerifyLengthHashes.
    pub fn verify(&self, data: &[u8]) -> Result<(), Error> {
        if let Some(h) = self.hashes.as_ref().filter(|h| !h.is_empty()) {
            verify_hashes(data, h)?;
        }
        if self.length != 0 {
            verify_length(data, self.length)?;
        }
        Ok(())
    }
}

/// A target file (TargetFiles).
#[derive(Debug, Clone, PartialEq)]
pub struct TargetFile {
    pub path: String,
    pub length: i64,
    pub hashes: Option<Hashes>,
    pub custom: Option<Value>,
    extra: Vec<(String, Value)>,
}

impl TargetFile {
    fn decode(path: &str, v: &Value) -> Result<Option<TargetFile>, Error> {
        if matches!(v, Value::Null) {
            return Ok(None);
        }
        let Some(o) = Obj::of(v, "Alias", "metadata.TargetFiles")? else {
            return Ok(None);
        };
        Ok(Some(TargetFile {
            path: path.to_string(),
            length: o.int("length", "int64")?,
            hashes: hashes(&o, "hashes")?,
            custom: match o.field("custom") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.clone()),
            },
            extra: o.extra(&["length", "hashes", "custom"]),
        }))
    }

    fn canon(&self) -> Canon {
        let mut own = vec![
            ("length", int(self.length)),
            (
                "hashes",
                self.hashes.as_ref().map_or(Canon::Null, |h| hashes_canon(h)),
            ),
        ];
        if let Some(c) = &self.custom {
            own.push(("custom", Canon::from_raw(c)));
        }
        object(&self.extra, own)
    }

    /// TargetFiles.VerifyLengthHashes.
    pub fn verify(&self, data: &[u8]) -> Result<(), Error> {
        let Some(h) = self.hashes.as_ref().filter(|h| !h.is_empty()) else {
            return Err(Error::LengthOrHashMismatch(
                "hashes must not be empty for target files".into(),
            ));
        };
        verify_hashes(data, h)?;
        verify_length(data, self.length)
    }
}

fn verify_length(data: &[u8], length: i64) -> Result<(), Error> {
    let got = i64::try_from(data.len()).unwrap_or(i64::MAX);
    if got != length {
        return Err(Error::LengthOrHashMismatch(format!(
            "length verification failed - expected {length}, got {got}"
        )));
    }
    Ok(())
}

/// verifyHashes: each hash, in go-tuf's map's order, which Go leaves random; here in the
/// metadata's (D104).
fn verify_hashes(data: &[u8], hashes: &[(String, Vec<u8>)]) -> Result<(), Error> {
    use aws_lc_rs::digest;
    for (alg, want) in hashes {
        let algorithm = match alg.as_str() {
            "sha256" => &digest::SHA256,
            "sha512" => &digest::SHA512,
            other => {
                return Err(Error::LengthOrHashMismatch(format!(
                    "hash verification failed - unknown hashing algorithm - {other}"
                )));
            }
        };
        if digest::digest(algorithm, data).as_ref() != want.as_slice() {
            return Err(Error::LengthOrHashMismatch(format!(
                "hash verification failed - mismatch for algorithm {alg}"
            )));
        }
    }
    Ok(())
}

/// A delegated role (DelegatedRole).
#[derive(Debug, Clone, PartialEq)]
pub struct DelegatedRole {
    pub name: String,
    pub keyids: Option<Vec<String>>,
    pub threshold: i64,
    pub terminating: bool,
    pub path_hash_prefixes: Option<Vec<String>>,
    pub paths: Option<Vec<String>>,
    extra: Vec<(String, Value)>,
}

/// Succinct hash-bin delegations (SuccinctRoles).
#[derive(Debug, Clone, PartialEq)]
pub struct SuccinctRoles {
    pub keyids: Option<Vec<String>>,
    pub threshold: i64,
    pub bit_length: i64,
    pub name_prefix: String,
    extra: Vec<(String, Value)>,
}

/// A targets role's delegations (Delegations).
#[derive(Debug, Clone, PartialEq)]
pub struct Delegations {
    pub keys: Option<Named<Key>>,
    pub roles: Option<Vec<DelegatedRole>>,
    pub succinct: Option<SuccinctRoles>,
    extra: Vec<(String, Value)>,
}

impl Delegations {
    fn decode(v: &Value) -> Result<Option<Delegations>, Error> {
        if matches!(v, Value::Null) {
            return Ok(None);
        }
        let Some(o) = Obj::of(v, "Alias", "metadata.Delegations")? else {
            return Ok(None);
        };
        let roles = match o.field("roles") {
            None | Some(Value::Null) => None,
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .map(|r| {
                        let ro = Obj::of(r, "Alias", "metadata.DelegatedRole")?;
                        Ok(match ro {
                            None => DelegatedRole {
                                name: String::new(),
                                keyids: None,
                                threshold: 0,
                                terminating: false,
                                path_hash_prefixes: None,
                                paths: None,
                                extra: Vec::new(),
                            },
                            Some(ro) => DelegatedRole {
                                name: ro.string("name")?,
                                keyids: ro.strings("keyids")?,
                                threshold: ro.int("threshold", "int")?,
                                terminating: ro.bool("terminating")?,
                                path_hash_prefixes: ro.strings("path_hash_prefixes")?,
                                paths: ro.strings("paths")?,
                                extra: ro.extra(&[
                                    "name",
                                    "keyids",
                                    "threshold",
                                    "terminating",
                                    "path_hash_prefixes",
                                    "paths",
                                ]),
                            },
                        })
                    })
                    .collect::<Result<Vec<_>, Error>>()?,
            ),
            Some(v) => return Err(type_error(v, o.name, "roles", "[]metadata.DelegatedRole")),
        };
        let succinct = match o.field("succinct_roles") {
            None | Some(Value::Null) => None,
            Some(s) => match Obj::of(s, "Alias", "metadata.SuccinctRoles")? {
                None => None,
                Some(so) => Some(SuccinctRoles {
                    keyids: so.strings("keyids")?,
                    threshold: so.int("threshold", "int")?,
                    bit_length: so.int("bit_length", "int")?,
                    name_prefix: so.string("name_prefix")?,
                    extra: so.extra(&["keyids", "threshold", "bit_length", "name_prefix"]),
                }),
            },
        };
        Ok(Some(Delegations {
            keys: keys(&o, "keys")?,
            roles,
            succinct,
            extra: o.extra(&["keys", "roles", "succinct_roles"]),
        }))
    }

    fn canon(&self) -> Result<Canon, Error> {
        let mut own = vec![("keys", keys_canon(&self.keys))];
        if let Some(roles) = &self.roles {
            let mut list = Vec::new();
            for r in roles {
                let mut fields = vec![
                    ("name", Canon::String(r.name.clone())),
                    ("keyids", strings_canon(&r.keyids)),
                    ("threshold", int(r.threshold)),
                    ("terminating", Canon::Bool(r.terminating)),
                ];
                if r.paths.is_some() && r.path_hash_prefixes.is_some() {
                    return Err(Error::Value(
                        "failed to marshal: not allowed to have both \"paths\" and \"path_hash_prefixes\" present".into(),
                    ));
                }
                if r.paths.is_some() {
                    fields.push(("paths", strings_canon(&r.paths)));
                } else if r.path_hash_prefixes.is_some() {
                    fields.push(("path_hash_prefixes", strings_canon(&r.path_hash_prefixes)));
                }
                list.push(object(&r.extra, fields));
            }
            own.push(("roles", Canon::Array(list)));
        } else if let Some(s) = &self.succinct {
            own.push((
                "succinct_roles",
                object(
                    &s.extra,
                    vec![
                        ("keyids", strings_canon(&s.keyids)),
                        ("threshold", int(s.threshold)),
                        ("bit_length", int(s.bit_length)),
                        ("name_prefix", Canon::String(s.name_prefix.clone())),
                    ],
                ),
            ));
        }
        Ok(object(&self.extra, own))
    }
}

/// What a role's metadata holds besides what all have.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Root {
        consistent_snapshot: bool,
        keys: Option<Named<Key>>,
        roles: Option<Named<Role>>,
    },
    /// A timestamp's or a snapshot's.
    Meta(Option<Named<MetaFile>>),
    Targets {
        targets: Option<Named<TargetFile>>,
        delegations: Option<Delegations>,
    },
}

/// A role's `signed` (RootType, TimestampType, SnapshotType, TargetsType).
#[derive(Debug, Clone, PartialEq)]
pub struct Signed {
    pub kind: String,
    pub spec_version: String,
    pub version: i64,
    pub expires: Option<Time>,
    pub body: Body,
    extra: Vec<(String, Value)>,
}

impl Signed {
    /// When it expires, in seconds and nanoseconds since 1970; go's zero time where it
    /// says none.
    pub fn expires_unix(&self) -> (i64, u32) {
        self.expires.as_ref().map_or((-62_135_596_800, 0), Time::unix)
    }

    /// IsExpired: whether `now` is after its expiry.
    pub fn is_expired(&self, now: (i64, u32)) -> bool {
        now > self.expires_unix()
    }

    /// cjson.EncodeCanonical of it, as go-tuf writes it: what its signatures sign.
    pub fn canonical(&self) -> Result<Vec<u8>, Error> {
        let mut own = vec![
            ("_type", Canon::String(self.kind.clone())),
            ("spec_version", Canon::String(self.spec_version.clone())),
            ("version", int(self.version)),
            ("expires", time_canon(&self.expires)?),
        ];
        match &self.body {
            Body::Root {
                consistent_snapshot,
                keys,
                roles,
            } => {
                own.push(("consistent_snapshot", Canon::Bool(*consistent_snapshot)));
                own.push(("keys", keys_canon(keys)));
                own.push((
                    "roles",
                    match roles {
                        None => Canon::Null,
                        Some(m) => Canon::Object(
                            m.iter()
                                .map(|(k, v)| (k.clone(), v.as_ref().map_or(Canon::Null, Role::canon)))
                                .collect(),
                        ),
                    },
                ));
            }
            Body::Meta(meta) => own.push((
                "meta",
                match meta {
                    None => Canon::Null,
                    Some(m) => Canon::Object(
                        m.iter()
                            .map(|(k, v)| (k.clone(), v.as_ref().map_or(Canon::Null, MetaFile::canon)))
                            .collect(),
                    ),
                },
            )),
            Body::Targets { targets, delegations } => {
                own.push((
                    "targets",
                    match targets {
                        None => Canon::Null,
                        Some(m) => Canon::Object(
                            m.iter()
                                .map(|(k, v)| (k.clone(), v.as_ref().map_or(Canon::Null, TargetFile::canon)))
                                .collect(),
                        ),
                    },
                ));
                if let Some(d) = delegations {
                    own.push(("delegations", d.canon()?));
                }
            }
        }
        let mut out = Vec::new();
        object(&self.extra, own).encode(&mut out).map_err(Error::Json)?;
        Ok(out)
    }

    pub fn meta(&self, name: &str) -> Option<&MetaFile> {
        match &self.body {
            Body::Meta(Some(m)) => m.iter().find(|(k, _)| k == name).and_then(|(_, v)| v.as_ref()),
            _ => None,
        }
    }
}

/// A signature (Signature).
#[derive(Debug, Clone, PartialEq)]
pub struct Signature {
    pub keyid: String,
    pub sig: Vec<u8>,
}

/// A role's metadata: what is signed, and its signatures.
#[derive(Debug, Clone, PartialEq)]
pub struct Metadata {
    pub signed: Signed,
    pub signatures: Vec<Signature>,
}

fn named_map<T>(
    o: &Obj<'_>,
    field: &str,
    ty: &str,
    f: impl Fn(&str, &Value) -> Result<Option<T>, Error>,
) -> Result<Option<Named<T>>, Error> {
    match o.field(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(m)) => map_of(m, f).map(Some),
        Some(v) => Err(type_error(v, o.name, field, ty)),
    }
}

impl Metadata {
    /// FromBytes for role `kind`: checkType, the typed decoding, checkUniqueSignatures.
    pub fn from_bytes(kind: &str, data: &[u8]) -> Result<Metadata, Error> {
        let v = crate::gojson::parse(data).map_err(Error::Json)?;
        // checkType, over map[string]any: exact names; null a nil map; every number read
        // as a float64.
        if !matches!(v, Value::Object(_) | Value::Null) {
            return Err(Error::Json(format!(
                "json: cannot unmarshal {} into Go value of type map[string]interface {{}}",
                v.kind()
            )));
        }
        if let Some(n) = v.first_overflow() {
            return Err(Error::Json(format!(
                "json: cannot unmarshal number {n} into Go value of type float64"
            )));
        }
        let Some(signed) = v.get("signed").filter(|s| matches!(s, Value::Object(_))) else {
            return Err(Error::Value(
                "metadata 'signed' field is missing or not an object".into(),
            ));
        };
        let Some(Value::String(t)) = signed.get("_type") else {
            return Err(Error::Value("no _type found in signed".into()));
        };
        if t != kind {
            return Err(Error::Value(format!("expected metadata type {kind}, got - {t}")));
        }
        // The typed decoding: `signed` and `signatures` matched as Go matches names.
        let Some(top) = Obj::of(&v, "", "")? else {
            return Err(Error::Value(
                "metadata 'signed' field is missing or not an object".into(),
            ));
        };
        let null = Value::Null;
        let signed_v = top.field("signed").unwrap_or(&null);
        let so = Obj::of(signed_v, "Alias", "metadata.Alias")?;
        let signed = match so {
            None => {
                return Err(Error::Value(
                    "metadata 'signed' field is missing or not an object".into(),
                ));
            }
            Some(so) => {
                let common = ["_type", "spec_version", "version", "expires"];
                let (body, own): (Body, &[&str]) = match kind {
                    ROOT => (
                        Body::Root {
                            consistent_snapshot: so.bool("consistent_snapshot")?,
                            keys: keys(&so, "keys")?,
                            roles: named_map(&so, "roles", "map[string]*metadata.Role", |_, v| {
                                Role::decode(v)
                            })?,
                        },
                        &["consistent_snapshot", "keys", "roles"],
                    ),
                    TARGETS => (
                        Body::Targets {
                            targets: named_map(
                                &so,
                                "targets",
                                "map[string]*metadata.TargetFiles",
                                TargetFile::decode,
                            )?,
                            delegations: match so.field("delegations") {
                                None | Some(Value::Null) => None,
                                Some(d) => Delegations::decode(d)?,
                            },
                        },
                        &["targets", "delegations"],
                    ),
                    _ => (
                        Body::Meta(named_map(
                            &so,
                            "meta",
                            "map[string]*metadata.MetaFiles",
                            |_, v| MetaFile::decode(v),
                        )?),
                        &["meta"],
                    ),
                };
                let mut known: Vec<&str> = common.to_vec();
                known.extend_from_slice(own);
                Signed {
                    kind: so.string("_type")?,
                    spec_version: so.string("spec_version")?,
                    version: so.int("version", "int64")?,
                    expires: so.time("expires")?,
                    body,
                    extra: so.extra(&known),
                }
            }
        };
        let signatures = match top.field("signatures") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|s| match Obj::of(s, "Alias", "metadata.Signature")? {
                    None => Ok(Signature {
                        keyid: String::new(),
                        sig: Vec::new(),
                    }),
                    Some(o) => Ok(Signature {
                        keyid: o.string("keyid")?,
                        sig: match o.field("sig") {
                            None => Vec::new(),
                            Some(v) => hex_bytes(v)?,
                        },
                    }),
                })
                .collect::<Result<Vec<_>, Error>>()?,
            Some(v) => return Err(type_error(v, "", "signatures", "[]metadata.Signature")),
        };
        // checkUniqueSignatures.
        let mut seen = std::collections::HashSet::with_capacity(signatures.len());
        for s in &signatures {
            if !seen.insert(s.keyid.as_str()) {
                return Err(Error::Value(format!(
                    "multiple signatures found for key ID {}",
                    s.keyid
                )));
            }
        }
        Ok(Metadata { signed, signatures })
    }
}
