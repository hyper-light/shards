//! A policy test's input as buildx v0.37.1 reads it (policy/tester.go lookupTestInput):
//! the `with input as` term printed as Rego prints it, decoded by encoding/json (a
//! Decoder that keeps numbers as written) into buildx's own Input types (policy/types.go),
//! and written again as encoding/json writes them: what a test is evaluated with, and
//! what its failure prints. Fields buildx's types lack are dropped, repeated members merge
//! into what came before, `null` leaves a value as it was or makes a pointer, map or
//! slice nil, and the first value of the wrong type fails the decoding in Go's words.

use std::collections::{BTreeMap, HashMap};

use shards_sigstore::godec::{Dec, Elem, GoSlice};
use shards_sigstore::tlog::gojson::{self, JValue};

use super::input::Json;

/// Each value's text, by where the value is in the decoded tree: what an Unmarshaler
/// (time.Time's) is given.
type Raw<'a> = HashMap<*const JValue, &'a [u8]>;

/// `Env`. Its maps nil where `None`, which `omitzero` tells from empty.
#[derive(Debug, Clone, Default)]
pub struct GoEnv {
    pub args: Option<BTreeMap<String, Option<String>>>,
    pub labels: Option<BTreeMap<String, String>>,
    pub filename: String,
    pub target: String,
    pub caps_request: bool,
    pub depth: i64,
}

#[derive(Debug, Clone, Default)]
pub struct GoLocal {
    pub name: String,
}

#[derive(Debug, Clone, Default)]
pub struct GoHttp {
    pub url: String,
    pub schema: String,
    pub host: String,
    pub path: String,
    pub query: Option<BTreeMap<String, Option<Vec<String>>>>,
    pub has_auth: bool,
    pub checksum: String,
}

/// A time.Time as JSON wrote it back: RFC 3339 to the nanosecond in its zone.
type GoTime = shards_dockerfile::go::Time;

#[derive(Debug, Clone, Default)]
pub struct GoActor {
    pub name: String,
    pub email: String,
    pub when: Option<GoTime>,
}

#[derive(Debug, Clone, Default)]
pub struct GoPgp {
    pub version: i64,
    pub key_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct GoSsh {
    pub version: i64,
    pub pub_key: String,
}

#[derive(Debug, Clone, Default)]
pub struct GoCommit {
    pub tree: String,
    pub parents: GoSlice<String>,
    pub author: GoActor,
    pub committer: GoActor,
    pub message: String,
    pub pgp: Option<GoPgp>,
    pub ssh: Option<GoSsh>,
}

#[derive(Debug, Clone, Default)]
pub struct GoTag {
    pub object: String,
    pub kind: String,
    pub tag: String,
    pub tagger: GoActor,
    pub message: String,
    pub pgp: Option<GoPgp>,
    pub ssh: Option<GoSsh>,
}

#[derive(Debug, Clone, Default)]
pub struct GoGit {
    pub schema: String,
    pub host: String,
    pub remote: String,
    pub full_url: String,
    pub tag_name: String,
    pub branch: String,
    pub reference: String,
    pub subdir: String,
    pub is_commit_ref: bool,
    pub is_sha256: bool,
    pub checksum: String,
    pub commit_checksum: String,
    pub is_annotated_tag: bool,
    pub tag: Option<GoTag>,
    pub commit: Option<GoCommit>,
}

#[derive(Debug, Clone, Default)]
pub struct GoConfigSource {
    pub uri: String,
    pub digest: Option<BTreeMap<String, String>>,
    pub path: String,
}

#[derive(Debug, Clone, Default)]
pub struct GoCompleteness {
    pub parameters: Option<bool>,
    pub environment: Option<bool>,
    pub materials: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct GoProvenance {
    pub predicate_type: String,
    pub build_type: String,
    pub builder_id: String,
    pub invocation_id: String,
    pub started_on: String,
    pub finished_on: String,
    pub config_source: Option<GoConfigSource>,
    pub frontend: String,
    pub build_args: Option<BTreeMap<String, String>>,
    pub raw_args: Option<BTreeMap<String, String>>,
    pub reproducible: Option<bool>,
    pub hermetic: Option<bool>,
    pub completeness: Option<GoCompleteness>,
    pub materials: GoSlice<GoInput>,
}

#[derive(Debug, Clone)]
pub struct GoTimestamp {
    pub kind: String,
    pub uri: String,
    pub timestamp: GoTime,
}

impl Default for GoTimestamp {
    fn default() -> Self {
        GoTimestamp {
            kind: String::new(),
            uri: String::new(),
            timestamp: zero_time(),
        }
    }
}

/// SignerInfo's fields, in its order: the first two written even when empty.
pub const SIGNER_FIELDS: [&str; 17] = [
    "certificateIssuer",
    "subjectAlternativeName",
    "issuer",
    "buildSignerURI",
    "buildSignerDigest",
    "runnerEnvironment",
    "sourceRepositoryURI",
    "sourceRepositoryDigest",
    "sourceRepositoryRef",
    "sourceRepositoryIdentifier",
    "sourceRepositoryOwnerURI",
    "sourceRepositoryOwnerIdentifier",
    "buildConfigURI",
    "buildConfigDigest",
    "buildTrigger",
    "runInvocationURI",
    "sourceRepositoryVisibilityAtSigning",
];

#[derive(Debug, Clone, Default)]
pub struct GoSignature {
    pub kind: String,
    pub signature_type: String,
    pub timestamps: GoSlice<GoTimestamp>,
    pub docker_reference: String,
    pub is_dhi: bool,
    pub signer: Option<[String; 17]>,
}

#[derive(Debug, Clone, Default)]
pub struct GoImage {
    pub reference: String,
    pub host: String,
    pub repo: String,
    pub full_repo: String,
    pub tag: String,
    pub platform: String,
    pub os: String,
    pub arch: String,
    pub variant: String,
    pub is_canonical: bool,
    pub checksum: String,
    pub created: String,
    pub env: GoSlice<String>,
    pub labels: Option<BTreeMap<String, String>>,
    pub user: String,
    pub volumes: GoSlice<String>,
    pub working_dir: String,
    pub has_provenance: bool,
    pub provenance: Option<Box<GoProvenance>>,
    pub signatures: GoSlice<GoSignature>,
}

/// buildx's `Input`.
#[derive(Debug, Clone, Default)]
pub struct GoInput {
    pub env: GoEnv,
    pub local: Option<GoLocal>,
    pub image: Option<GoImage>,
    pub http: Option<GoHttp>,
    pub git: Option<GoGit>,
}

/// Go's sizes of the slices' elements on a 64-bit machine, for the capacity Go's runtime
/// gives a slice that grows. No decoded value shows it: every slice here starts nil and
/// fills in order, so a slot past the longest fill is as zero as a new one.
const INPUT: Elem = Elem {
    size: 120,
    noscan: false,
};
const SIGNATURE: Elem = Elem {
    size: 96,
    noscan: false,
};
const TIMESTAMP: Elem = Elem {
    size: 56,
    noscan: false,
};

fn zero_time() -> GoTime {
    GoTime {
        year: 1,
        month: 1,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0,
        nanosecond: 0,
        offset: 0,
    }
}

/// `ast.As(term, &Input{})`: `text` (the term as Rego prints it) decoded.
pub fn decode(text: &str) -> Result<GoInput, String> {
    let (v, spans) = gojson::decode_first_raw(text.as_bytes())?;
    let mut order = Vec::new();
    gojson::pre_order(&v, &mut order);
    let raw: Raw<'_> = order
        .iter()
        .zip(&spans)
        .filter_map(|(x, &(s, e))| Some((std::ptr::from_ref(*x), text.as_bytes().get(s..e)?)))
        .collect();
    let mut out = GoInput::default();
    let mut d = Dec::new();
    input(&mut d, &v, &mut out, &raw);
    d.done(out)
}

fn input(d: &mut Dec, v: &JValue, dst: &mut GoInput, raw: &Raw<'_>) {
    let fields = [
        ("env", "env"),
        ("local", "local"),
        ("image", "image"),
        ("http", "http"),
        ("git", "git"),
    ];
    d.object(v, "Input", "policy.Input", &fields, |d, i, x| match i {
        0 => env(d, x, &mut dst.env),
        1 => d.pointer(x, &mut dst.local, |d, x, l| {
            d.object(x, "Local", "policy.Local", &[("name", "name")], |d, _, y| {
                d.string(y, &mut l.name, "string")
            })
        }),
        2 => d.pointer(x, &mut dst.image, |d, x, img| image(d, x, img, raw)),
        3 => d.pointer(x, &mut dst.http, http),
        _ => d.pointer(x, &mut dst.git, |d, x, g| git(d, x, g, raw)),
    });
}

/// A map[string]V member: nil on `null`; else members decoded each from V's zero value
/// (encoding/json's mapElem) into the map, made where nil.
fn map_of<V>(
    d: &mut Dec,
    v: &JValue,
    dst: &mut Option<BTreeMap<String, V>>,
    ty: &str,
    mut elem: impl FnMut(&mut Dec, &JValue) -> V,
) {
    match v {
        JValue::Null => *dst = None,
        JValue::Object(members) => {
            let m = dst.get_or_insert_with(BTreeMap::new);
            for (k, x) in members {
                let e = elem(d, x);
                m.insert(k.clone(), e);
            }
        }
        other => d.save(other.kind(), ty),
    }
}

fn strings_map(d: &mut Dec, v: &JValue, dst: &mut Option<BTreeMap<String, String>>) {
    map_of(d, v, dst, "map[string]string", |d, x| {
        let mut s = String::new();
        d.string(x, &mut s, "string");
        s
    });
}

fn boolean(d: &mut Dec, v: &JValue, dst: &mut bool) {
    match v {
        JValue::Bool(b) => *dst = *b,
        JValue::Null => {}
        other => d.save(other.kind(), "bool"),
    }
}

/// A *bool: nil on `null`.
fn bool_pointer(d: &mut Dec, v: &JValue, dst: &mut Option<bool>) {
    match v {
        JValue::Null => *dst = None,
        JValue::Bool(b) => *dst = Some(*b),
        other => d.save(other.kind(), "bool"),
    }
}

fn env(d: &mut Dec, v: &JValue, dst: &mut GoEnv) {
    let fields = [
        ("args", "args"),
        ("labels", "labels"),
        ("filename", "filename"),
        ("target", "target"),
        ("capsRequest", "capsRequest"),
        ("depth", "depth"),
    ];
    d.object(v, "Env", "policy.Env", &fields, |d, i, x| match i {
        0 => map_of(d, x, &mut dst.args, "map[string]*string", |d, y| match y {
            JValue::Null => None,
            _ => {
                let mut s = String::new();
                d.string(y, &mut s, "string");
                Some(s)
            }
        }),
        1 => strings_map(d, x, &mut dst.labels),
        2 => d.string(x, &mut dst.filename, "string"),
        3 => d.string(x, &mut dst.target, "string"),
        4 => boolean(d, x, &mut dst.caps_request),
        _ => d.int64(x, &mut dst.depth, "int"),
    });
}

fn http(d: &mut Dec, v: &JValue, dst: &mut GoHttp) {
    let fields = [
        ("url", "url"),
        ("schema", "schema"),
        ("host", "host"),
        ("path", "path"),
        ("query", "query"),
        ("hasAuth", "hasAuth"),
        ("checksum", "checksum"),
    ];
    d.object(v, "HTTP", "policy.HTTP", &fields, |d, i, x| match i {
        0 => d.string(x, &mut dst.url, "string"),
        1 => d.string(x, &mut dst.schema, "string"),
        2 => d.string(x, &mut dst.host, "string"),
        3 => d.string(x, &mut dst.path, "string"),
        4 => map_of(d, x, &mut dst.query, "map[string][]string", |d, y| {
            let mut s = GoSlice::default();
            d.strings(y, &mut s);
            (!matches!(y, JValue::Null)).then(|| s.into_vec())
        }),
        5 => boolean(d, x, &mut dst.has_auth),
        _ => d.string(x, &mut dst.checksum, "string"),
    });
}

/// time.Time's UnmarshalJSON over the value's own text: `null` leaves it as it was.
fn time(d: &mut Dec, v: &JValue, dst: &mut GoTime, raw: &Raw<'_>) {
    let Some(text) = raw.get(&std::ptr::from_ref(v)) else {
        return;
    };
    if *text == b"null" {
        return;
    }
    let inner = match (text.first(), text.last(), text.len()) {
        (Some(b'"'), Some(b'"'), n) if n >= 2 => text.get(1..n - 1).unwrap_or_default(),
        _ => {
            d.save_error("Time.UnmarshalJSON: input is not a JSON string".into());
            return;
        }
    };
    // parseStrictRFC3339, whose strict checks Go 1.26 leaves disabled (go.dev/issue/54580):
    // time.Parse's RFC 3339.
    match shards_dockerfile::go::parse_rfc3339(inner) {
        Ok(t) => *dst = t,
        Err(e) => d.save_error(String::from_utf8_lossy(&e).into_owned()),
    }
}

fn actor(d: &mut Dec, v: &JValue, dst: &mut GoActor, raw: &Raw<'_>) {
    let fields = [("name", "name"), ("email", "email"), ("when", "when")];
    d.object(v, "Actor", "policy.Actor", &fields, |d, i, x| match i {
        0 => d.string(x, &mut dst.name, "string"),
        1 => d.string(x, &mut dst.email, "string"),
        _ => match x {
            JValue::Null => dst.when = None,
            _ => time(d, x, dst.when.get_or_insert_with(zero_time), raw),
        },
    });
}

fn pgp(d: &mut Dec, v: &JValue, dst: &mut GoPgp) {
    let fields = [("version", "version"), ("keyID", "keyID")];
    d.object(
        v,
        "PGPSignature",
        "policy.PGPSignature",
        &fields,
        |d, i, x| match i {
            0 => d.int64(x, &mut dst.version, "int"),
            _ => d.string(x, &mut dst.key_id, "string"),
        },
    );
}

fn ssh(d: &mut Dec, v: &JValue, dst: &mut GoSsh) {
    let fields = [("version", "version"), ("pubKey", "pubKey")];
    d.object(
        v,
        "SSHSignature",
        "policy.SSHSignature",
        &fields,
        |d, i, x| match i {
            0 => d.int64(x, &mut dst.version, "int"),
            _ => d.string(x, &mut dst.pub_key, "string"),
        },
    );
}

fn commit(d: &mut Dec, v: &JValue, dst: &mut GoCommit, raw: &Raw<'_>) {
    let fields = [
        ("tree", "tree"),
        ("parents", "parents"),
        ("author", "author"),
        ("committer", "committer"),
        ("message", "message"),
        ("pgpSignature", "pgpSignature"),
        ("sshSignature", "sshSignature"),
    ];
    d.object(v, "Commit", "policy.Commit", &fields, |d, i, x| match i {
        0 => d.string(x, &mut dst.tree, "string"),
        1 => d.strings(x, &mut dst.parents),
        2 => actor(d, x, &mut dst.author, raw),
        3 => actor(d, x, &mut dst.committer, raw),
        4 => d.string(x, &mut dst.message, "string"),
        5 => d.pointer(x, &mut dst.pgp, pgp),
        _ => d.pointer(x, &mut dst.ssh, ssh),
    });
}

fn tag(d: &mut Dec, v: &JValue, dst: &mut GoTag, raw: &Raw<'_>) {
    let fields = [
        ("object", "object"),
        ("type", "type"),
        ("tag", "tag"),
        ("tagger", "tagger"),
        ("message", "message"),
        ("pgpSignature", "pgpSignature"),
        ("sshSignature", "sshSignature"),
    ];
    d.object(v, "Tag", "policy.Tag", &fields, |d, i, x| match i {
        0 => d.string(x, &mut dst.object, "string"),
        1 => d.string(x, &mut dst.kind, "string"),
        2 => d.string(x, &mut dst.tag, "string"),
        3 => actor(d, x, &mut dst.tagger, raw),
        4 => d.string(x, &mut dst.message, "string"),
        5 => d.pointer(x, &mut dst.pgp, pgp),
        _ => d.pointer(x, &mut dst.ssh, ssh),
    });
}

fn git(d: &mut Dec, v: &JValue, dst: &mut GoGit, raw: &Raw<'_>) {
    let fields = [
        ("schema", "schema"),
        ("host", "host"),
        ("remote", "remote"),
        ("fullURL", "fullURL"),
        ("tagName", "tagName"),
        ("branch", "branch"),
        ("ref", "ref"),
        ("subDir", "subDir"),
        ("isCommitRef", "isCommitRef"),
        ("isSHA256", "isSHA256"),
        ("checksum", "checksum"),
        ("commitChecksum", "commitChecksum"),
        ("isAnnotatedTag", "isAnnotatedTag"),
        ("tag", "tag"),
        ("commit", "commit"),
    ];
    d.object(v, "Git", "policy.Git", &fields, |d, i, x| match i {
        0 => d.string(x, &mut dst.schema, "string"),
        1 => d.string(x, &mut dst.host, "string"),
        2 => d.string(x, &mut dst.remote, "string"),
        3 => d.string(x, &mut dst.full_url, "string"),
        4 => d.string(x, &mut dst.tag_name, "string"),
        5 => d.string(x, &mut dst.branch, "string"),
        6 => d.string(x, &mut dst.reference, "string"),
        7 => d.string(x, &mut dst.subdir, "string"),
        8 => boolean(d, x, &mut dst.is_commit_ref),
        9 => boolean(d, x, &mut dst.is_sha256),
        10 => d.string(x, &mut dst.checksum, "string"),
        11 => d.string(x, &mut dst.commit_checksum, "string"),
        12 => boolean(d, x, &mut dst.is_annotated_tag),
        13 => d.pointer(x, &mut dst.tag, |d, x, t| tag(d, x, t, raw)),
        _ => d.pointer(x, &mut dst.commit, |d, x, c| commit(d, x, c, raw)),
    });
}

fn image(d: &mut Dec, v: &JValue, dst: &mut GoImage, raw: &Raw<'_>) {
    let fields = [
        ("ref", "ref"),
        ("host", "host"),
        ("repo", "repo"),
        ("fullRepo", "fullRepo"),
        ("tag", "tag"),
        ("platform", "platform"),
        ("os", "os"),
        ("arch", "arch"),
        ("variant", "variant"),
        ("isCanonical", "isCanonical"),
        ("checksum", "checksum"),
        ("createdTime", "createdTime"),
        ("env", "env"),
        ("labels", "labels"),
        ("user", "user"),
        ("volumes", "volumes"),
        ("workingDir", "workingDir"),
        ("hasProvenance", "hasProvenance"),
        ("provenance", "provenance"),
        ("signatures", "signatures"),
    ];
    d.object(v, "Image", "policy.Image", &fields, |d, i, x| match i {
        0 => d.string(x, &mut dst.reference, "string"),
        1 => d.string(x, &mut dst.host, "string"),
        2 => d.string(x, &mut dst.repo, "string"),
        3 => d.string(x, &mut dst.full_repo, "string"),
        4 => d.string(x, &mut dst.tag, "string"),
        5 => d.string(x, &mut dst.platform, "string"),
        6 => d.string(x, &mut dst.os, "string"),
        7 => d.string(x, &mut dst.arch, "string"),
        8 => d.string(x, &mut dst.variant, "string"),
        9 => boolean(d, x, &mut dst.is_canonical),
        10 => d.string(x, &mut dst.checksum, "string"),
        11 => d.string(x, &mut dst.created, "string"),
        12 => d.strings(x, &mut dst.env),
        13 => strings_map(d, x, &mut dst.labels),
        14 => d.string(x, &mut dst.user, "string"),
        15 => d.strings(x, &mut dst.volumes),
        16 => d.string(x, &mut dst.working_dir, "string"),
        17 => boolean(d, x, &mut dst.has_provenance),
        18 => match x {
            JValue::Null => dst.provenance = None,
            _ => provenance(d, x, dst.provenance.get_or_insert_with(Box::default), raw),
        },
        _ => d.slice(
            x,
            &mut dst.signatures,
            "[]policy.AttestationSignature",
            SIGNATURE,
            |d, y, s| signature(d, y, s, raw),
        ),
    });
}

fn provenance(d: &mut Dec, v: &JValue, dst: &mut GoProvenance, raw: &Raw<'_>) {
    let fields = [
        ("predicateType", "predicateType"),
        ("buildType", "buildType"),
        ("builderID", "builderID"),
        ("invocationID", "invocationID"),
        ("startedOn", "startedOn"),
        ("finishedOn", "finishedOn"),
        ("configSource", "configSource"),
        ("frontend", "frontend"),
        ("buildArgs", "buildArgs"),
        ("rawArgs", "rawArgs"),
        ("reproducible", "reproducible"),
        ("hermetic", "hermetic"),
        ("completeness", "completeness"),
        ("materials", "materials"),
    ];
    d.object(
        v,
        "ImageProvenance",
        "policy.ImageProvenance",
        &fields,
        |d, i, x| match i {
            0 => d.string(x, &mut dst.predicate_type, "string"),
            1 => d.string(x, &mut dst.build_type, "string"),
            2 => d.string(x, &mut dst.builder_id, "string"),
            3 => d.string(x, &mut dst.invocation_id, "string"),
            4 => d.string(x, &mut dst.started_on, "string"),
            5 => d.string(x, &mut dst.finished_on, "string"),
            6 => d.pointer(x, &mut dst.config_source, |d, x, c| {
                let fields = [("uri", "uri"), ("digest", "digest"), ("path", "path")];
                d.object(
                    x,
                    "ImageProvenanceConfigSource",
                    "policy.ImageProvenanceConfigSource",
                    &fields,
                    |d, j, y| match j {
                        0 => d.string(y, &mut c.uri, "string"),
                        1 => strings_map(d, y, &mut c.digest),
                        _ => d.string(y, &mut c.path, "string"),
                    },
                )
            }),
            7 => d.string(x, &mut dst.frontend, "string"),
            8 => strings_map(d, x, &mut dst.build_args),
            9 => strings_map(d, x, &mut dst.raw_args),
            10 => bool_pointer(d, x, &mut dst.reproducible),
            11 => bool_pointer(d, x, &mut dst.hermetic),
            12 => d.pointer(x, &mut dst.completeness, |d, x, c| {
                let fields = [
                    ("parameters", "parameters"),
                    ("environment", "environment"),
                    ("materials", "materials"),
                ];
                d.object(
                    x,
                    "ImageProvenanceCompleteness",
                    "policy.ImageProvenanceCompleteness",
                    &fields,
                    |d, j, y| match j {
                        0 => bool_pointer(d, y, &mut c.parameters),
                        1 => bool_pointer(d, y, &mut c.environment),
                        _ => bool_pointer(d, y, &mut c.materials),
                    },
                )
            }),
            _ => d.slice(x, &mut dst.materials, "[]policy.Input", INPUT, |d, y, m| {
                input(d, y, m, raw)
            }),
        },
    );
}

fn signature(d: &mut Dec, v: &JValue, dst: &mut GoSignature, raw: &Raw<'_>) {
    let fields = [
        ("kind", "kind"),
        ("type", "type"),
        ("timestamps", "timestamps"),
        ("dockerReference", "dockerReference"),
        ("isDHI", "isDHI"),
        ("signer", "signer"),
    ];
    d.object(
        v,
        "AttestationSignature",
        "policy.AttestationSignature",
        &fields,
        |d, i, x| match i {
            0 => d.string(x, &mut dst.kind, "policy.SignatureKind"),
            1 => d.string(x, &mut dst.signature_type, "policy.SignatureType"),
            2 => d.slice(
                x,
                &mut dst.timestamps,
                "[]types.TimestampVerificationResult",
                TIMESTAMP,
                |d, y, t| {
                    let fields = [("type", "type"), ("uri", "uri"), ("timestamp", "timestamp")];
                    d.object(
                        y,
                        "TimestampVerificationResult",
                        "types.TimestampVerificationResult",
                        &fields,
                        |d, j, z| match j {
                            0 => d.string(z, &mut t.kind, "string"),
                            1 => d.string(z, &mut t.uri, "string"),
                            _ => time(d, z, &mut t.timestamp, raw),
                        },
                    )
                },
            ),
            3 => d.string(x, &mut dst.docker_reference, "string"),
            4 => boolean(d, x, &mut dst.is_dhi),
            _ => d.pointer(x, &mut dst.signer, |d, x, s| {
                let fields: Vec<(&str, &str)> = SIGNER_FIELDS.iter().map(|f| (*f, *f)).collect();
                d.object(x, "SignerInfo", "policy.SignerInfo", &fields, |d, j, y| {
                    if let Some(f) = s.get_mut(j) {
                        d.string(y, f, "string");
                    }
                })
            }),
        },
    );
}

/// encoding/json's writing of buildx's types: struct fields in order, `omitempty` ones
/// left out where empty, `omitzero` ones where zero.
#[derive(Default)]
struct Out(Vec<(String, Json)>);

impl Out {
    fn str(mut self, k: &str, v: &str) -> Self {
        if !v.is_empty() {
            self.0.push((k.into(), Json::Str(v.into())));
        }
        self
    }

    fn always(mut self, k: &str, v: Json) -> Self {
        self.0.push((k.into(), v));
        self
    }

    fn flag(self, k: &str, v: bool) -> Self {
        if v { self.always(k, Json::Bool(true)) } else { self }
    }

    fn opt(self, k: &str, v: Option<Json>) -> Self {
        match v {
            Some(v) => self.always(k, v),
            None => self,
        }
    }

    fn int(self, k: &str, v: i64) -> Self {
        if v != 0 {
            self.always(k, Json::Int(v))
        } else {
            self
        }
    }

    fn strs(self, k: &str, v: &[String]) -> Self {
        if v.is_empty() {
            return self;
        }
        self.always(k, Json::Arr(v.iter().map(|s| Json::Str(s.clone())).collect()))
    }

    fn map(self, k: &str, v: &Option<BTreeMap<String, String>>) -> Self {
        match v {
            Some(m) if !m.is_empty() => self.always(
                k,
                Json::Obj(m.iter().map(|(a, b)| (a.clone(), Json::Str(b.clone()))).collect()),
            ),
            _ => self,
        }
    }

    fn done(self) -> Json {
        Json::Obj(self.0)
    }
}

/// Time.MarshalJSON's text; a time JSON cannot write (an offset of 24 hours) fails the
/// writing, in the words of encoding/json for the field's type `ty`.
fn time_json(t: &GoTime, ty: &str) -> Result<Json, String> {
    t.rfc3339_nano().map(Json::Str).map_err(|e| {
        format!(
            "json: error calling MarshalJSON for type {ty}: {}",
            String::from_utf8_lossy(&e)
        )
    })
}

impl GoEnv {
    /// `omitzero`: reflect's IsZero, where an empty map is not nil.
    fn is_zero(&self) -> bool {
        self.args.is_none()
            && self.labels.is_none()
            && self.filename.is_empty()
            && self.target.is_empty()
            && !self.caps_request
            && self.depth == 0
    }

    fn json(&self) -> Json {
        let args = self.args.as_ref().filter(|a| !a.is_empty()).map(|a| {
            Json::Obj(
                a.iter()
                    .map(|(k, v)| (k.clone(), v.as_ref().map_or(Json::Null, |s| Json::Str(s.clone()))))
                    .collect(),
            )
        });
        Out::default()
            .opt("args", args)
            .map("labels", &self.labels)
            .str("filename", &self.filename)
            .str("target", &self.target)
            .flag("capsRequest", self.caps_request)
            .always("depth", Json::Int(self.depth))
            .done()
    }
}

impl GoActor {
    fn is_zero(&self) -> bool {
        self.name.is_empty() && self.email.is_empty() && self.when.is_none()
    }

    fn json(&self) -> Result<Json, String> {
        Ok(Out::default()
            .str("name", &self.name)
            .str("email", &self.email)
            .opt(
                "when",
                self.when
                    .as_ref()
                    .map(|t| time_json(t, "*time.Time"))
                    .transpose()?,
            )
            .done())
    }
}

fn signatures_json(pgp: &Option<GoPgp>, ssh: &Option<GoSsh>, out: Out) -> Out {
    out.opt(
        "pgpSignature",
        pgp.as_ref().map(|p| {
            Out::default()
                .int("version", p.version)
                .str("keyID", &p.key_id)
                .done()
        }),
    )
    .opt(
        "sshSignature",
        ssh.as_ref().map(|s| {
            Out::default()
                .int("version", s.version)
                .str("pubKey", &s.pub_key)
                .done()
        }),
    )
}

impl GoGit {
    fn json(&self) -> Result<Json, String> {
        let actor = |a: &GoActor| -> Result<Option<Json>, String> {
            if a.is_zero() { Ok(None) } else { a.json().map(Some) }
        };
        let tag = match &self.tag {
            None => None,
            Some(t) => {
                let out = Out::default()
                    .str("object", &t.object)
                    .str("type", &t.kind)
                    .str("tag", &t.tag)
                    .opt("tagger", actor(&t.tagger)?)
                    .str("message", &t.message);
                Some(signatures_json(&t.pgp, &t.ssh, out).done())
            }
        };
        let commit = match &self.commit {
            None => None,
            Some(c) => {
                let out = Out::default()
                    .str("tree", &c.tree)
                    .strs("parents", c.parents.items())
                    .opt("author", actor(&c.author)?)
                    .opt("committer", actor(&c.committer)?)
                    .str("message", &c.message);
                Some(signatures_json(&c.pgp, &c.ssh, out).done())
            }
        };
        Ok(Out::default()
            .str("schema", &self.schema)
            .str("host", &self.host)
            .str("remote", &self.remote)
            .str("fullURL", &self.full_url)
            .str("tagName", &self.tag_name)
            .str("branch", &self.branch)
            .str("ref", &self.reference)
            .str("subDir", &self.subdir)
            .flag("isCommitRef", self.is_commit_ref)
            .flag("isSHA256", self.is_sha256)
            .str("checksum", &self.checksum)
            .str("commitChecksum", &self.commit_checksum)
            .flag("isAnnotatedTag", self.is_annotated_tag)
            .opt("tag", tag)
            .opt("commit", commit)
            .done())
    }
}

impl GoHttp {
    fn json(&self) -> Json {
        let query = self.query.as_ref().filter(|q| !q.is_empty()).map(|q| {
            Json::Obj(
                q.iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            v.as_ref().map_or(Json::Null, |v| {
                                Json::Arr(v.iter().map(|s| Json::Str(s.clone())).collect())
                            }),
                        )
                    })
                    .collect(),
            )
        });
        Out::default()
            .str("url", &self.url)
            .str("schema", &self.schema)
            .str("host", &self.host)
            .str("path", &self.path)
            .opt("query", query)
            .flag("hasAuth", self.has_auth)
            .str("checksum", &self.checksum)
            .done()
    }
}

impl GoProvenance {
    fn json(&self) -> Result<Json, String> {
        let pointer = |b: Option<bool>| b.map(Json::Bool);
        let materials = if self.materials.items().is_empty() {
            None
        } else {
            Some(Json::Arr(
                self.materials
                    .items()
                    .iter()
                    .map(GoInput::json)
                    .collect::<Result<_, _>>()?,
            ))
        };
        Ok(Out::default()
            .str("predicateType", &self.predicate_type)
            .str("buildType", &self.build_type)
            .str("builderID", &self.builder_id)
            .str("invocationID", &self.invocation_id)
            .str("startedOn", &self.started_on)
            .str("finishedOn", &self.finished_on)
            .opt(
                "configSource",
                self.config_source.as_ref().map(|c| {
                    Out::default()
                        .str("uri", &c.uri)
                        .map("digest", &c.digest)
                        .str("path", &c.path)
                        .done()
                }),
            )
            .str("frontend", &self.frontend)
            .map("buildArgs", &self.build_args)
            .map("rawArgs", &self.raw_args)
            .opt("reproducible", pointer(self.reproducible))
            .opt("hermetic", pointer(self.hermetic))
            .opt(
                "completeness",
                self.completeness.as_ref().map(|c| {
                    Out::default()
                        .opt("parameters", pointer(c.parameters))
                        .opt("environment", pointer(c.environment))
                        .opt("materials", pointer(c.materials))
                        .done()
                }),
            )
            .opt("materials", materials)
            .done())
    }
}

impl GoSignature {
    fn json(&self) -> Result<Json, String> {
        let timestamps = if self.timestamps.items().is_empty() {
            None
        } else {
            Some(Json::Arr(
                self.timestamps
                    .items()
                    .iter()
                    .map(|t| {
                        Ok(Json::Obj(vec![
                            ("type".into(), Json::Str(t.kind.clone())),
                            ("uri".into(), Json::Str(t.uri.clone())),
                            ("timestamp".into(), time_json(&t.timestamp, "time.Time")?),
                        ]))
                    })
                    .collect::<Result<_, String>>()?,
            ))
        };
        let signer = self.signer.as_ref().map(|s| {
            let mut out = Out::default();
            for (i, (k, v)) in SIGNER_FIELDS.iter().zip(s.iter()).enumerate() {
                out = if i < 2 {
                    out.always(k, Json::Str(v.clone()))
                } else {
                    out.str(k, v)
                };
            }
            out.done()
        });
        Ok(Out::default()
            .str("kind", &self.kind)
            .str("type", &self.signature_type)
            .opt("timestamps", timestamps)
            .str("dockerReference", &self.docker_reference)
            .flag("isDHI", self.is_dhi)
            .opt("signer", signer)
            .done())
    }
}

impl GoImage {
    fn json(&self) -> Result<Json, String> {
        let signatures = if self.signatures.items().is_empty() {
            None
        } else {
            Some(Json::Arr(
                self.signatures
                    .items()
                    .iter()
                    .map(GoSignature::json)
                    .collect::<Result<_, _>>()?,
            ))
        };
        Ok(Out::default()
            .str("ref", &self.reference)
            .str("host", &self.host)
            .str("repo", &self.repo)
            .str("fullRepo", &self.full_repo)
            .str("tag", &self.tag)
            .str("platform", &self.platform)
            .str("os", &self.os)
            .str("arch", &self.arch)
            .str("variant", &self.variant)
            .flag("isCanonical", self.is_canonical)
            .str("checksum", &self.checksum)
            .str("createdTime", &self.created)
            .strs("env", self.env.items())
            .map("labels", &self.labels)
            .str("user", &self.user)
            .strs("volumes", self.volumes.items())
            .str("workingDir", &self.working_dir)
            .flag("hasProvenance", self.has_provenance)
            .opt(
                "provenance",
                self.provenance.as_ref().map(|p| p.json()).transpose()?,
            )
            .opt("signatures", signatures)
            .done())
    }
}

impl GoInput {
    /// json.Marshal of the Input: how a test's input is evaluated and printed.
    pub fn json(&self) -> Result<Json, String> {
        Ok(Out::default()
            .opt("env", (!self.env.is_zero()).then(|| self.env.json()))
            .opt(
                "local",
                self.local
                    .as_ref()
                    .map(|l| Out::default().str("name", &l.name).done()),
            )
            .opt("image", self.image.as_ref().map(GoImage::json).transpose()?)
            .opt("http", self.http.as_ref().map(GoHttp::json))
            .opt("git", self.git.as_ref().map(GoGit::json).transpose()?)
            .done())
    }

    /// Whether its Env says anything of the build (hasEnv).
    pub fn has_env(&self) -> bool {
        !self.env.filename.is_empty()
            || !self.env.target.is_empty()
            || self.env.args.as_ref().is_some_and(|a| !a.is_empty())
            || self.env.labels.as_ref().is_some_and(|l| !l.is_empty())
    }
}

impl GoEnv {
    /// The Env as the policy machinery holds one.
    pub fn to_env(&self) -> super::input::Env {
        super::input::Env {
            args: self.args.clone().unwrap_or_default(),
            labels: self.labels.clone().unwrap_or_default(),
            filename: self.filename.clone(),
            target: self.target.clone(),
            caps_request: self.caps_request,
            depth: self.depth,
        }
    }
}
