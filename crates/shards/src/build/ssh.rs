//! Git over SSH, as git reaches a repository with `ssh` (connect.c: `git-upload-pack
//! 'PATH'` run on the host, `GIT_PROTOCOL=version=2` sent as git sends it with
//! `-o SendEnv=GIT_PROTOCOL`): an SSH client of its own (D69), which takes no `ssh` binary.
//!
//! - The transport (RFC 4253): `mlkem768x25519-sha256` (draft-ietf-sshm-mlkem-hybrid-kex,
//!   OpenSSH 10.0's default) or `curve25519-sha256` (RFC 8731); the server's host key
//!   verified by its signature (Ed25519, ECDSA, RSA with SHA-2) and found in the
//!   known_hosts files OpenSSH reads; AEAD ciphers alone (`chacha20-poly1305@openssh.com`,
//!   `aes256-gcm@openssh.com`, `aes128-gcm@openssh.com`, RFC 5647); strict key exchange
//!   (`kex-strict-c-v00@openssh.com`, OpenSSH PROTOCOL §1.10), against Terrapin
//!   (CVE-2023-48795); rekeying when the server asks.
//! - Authentication (RFC 4252 §7): `publickey`, each of the build's agent's keys in turn,
//!   signed by that agent; RSA with SHA-2 alone (RFC 8332).
//! - Channels (RFC 4254): a session for each request, as git's own transport takes a
//!   connection for each (crate::git's `Daemon`); one connection for all of a fetch's.
//!
//! Better than BuildKit's: BuildKit scans the host's keys while planning and trusts
//! whatever it is shown (llb.Git, `sshutil.SSHKeyScan`); shards trusts only the keys the
//! user's known_hosts holds, and refuses a host whose key it does not know or has changed.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use aws_lc_rs::aead::{self, chacha20_poly1305_openssh as chacha};
use aws_lc_rs::{agreement, digest, hmac, kem, rand as lc_rand, signature};
use shards_cmdline::buildflags::Agent;
use shards_git::remote::Transport;
use zeroize::Zeroizing;

use super::sshkey::{self, Key};

/// Message numbers (RFC 4250 §4.1).
mod msg {
    pub const DISCONNECT: u8 = 1;
    pub const IGNORE: u8 = 2;
    pub const UNIMPLEMENTED: u8 = 3;
    pub const DEBUG: u8 = 4;
    pub const SERVICE_REQUEST: u8 = 5;
    pub const SERVICE_ACCEPT: u8 = 6;
    pub const EXT_INFO: u8 = 7;
    pub const KEXINIT: u8 = 20;
    pub const NEWKEYS: u8 = 21;
    pub const KEX_ECDH_INIT: u8 = 30;
    pub const KEX_ECDH_REPLY: u8 = 31;
    pub const USERAUTH_REQUEST: u8 = 50;
    pub const USERAUTH_FAILURE: u8 = 51;
    pub const USERAUTH_SUCCESS: u8 = 52;
    pub const USERAUTH_BANNER: u8 = 53;
    pub const GLOBAL_REQUEST: u8 = 80;
    pub const REQUEST_FAILURE: u8 = 82;
    pub const CHANNEL_OPEN: u8 = 90;
    pub const CHANNEL_OPEN_CONFIRMATION: u8 = 91;
    pub const CHANNEL_OPEN_FAILURE: u8 = 92;
    pub const CHANNEL_WINDOW_ADJUST: u8 = 93;
    pub const CHANNEL_DATA: u8 = 94;
    pub const CHANNEL_EXTENDED_DATA: u8 = 95;
    pub const CHANNEL_EOF: u8 = 96;
    pub const CHANNEL_CLOSE: u8 = 97;
    pub const CHANNEL_REQUEST: u8 = 98;
    pub const CHANNEL_SUCCESS: u8 = 99;
    pub const CHANNEL_FAILURE: u8 = 100;
}

const KEX_MLKEM: &str = "mlkem768x25519-sha256";
const KEX_CURVE: &str = "curve25519-sha256";
const KEX_CURVE_OLD: &str = "curve25519-sha256@libssh.org";
const KEX_ALGORITHMS: &[&str] = &[KEX_MLKEM, KEX_CURVE, KEX_CURVE_OLD];
const CIPHERS: &[&str] = &[
    "chacha20-poly1305@openssh.com",
    "aes256-gcm@openssh.com",
    "aes128-gcm@openssh.com",
];
/// Offered for servers that negotiate a MAC whatever the cipher; an AEAD cipher uses none
/// (RFC 5647 §5.1, OpenSSH PROTOCOL.chacha20poly1305).
const MACS: &[&str] = &["hmac-sha2-256-etm@openssh.com", "hmac-sha2-512-etm@openssh.com"];
const HOST_KEY_ALGORITHMS: &[&str] = &[
    "ssh-ed25519",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "rsa-sha2-512",
    "rsa-sha2-256",
];

/// The largest packet taken (OpenSSH's PACKET_MAX_SIZE).
const PACKET_MAX: usize = 256 * 1024;
/// What a channel may be sent before it says it has room for more, and the most of it in
/// one packet (OpenSSH's CHAN_SES_WINDOW_DEFAULT and CHAN_SES_PACKET_DEFAULT).
const WINDOW: u32 = 2 * 1024 * 1024;
const MAX_PACKET: u32 = 32 * 1024;
/// The version shards says it speaks, and the longest line of the server's taken before
/// its own (RFC 4253 §4.2).
const LINE_MAX: usize = 255;

/// Where a repository is reached: `ssh://[USER@]HOST[:PORT]/PATH`, or scp's
/// `[USER@]HOST:PATH`, as git reads them (connect.c parse_connect_url).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub user: String,
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// Whether `url` names a repository over SSH, as git tells (transport.c, connect.c).
pub fn is_ssh(url: &str) -> bool {
    ["ssh://", "git+ssh://", "ssh+git://"]
        .iter()
        .any(|p| url.starts_with(p))
        || scp_like(url).is_some()
}

/// scp's form: no `://`, and a `:` before any `/` (a host, then its path); `[...]` holds
/// a host with colons.
fn scp_like(url: &str) -> Option<(&str, &str)> {
    if url.contains("://") {
        return None;
    }
    let colon = if url.starts_with('[') || url.contains("@[") {
        let close = url.find(']')?;
        close + url.get(close..)?.find(':')?
    } else {
        url.find(':')?
    };
    let (host, path) = (url.get(..colon)?, url.get(colon + 1..)?);
    if host.contains('/') || host.is_empty() {
        return None;
    }
    Some((host, path))
}

impl Target {
    /// The target `url` names; a user not named is `local_user`, as ssh takes it.
    pub fn of_url(url: &str, local_user: &str) -> Result<Target, String> {
        let (authority, path) = match ["ssh://", "git+ssh://", "ssh+git://"]
            .iter()
            .find_map(|p| url.strip_prefix(p))
        {
            Some(rest) => {
                let slash = rest.find('/').ok_or_else(|| format!("{url}: no path"))?;
                let (authority, path) = rest.split_at(slash);
                // `/~user/...` is `~user/...`, relative to that user's home.
                let path = if path.starts_with("/~") {
                    path.get(1..).unwrap_or_default()
                } else {
                    path
                };
                (authority, path)
            }
            None => scp_like(url).ok_or_else(|| format!("{url}: not an SSH URL"))?,
        };
        let (user, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (u.to_string(), h),
            None => (local_user.to_string(), authority),
        };
        let scp = !url.contains("://");
        let (host, port) = if let Some(inner) = hostport.strip_prefix('[') {
            let (h, rest) = inner
                .split_once(']')
                .ok_or_else(|| format!("{url}: an unclosed ["))?;
            match rest.strip_prefix(':') {
                Some(p) if !scp => (h, p),
                _ => (h, ""),
            }
        } else if scp {
            (hostport, "")
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h, p),
                None => (hostport, ""),
            }
        };
        let port = if port.is_empty() {
            22
        } else {
            port.parse().map_err(|_| format!("{url}: a bad port {port}"))?
        };
        if host.is_empty() || path.is_empty() {
            return Err(format!("{url}: no host or path"));
        }
        if host.starts_with('-') || user.starts_with('-') {
            return Err(format!("strange hostname '{host}' blocked"));
        }
        Ok(Target {
            user,
            host: host.to_ascii_lowercase(),
            port,
            path: path.to_string(),
        })
    }

    /// The command git runs: `git-upload-pack 'PATH'`, the path quoted as git's
    /// sq_quote_buf quotes it.
    fn command(&self) -> String {
        let mut quoted = String::from("'");
        for c in self.path.chars() {
            match c {
                '\'' => quoted.push_str("'\\''"),
                '!' => quoted.push_str("'\\!'"),
                c => quoted.push(c),
            }
        }
        quoted.push('\'');
        format!("git-upload-pack {quoted}")
    }

    /// The name known_hosts knows the host by: `HOST`, or `[HOST]:PORT` off port 22.
    fn known_name(&self) -> String {
        if self.port == 22 {
            self.host.clone()
        } else {
            format!("[{}]:{}", self.host, self.port)
        }
    }
}

/// What the known_hosts files say of a host.
#[derive(Debug, Default)]
pub struct Known {
    /// The keys it is known by.
    keys: Vec<Vec<u8>>,
    /// Keys revoked (`@revoked`), for any host.
    revoked: Vec<Vec<u8>>,
}

/// The files OpenSSH reads known hosts from (ssh_config(5) UserKnownHostsFile,
/// GlobalKnownHostsFile): the user's, then the system's.
pub fn known_hosts_files() -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        let ssh = std::path::Path::new(&home).join(".ssh");
        files.push(ssh.join("known_hosts"));
        files.push(ssh.join("known_hosts2"));
    }
    files.push("/etc/ssh/ssh_known_hosts".into());
    files.push("/etc/ssh/ssh_known_hosts2".into());
    files
}

impl Known {
    /// What `texts` (known_hosts files, sshd(8) "SSH_KNOWN_HOSTS FILE FORMAT") say of
    /// `name`: its keys, and keys revoked.
    pub fn of(name: &str, texts: &[Vec<u8>]) -> Known {
        let mut known = Known::default();
        for text in texts {
            for line in text.split(|b| *b == b'\n') {
                let line = String::from_utf8_lossy(line);
                let mut fields = line.split_ascii_whitespace();
                let Some(mut first) = fields.next() else { continue };
                if first.starts_with('#') {
                    continue;
                }
                let marker = first.strip_prefix('@').map(str::to_string);
                if marker.is_some() {
                    let Some(f) = fields.next() else { continue };
                    first = f;
                }
                let (Some(_kind), Some(key)) = (fields.next(), fields.next()) else {
                    continue;
                };
                let Some(blob) = base64_decode(key) else { continue };
                match marker.as_deref() {
                    Some("revoked") => known.revoked.push(blob),
                    // Host certificates' authorities: shards takes no certificates.
                    Some(_) => {}
                    None if host_matches(first, name) => known.keys.push(blob),
                    None => {}
                }
            }
        }
        known
    }

    /// The host key algorithms to offer, those of keys known for the host first, as
    /// OpenSSH orders them (sshconnect2.c order_hostkeyalgs).
    fn algorithms(&self) -> Vec<&'static str> {
        let known_types: Vec<Vec<u8>> = self
            .keys
            .iter()
            .filter_map(|k| Fields(k).string().map(<[u8]>::to_vec))
            .collect();
        let of_known = |alg: &str| {
            let kind = if alg.starts_with("rsa-sha2-") {
                "ssh-rsa"
            } else {
                alg
            };
            known_types.iter().any(|t| t == kind.as_bytes())
        };
        let mut algs: Vec<&'static str> = HOST_KEY_ALGORITHMS
            .iter()
            .copied()
            .filter(|a| of_known(a))
            .collect();
        algs.extend(HOST_KEY_ALGORITHMS.iter().copied().filter(|a| !of_known(a)));
        algs
    }

    /// Whether `blob`, the key the host proved it holds, is one it is known by.
    fn check(&self, name: &str, blob: &[u8]) -> Result<(), String> {
        let shown = || {
            let kind = Fields(blob)
                .string()
                .map(String::from_utf8_lossy)
                .unwrap_or_default();
            format!("{kind} key {}", fingerprint(blob))
        };
        if self.revoked.iter().any(|k| k == blob) {
            return Err(format!(
                "Host key verification failed: {name}'s {} is revoked in known_hosts",
                shown()
            ));
        }
        if self.keys.iter().any(|k| k == blob) {
            return Ok(());
        }
        let kind = Fields(blob).string().unwrap_or_default();
        if self.keys.iter().any(|k| Fields(k).string() == Some(kind)) {
            return Err(format!(
                "Host key verification failed: {name}'s host key has changed: known_hosts holds another key of its type; it showed the {}",
                shown()
            ));
        }
        Err(format!(
            "Host key verification failed: {name} is not in known_hosts ({}); it showed the {}. Check the fingerprint with the host's owner, then add its key (ssh-keyscan {name})",
            known_hosts_files()
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            shown()
        ))
    }
}

/// A key's fingerprint as `ssh-keygen -l` gives it: `SHA256:` and its digest in base64,
/// unpadded.
fn fingerprint(blob: &[u8]) -> String {
    use base64::Engine as _;
    let d = digest::digest(&digest::SHA256, blob);
    format!(
        "SHA256:{}",
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(d.as_ref())
    )
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// Whether a known_hosts host field names `name` (OpenSSH match_hostname): its patterns
/// separated by commas, `*` and `?` wildcards, `!` negating; or one hashed name,
/// `|1|SALT|HASH`, HMAC-SHA1 of the name under the salt.
fn host_matches(patterns: &str, name: &str) -> bool {
    if let Some(hashed) = patterns.strip_prefix("|1|") {
        let Some((salt, hash)) = hashed.split_once('|') else {
            return false;
        };
        let (Some(salt), Some(hash)) = (base64_decode(salt), base64_decode(hash)) else {
            return false;
        };
        let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &salt);
        return hmac::verify(&key, name.as_bytes(), &hash).is_ok();
    }
    let mut matched = false;
    for p in patterns.split(',') {
        let (negated, p) = match p.strip_prefix('!') {
            Some(p) => (true, p),
            None => (false, p),
        };
        if wildcard(p.to_ascii_lowercase().as_bytes(), name.as_bytes()) {
            if negated {
                return false;
            }
            matched = true;
        }
    }
    matched
}

/// `*` (any run) and `?` (any one), as OpenSSH's match_pattern.
fn wildcard(p: &[u8], s: &[u8]) -> bool {
    match (p.split_first(), s.split_first()) {
        (None, None) => true,
        (Some((b'*', rest)), _) => wildcard(rest, s) || s.split_first().is_some_and(|(_, t)| wildcard(p, t)),
        (Some((b'?', rest)), Some((_, t))) => wildcard(rest, t),
        (Some((a, rest)), Some((b, t))) if a == b => wildcard(rest, t),
        _ => false,
    }
}

/// SSH wire fields (RFC 4251 §5), read.
struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    fn byte(&mut self) -> Option<u8> {
        let (b, rest) = self.0.split_first()?;
        self.0 = rest;
        Some(*b)
    }

    fn u32(&mut self) -> Option<u32> {
        let (n, rest) = self.0.split_first_chunk::<4>()?;
        self.0 = rest;
        Some(u32::from_be_bytes(*n))
    }

    fn string(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.u32()?).ok()?;
        let (s, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(s)
    }

    fn bool(&mut self) -> Option<bool> {
        self.byte().map(|b| b != 0)
    }

    fn name_list(&mut self) -> Option<Vec<String>> {
        let s = self.string()?;
        Some(if s.is_empty() {
            Vec::new()
        } else {
            String::from_utf8_lossy(s)
                .split(',')
                .map(str::to_string)
                .collect()
        })
    }
}

/// SSH wire fields, written.
#[derive(Default)]
struct Out(Vec<u8>);

impl Out {
    fn of(kind: u8) -> Out {
        Out(vec![kind])
    }

    fn byte(mut self, b: u8) -> Out {
        self.0.push(b);
        self
    }

    fn u32(mut self, n: u32) -> Out {
        self.0.extend_from_slice(&n.to_be_bytes());
        self
    }

    fn string(mut self, s: &[u8]) -> Out {
        let n = u32::try_from(s.len()).unwrap_or(u32::MAX);
        self.0.extend_from_slice(&n.to_be_bytes());
        self.0.extend_from_slice(s);
        self
    }

    fn mpint(self, n: &[u8]) -> Out {
        let zeros = n.iter().take_while(|b| **b == 0).count();
        let n = n.get(zeros..).unwrap_or_default();
        if n.first().is_some_and(|b| b & 0x80 != 0) {
            self.string(&[&[0], n].concat())
        } else {
            self.string(n)
        }
    }

    fn names(self, names: &[&str]) -> Out {
        self.string(names.join(",").as_bytes())
    }
}

/// A direction's cipher, once keys are in force.
enum Cipher {
    Chacha {
        seal: Option<chacha::SealingKey>,
        open: Option<chacha::OpeningKey>,
    },
    Gcm {
        key: aead::LessSafeKey,
        /// RFC 5647 §7.1: 4 bytes fixed, then an 8-byte invocation counter.
        iv: [u8; 12],
    },
}

impl Cipher {
    fn new(name: &str, key: &[u8], iv: &[u8], sealing: bool) -> Result<Cipher, String> {
        match name {
            "chacha20-poly1305@openssh.com" => {
                let k: &[u8; 64] = key
                    .get(..64)
                    .and_then(|k| k.try_into().ok())
                    .ok_or("a short chacha20-poly1305 key")?;
                Ok(if sealing {
                    Cipher::Chacha {
                        seal: Some(chacha::SealingKey::new(k)),
                        open: None,
                    }
                } else {
                    Cipher::Chacha {
                        seal: None,
                        open: Some(chacha::OpeningKey::new(k)),
                    }
                })
            }
            "aes256-gcm@openssh.com" | "aes128-gcm@openssh.com" => {
                let (alg, len) = if name.starts_with("aes256") {
                    (&aead::AES_256_GCM, 32)
                } else {
                    (&aead::AES_128_GCM, 16)
                };
                let unbound = aead::UnboundKey::new(alg, key.get(..len).ok_or("a short AES key")?)
                    .map_err(|_| "a bad AES key")?;
                let iv: [u8; 12] = iv.get(..12).and_then(|i| i.try_into().ok()).ok_or("a short IV")?;
                Ok(Cipher::Gcm {
                    key: aead::LessSafeKey::new(unbound),
                    iv,
                })
            }
            other => Err(format!("no cipher {other}")),
        }
    }

    /// The block packets are padded to.
    fn block(&self) -> usize {
        match self {
            Cipher::Chacha { .. } => 8,
            Cipher::Gcm { .. } => 16,
        }
    }

    fn next_gcm_nonce(iv: &mut [u8; 12]) -> aead::Nonce {
        let nonce = aead::Nonce::assume_unique_for_key(*iv);
        let mut counter = [0u8; 8];
        counter.copy_from_slice(iv.get(4..).unwrap_or(&[0; 8]));
        let next = u64::from_be_bytes(counter).wrapping_add(1).to_be_bytes();
        if let Some(c) = iv.get_mut(4..) {
            c.copy_from_slice(&next);
        }
        nonce
    }
}

/// Keys to put in force at NEWKEYS: each direction's.
struct NewKeys {
    out: Cipher,
    inn: Cipher,
}

/// An SSH connection: its transport, authenticated, and the channels opened on it in turn.
struct Conn {
    tcp: TcpStream,
    seq_out: u32,
    seq_in: u32,
    out: Option<Cipher>,
    inn: Option<Cipher>,
    session_id: Vec<u8>,
    v_c: Vec<u8>,
    v_s: Vec<u8>,
    /// The host key the first exchange proved, which a rekeying must prove again.
    host_key: Vec<u8>,
    host_algs: Vec<&'static str>,
    strict: bool,
    /// Signature algorithms the server takes (EXT_INFO server-sig-algs).
    sig_algs: Option<Vec<String>>,
    next_channel: u32,
    /// The channel in use: its numbers, the room the server has given it, its most in a
    /// packet, the room this side has left to give, and whether the server has ended it.
    channel: Option<Channel>,
    /// What the channel in use has sent and is not read yet.
    pending: Vec<u8>,
}

#[derive(Debug)]
struct Channel {
    ours: u32,
    theirs: u32,
    room: u32,
    max_packet: u32,
    window_left: u32,
    eof: bool,
    closed: bool,
    exit: Option<u32>,
    stderr: Vec<u8>,
}

fn rand_bytes(n: usize) -> Result<Vec<u8>, String> {
    let mut b = vec![0u8; n];
    lc_rand::fill(&mut b).map_err(|_| "no randomness")?;
    Ok(b)
}

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut c = digest::Context::new(&digest::SHA256);
    for p in parts {
        c.update(p);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(c.finish().as_ref());
    out
}

impl Conn {
    fn connect(target: &Target, known: &Known, patience: Duration) -> Result<Conn, String> {
        let at = format!("{}:{}", target.host, target.port);
        let addrs = std::net::ToSocketAddrs::to_socket_addrs(&(target.host.as_str(), target.port))
            .map_err(|e| format!("ssh: Could not resolve hostname {}: {e}", target.host))?;
        let mut last = format!(
            "ssh: connect to host {} port {}: no address",
            target.host, target.port
        );
        let mut tcp = None;
        for a in addrs {
            match TcpStream::connect_timeout(&a, patience) {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) => last = format!("ssh: connect to host {} port {}: {e}", target.host, target.port),
            }
        }
        let tcp = tcp.ok_or(last)?;
        tcp.set_read_timeout(Some(patience))
            .map_err(|e| format!("{at}: {e}"))?;
        tcp.set_nodelay(true).map_err(|e| format!("{at}: {e}"))?;
        let mut conn = Conn {
            tcp,
            seq_out: 0,
            seq_in: 0,
            out: None,
            inn: None,
            session_id: Vec::new(),
            v_c: format!("SSH-2.0-shards_{}", env!("CARGO_PKG_VERSION")).into_bytes(),
            v_s: Vec::new(),
            host_key: Vec::new(),
            host_algs: known.algorithms(),
            strict: false,
            sig_algs: None,
            next_channel: 0,
            channel: None,
            pending: Vec::new(),
        };
        let line = [conn.v_c.as_slice(), b"\r\n"].concat();
        conn.tcp.write_all(&line).map_err(|e| format!("{at}: {e}"))?;
        conn.v_s = conn.version().map_err(|e| format!("{at}: {e}"))?;
        conn.kex(None, known, &target.known_name())?;
        Ok(conn)
    }

    /// The server's version line (RFC 4253 §4.2): lines before it skipped.
    fn version(&mut self) -> Result<Vec<u8>, String> {
        for _ in 0..1024 {
            let mut line = Vec::new();
            loop {
                let mut b = [0u8; 1];
                self.tcp
                    .read_exact(&mut b)
                    .map_err(|e| format!("the server's version: {e}"))?;
                if b[0] == b'\n' {
                    break;
                }
                line.push(b[0]);
                if line.len() > LINE_MAX * 4 {
                    return Err("the server's version line is too long".into());
                }
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.starts_with(b"SSH-") {
                if !(line.starts_with(b"SSH-2.0-") || line.starts_with(b"SSH-1.99-")) || line.len() > LINE_MAX
                {
                    return Err(format!(
                        "the server speaks {}, not SSH 2",
                        String::from_utf8_lossy(&line)
                    ));
                }
                return Ok(line);
            }
        }
        Err("the server sent no version".into())
    }

    fn send(&mut self, payload: &[u8]) -> Result<(), String> {
        let block = self.out.as_ref().map_or(8, Cipher::block);
        // The length field is outside what the AEAD ciphers pad to their block.
        let counted = if self.out.is_some() {
            1 + payload.len()
        } else {
            5 + payload.len()
        };
        let mut pad = block - counted % block;
        if pad < 4 {
            pad += block;
        }
        let len = 1 + payload.len() + pad;
        let mut packet = Zeroizing::new(Vec::with_capacity(4 + len + 16));
        packet.extend_from_slice(&u32::try_from(len).map_err(|_| "a packet too long")?.to_be_bytes());
        packet.push(u8::try_from(pad).map_err(|_| "padding too long")?);
        packet.extend_from_slice(payload);
        packet.extend_from_slice(&rand_bytes(pad)?);
        match &mut self.out {
            None => {}
            Some(Cipher::Chacha { seal: Some(k), .. }) => {
                let mut tag = [0u8; 16];
                k.seal_in_place(self.seq_out, &mut packet, &mut tag);
                packet.extend_from_slice(&tag);
            }
            Some(Cipher::Gcm { key, iv }) => {
                let nonce = Cipher::next_gcm_nonce(iv);
                let (head, body) = packet.split_at_mut(4);
                let tag = key
                    .seal_in_place_separate_tag(nonce, aead::Aad::from(&*head), body)
                    .map_err(|_| "sealing a packet failed")?;
                packet.extend_from_slice(tag.as_ref());
            }
            Some(Cipher::Chacha { seal: None, .. }) => return Err("no sealing key".into()),
        }
        self.tcp.write_all(&packet).map_err(|e| format!("ssh: {e}"))?;
        self.seq_out = self.seq_out.wrapping_add(1);
        Ok(())
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), String> {
        self.tcp.read_exact(buf).map_err(|e| match e.kind() {
            std::io::ErrorKind::UnexpectedEof => "ssh: the server closed the connection".to_string(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                "ssh: the server stopped answering".to_string()
            }
            _ => format!("ssh: {e}"),
        })
    }

    /// The next packet's payload, whatever it is.
    fn recv_raw(&mut self) -> Result<Zeroizing<Vec<u8>>, String> {
        let mut head = [0u8; 4];
        self.read_exact(&mut head)?;
        let check = |len: usize, block: usize| -> Result<(), String> {
            if !(5..=PACKET_MAX).contains(&len) || !len.is_multiple_of(block) {
                return Err(format!("ssh: a packet of a bad length ({len})"));
            }
            Ok(())
        };
        let seq = self.seq_in;
        let body = match &mut self.inn {
            None => {
                let len = usize::try_from(u32::from_be_bytes(head)).unwrap_or(usize::MAX);
                if !(5..=PACKET_MAX).contains(&len) || !(len + 4).is_multiple_of(8) {
                    return Err(format!("ssh: a packet of a bad length ({len})"));
                }
                let mut body = Zeroizing::new(vec![0u8; len]);
                self.tcp.read_exact(&mut body).map_err(|e| format!("ssh: {e}"))?;
                body
            }
            Some(Cipher::Chacha { open: Some(k), .. }) => {
                let len = u32::from_be_bytes(k.decrypt_packet_length(seq, head));
                let len = usize::try_from(len).unwrap_or(usize::MAX);
                check(len, 8)?;
                let mut all = Zeroizing::new(vec![0u8; 4 + len + 16]);
                if let Some(h) = all.get_mut(..4) {
                    h.copy_from_slice(&head);
                }
                self.tcp
                    .read_exact(all.get_mut(4..).unwrap_or_default())
                    .map_err(|e| format!("ssh: {e}"))?;
                let (data, tag) = all.split_at_mut(4 + len);
                let tag: &[u8; 16] = (&*tag).try_into().map_err(|_| "a short tag")?;
                let plain = k
                    .open_in_place(seq, data, tag)
                    .map_err(|_| "ssh: a packet failed its authentication")?;
                Zeroizing::new(plain.to_vec())
            }
            Some(Cipher::Gcm { key, iv }) => {
                let len = usize::try_from(u32::from_be_bytes(head)).unwrap_or(usize::MAX);
                check(len, 16)?;
                let mut body = Zeroizing::new(vec![0u8; len + 16]);
                self.tcp.read_exact(&mut body).map_err(|e| format!("ssh: {e}"))?;
                let nonce = Cipher::next_gcm_nonce(iv);
                let plain = key
                    .open_in_place(nonce, aead::Aad::from(head), &mut body)
                    .map_err(|_| "ssh: a packet failed its authentication")?;
                Zeroizing::new(plain.to_vec())
            }
            Some(Cipher::Chacha { open: None, .. }) => return Err("no opening key".into()),
        };
        self.seq_in = self.seq_in.wrapping_add(1);
        let pad = usize::from(*body.first().ok_or("an empty packet")?);
        if pad < 4 || pad + 1 > body.len() {
            return Err("ssh: a packet's padding is bad".into());
        }
        Ok(Zeroizing::new(
            body.get(1..body.len() - pad).unwrap_or_default().to_vec(),
        ))
    }

    /// The next packet that is not the transport's own: those answered, a rekeying done,
    /// a disconnect an error.
    fn recv(&mut self) -> Result<Zeroizing<Vec<u8>>, String> {
        loop {
            let p = self.recv_raw()?;
            match p.first().copied() {
                Some(msg::IGNORE | msg::DEBUG | msg::UNIMPLEMENTED) => {}
                Some(msg::DISCONNECT) => {
                    let mut f = Fields(p.get(1..).unwrap_or_default());
                    let _ = f.u32();
                    let why = f.string().map(String::from_utf8_lossy).unwrap_or_default();
                    return Err(format!("ssh: the server disconnected: {why}"));
                }
                Some(msg::KEXINIT) => self.kex(Some(p.to_vec()), &Known::default(), "")?,
                Some(msg::EXT_INFO) => self.ext_info(&p),
                Some(msg::GLOBAL_REQUEST) => {
                    let mut f = Fields(p.get(1..).unwrap_or_default());
                    let _ = f.string();
                    if f.bool() == Some(true) {
                        self.send(&[msg::REQUEST_FAILURE])?;
                    }
                }
                _ => return Ok(p),
            }
        }
    }

    fn ext_info(&mut self, p: &[u8]) {
        let mut f = Fields(p.get(1..).unwrap_or_default());
        let n = f.u32().unwrap_or(0);
        for _ in 0..n {
            let (Some(name), Some(value)) = (f.string(), f.string()) else {
                break;
            };
            if name == b"server-sig-algs" {
                self.sig_algs = Some(
                    String::from_utf8_lossy(value)
                        .split(',')
                        .map(str::to_string)
                        .collect(),
                );
            }
        }
    }

    /// A key exchange (RFC 4253 §7): the first, or a rekeying the server began with
    /// `theirs`, its KEXINIT. The host key is checked against `known` the first time,
    /// and must be the same after.
    fn kex(&mut self, theirs: Option<Vec<u8>>, known: &Known, name: &str) -> Result<(), String> {
        let first = self.session_id.is_empty();
        let mut kex_algs: Vec<&str> = KEX_ALGORITHMS.to_vec();
        if first {
            kex_algs.extend(["ext-info-c", "kex-strict-c-v00@openssh.com"]);
        }
        let mut ours = Out::of(msg::KEXINIT);
        ours.0.extend_from_slice(&rand_bytes(16)?);
        let i_c = ours
            .names(&kex_algs)
            .names(&self.host_algs)
            .names(CIPHERS)
            .names(CIPHERS)
            .names(MACS)
            .names(MACS)
            .names(&["none"])
            .names(&["none"])
            .names(&[])
            .names(&[])
            .byte(0)
            .u32(0)
            .0;
        self.send(&i_c)?;
        // Under strict exchange the server's first packet must be its KEXINIT: what came
        // before it is counted, and refused once its KEXINIT says it is strict.
        let mut before = 0;
        let i_s = match theirs {
            Some(t) => t,
            None => loop {
                let p = self.recv_raw()?;
                match p.first().copied() {
                    Some(msg::KEXINIT) => break p.to_vec(),
                    Some(msg::IGNORE | msg::DEBUG) => before += 1,
                    _ => return Err("ssh: the server's first packet is not KEXINIT".into()),
                }
            },
        };
        let mut f = Fields(i_s.get(17..).ok_or("a short KEXINIT")?);
        let lists: Vec<Vec<String>> = (0..10).map(|_| f.name_list().unwrap_or_default()).collect();
        let follows = f.bool().unwrap_or(false);
        let pick = |client: &[&str], server: &[String]| -> Option<String> {
            client
                .iter()
                .find(|c| server.iter().any(|s| s == *c))
                .map(|s| s.to_string())
        };
        let server_kex = lists.first().cloned().unwrap_or_default();
        if first && server_kex.iter().any(|a| a == "kex-strict-s-v00@openssh.com") {
            self.strict = true;
        }
        if first && self.strict && before > 0 {
            return Err("ssh: the server sent packets before its KEXINIT under strict key exchange".into());
        }
        let kex_alg = pick(KEX_ALGORITHMS, &server_kex)
            .ok_or("ssh: no key exchange method in common with the server")?;
        let host_alg = pick(&self.host_algs, lists.get(1).map_or(&[][..], |v| v.as_slice()))
            .ok_or("ssh: no host key algorithm in common with the server")?;
        let c2s = pick(CIPHERS, lists.get(2).map_or(&[][..], |v| v.as_slice()))
            .ok_or("ssh: no cipher in common with the server")?;
        let s2c = pick(CIPHERS, lists.get(3).map_or(&[][..], |v| v.as_slice()))
            .ok_or("ssh: no cipher in common with the server")?;
        for i in [6, 7] {
            if !lists.get(i).is_some_and(|l| l.iter().any(|c| c == "none")) {
                return Err("ssh: the server takes no uncompressed packets".into());
            }
        }
        // A guessed packet that guessed wrong is skipped (RFC 4253 §7).
        if follows
            && (server_kex.first() != Some(&kex_alg)
                || lists.get(1).and_then(|l| l.first()) != Some(&host_alg))
        {
            let _ = self.recv_raw()?;
        }

        // The client's share: ML-KEM-768's encapsulation key and X25519's, or X25519's.
        let rng = lc_rand::SystemRandom::new();
        let x25519 = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng)
            .map_err(|_| "ssh: no X25519 key")?;
        let x_pub = x25519.compute_public_key().map_err(|_| "ssh: no X25519 key")?;
        let hybrid = kex_alg == KEX_MLKEM;
        let mlkem = if hybrid {
            Some(kem::DecapsulationKey::generate(&kem::ML_KEM_768).map_err(|_| "ssh: no ML-KEM key")?)
        } else {
            None
        };
        let q_c = match &mlkem {
            Some(dk) => {
                let ek = dk.encapsulation_key().map_err(|_| "ssh: no ML-KEM key")?;
                let ek = ek.key_bytes().map_err(|_| "ssh: no ML-KEM key")?;
                [ek.as_ref(), x_pub.as_ref()].concat()
            }
            None => x_pub.as_ref().to_vec(),
        };
        self.send(&Out::of(msg::KEX_ECDH_INIT).string(&q_c).0)?;
        let reply = loop {
            let p = self.recv_raw()?;
            match p.first().copied() {
                Some(msg::KEX_ECDH_REPLY) => break p,
                Some(msg::IGNORE | msg::DEBUG) if !(first && self.strict) => {}
                Some(msg::DISCONNECT) => {
                    return Err("ssh: the server disconnected during key exchange".into());
                }
                Some(other) => return Err(format!("ssh: message {other} during key exchange")),
                None => return Err("ssh: an empty packet".into()),
            }
        };
        let mut f = Fields(reply.get(1..).unwrap_or_default());
        let (k_s, q_s, sig) = (
            f.string().ok_or("a short KEX reply")?.to_vec(),
            f.string().ok_or("a short KEX reply")?.to_vec(),
            f.string().ok_or("a short KEX reply")?.to_vec(),
        );
        let (s_ct, s_x) = if hybrid {
            q_s.split_at_checked(1088).ok_or("ssh: a short hybrid reply")?
        } else {
            (&[][..], q_s.as_slice())
        };
        if s_x.len() != 32 {
            return Err("ssh: the server's X25519 share is not 32 bytes".into());
        }
        let x_secret = agreement::agree_ephemeral(
            x25519,
            agreement::UnparsedPublicKey::new(&agreement::X25519, s_x),
            "ssh: the X25519 exchange failed",
            |k| Ok(Zeroizing::new(k.to_vec())),
        )?;
        // K: X25519's alone as an mpint (RFC 8731 §3.1); the hybrid's, SHA-256 of
        // ML-KEM's secret then X25519's, as a string.
        let k_encoded = Zeroizing::new(match &mlkem {
            Some(dk) => {
                let pq = dk
                    .decapsulate(kem::Ciphertext::from(s_ct))
                    .map_err(|_| "ssh: ML-KEM decapsulation failed")?;
                let k = Zeroizing::new(sha256(&[pq.as_ref(), &x_secret]));
                Out::default().string(&*k).0
            }
            None => Out::default().mpint(&x_secret).0,
        });
        let mut h_input = Out::default()
            .string(&self.v_c)
            .string(&self.v_s)
            .string(&i_c)
            .string(&i_s)
            .string(&k_s)
            .string(&q_c)
            .string(&q_s)
            .0;
        h_input.extend_from_slice(&k_encoded);
        let h = sha256(&[&h_input]);
        verify_host_signature(&k_s, &host_alg, &h, &sig)?;
        if first {
            known.check(name, &k_s)?;
            self.host_key = k_s;
            self.session_id = h.to_vec();
        } else if k_s != self.host_key {
            return Err("ssh: the server's host key changed while rekeying".into());
        }

        let derive = |letter: u8, len: usize| -> Zeroizing<Vec<u8>> {
            let mut out = Zeroizing::new(sha256(&[&k_encoded, &h, &[letter], &self.session_id]).to_vec());
            while out.len() < len {
                let more = sha256(&[&k_encoded, &h, &out]);
                out.extend_from_slice(&more);
            }
            out.truncate(len);
            out
        };
        let key_len = |c: &str| match c {
            "chacha20-poly1305@openssh.com" => 64,
            "aes256-gcm@openssh.com" => 32,
            _ => 16,
        };
        let keys = NewKeys {
            out: Cipher::new(&c2s, &derive(b'C', key_len(&c2s)), &derive(b'A', 12), true)?,
            inn: Cipher::new(&s2c, &derive(b'D', key_len(&s2c)), &derive(b'B', 12), false)?,
        };
        self.send(&[msg::NEWKEYS])?;
        self.out = Some(keys.out);
        if self.strict {
            self.seq_out = 0;
        }
        loop {
            let p = self.recv_raw()?;
            match p.first().copied() {
                Some(msg::NEWKEYS) => break,
                Some(msg::IGNORE | msg::DEBUG) if !(first && self.strict) => {}
                Some(other) => return Err(format!("ssh: message {other} before NEWKEYS")),
                None => return Err("ssh: an empty packet".into()),
            }
        }
        self.inn = Some(keys.inn);
        if self.strict {
            self.seq_in = 0;
        }
        Ok(())
    }

    /// `publickey` (RFC 4252 §7), each of the agent's keys in turn.
    fn authenticate(&mut self, user: &str, host: &str, agent: &mut AgentConn<'_>) -> Result<(), String> {
        self.send(&Out::of(msg::SERVICE_REQUEST).string(b"ssh-userauth").0)?;
        let p = self.recv()?;
        if p.first() != Some(&msg::SERVICE_ACCEPT) {
            return Err("ssh: the server refused authentication".into());
        }
        let denied = || format!("{user}@{host}: Permission denied (publickey).");
        for blob in agent.identities()? {
            let Some(kind) = Fields(&blob)
                .string()
                .map(|k| String::from_utf8_lossy(k).into_owned())
            else {
                continue;
            };
            let (alg, flags) = if kind == "ssh-rsa" || kind == "ssh-rsa-cert-v01@openssh.com" {
                let takes = |a: &str| self.sig_algs.as_ref().is_none_or(|s| s.iter().any(|x| x == a));
                let cert = kind.ends_with("-cert-v01@openssh.com");
                if takes("rsa-sha2-512") {
                    (
                        if cert {
                            "rsa-sha2-512-cert-v01@openssh.com"
                        } else {
                            "rsa-sha2-512"
                        },
                        4,
                    )
                } else if takes("rsa-sha2-256") {
                    (
                        if cert {
                            "rsa-sha2-256-cert-v01@openssh.com"
                        } else {
                            "rsa-sha2-256"
                        },
                        2,
                    )
                } else {
                    continue;
                }
            } else {
                (kind.as_str(), 0)
            };
            let signed = Out::default()
                .string(&self.session_id)
                .byte(msg::USERAUTH_REQUEST)
                .string(user.as_bytes())
                .string(b"ssh-connection")
                .string(b"publickey")
                .byte(1)
                .string(alg.as_bytes())
                .string(&blob)
                .0;
            let Some(sig) = agent.sign(&blob, &signed, flags)? else {
                continue;
            };
            let request = Out::default()
                .byte(msg::USERAUTH_REQUEST)
                .string(user.as_bytes())
                .string(b"ssh-connection")
                .string(b"publickey")
                .byte(1)
                .string(alg.as_bytes())
                .string(&blob)
                .string(&sig)
                .0;
            self.send(&request)?;
            loop {
                let p = self.recv()?;
                match p.first().copied() {
                    Some(msg::USERAUTH_SUCCESS) => return Ok(()),
                    Some(msg::USERAUTH_FAILURE) => {
                        let mut f = Fields(p.get(1..).unwrap_or_default());
                        let methods = f.name_list().unwrap_or_default();
                        if !methods.iter().any(|m| m == "publickey") {
                            return Err(denied());
                        }
                        break;
                    }
                    Some(msg::USERAUTH_BANNER) => {}
                    Some(other) => return Err(format!("ssh: message {other} during authentication")),
                    None => return Err("ssh: an empty packet".into()),
                }
            }
        }
        Err(denied())
    }

    /// A session channel running `command`, with `GIT_PROTOCOL=version=2`.
    fn exec(&mut self, command: &str) -> Result<(), String> {
        let ours = self.next_channel;
        self.next_channel = self.next_channel.wrapping_add(1);
        self.send(
            &Out::of(msg::CHANNEL_OPEN)
                .string(b"session")
                .u32(ours)
                .u32(WINDOW)
                .u32(MAX_PACKET)
                .0,
        )?;
        loop {
            let p = self.recv()?;
            let mut f = Fields(p.get(1..).unwrap_or_default());
            match p.first().copied() {
                Some(msg::CHANNEL_OPEN_CONFIRMATION) if f.u32() == Some(ours) => {
                    let (theirs, room, max_packet) = (
                        f.u32().ok_or("a short confirmation")?,
                        f.u32().ok_or("a short confirmation")?,
                        f.u32().ok_or("a short confirmation")?,
                    );
                    self.channel = Some(Channel {
                        ours,
                        theirs,
                        room,
                        max_packet: max_packet.clamp(1, MAX_PACKET),
                        window_left: WINDOW,
                        eof: false,
                        closed: false,
                        exit: None,
                        stderr: Vec::new(),
                    });
                    break;
                }
                Some(msg::CHANNEL_OPEN_FAILURE) if f.u32() == Some(ours) => {
                    let _ = f.u32();
                    let why = f.string().map(String::from_utf8_lossy).unwrap_or_default();
                    return Err(format!("ssh: the server refused a session: {why}"));
                }
                // What closed channels still send.
                Some(msg::CHANNEL_DATA..=msg::CHANNEL_FAILURE | msg::CHANNEL_WINDOW_ADJUST) => {}
                Some(other) => return Err(format!("ssh: message {other} opening a session")),
                None => return Err("ssh: an empty packet".into()),
            }
        }
        let theirs = self.channel.as_ref().map_or(0, |c| c.theirs);
        self.send(
            &Out::of(msg::CHANNEL_REQUEST)
                .u32(theirs)
                .string(b"env")
                .byte(0)
                .string(b"GIT_PROTOCOL")
                .string(b"version=2")
                .0,
        )?;
        self.send(
            &Out::of(msg::CHANNEL_REQUEST)
                .u32(theirs)
                .string(b"exec")
                .byte(1)
                .string(command.as_bytes())
                .0,
        )?;
        loop {
            match self.next_event()? {
                Some(Ok(())) => return Ok(()),
                Some(Err(())) => return Err(format!("ssh: the server would not run {command}")),
                None => {}
            }
        }
    }

    /// The next packet for the channel in use, taken into it; a channel request's answer
    /// (success or failure) if it was one.
    fn next_event(&mut self) -> Result<Option<Result<(), ()>>, String> {
        let p = self.recv()?;
        let mut f = Fields(p.get(1..).unwrap_or_default());
        let to = f.u32();
        let Some(ch) = self.channel.as_mut().filter(|c| Some(c.ours) == to) else {
            return Ok(None);
        };
        match p.first().copied() {
            Some(msg::CHANNEL_SUCCESS) => return Ok(Some(Ok(()))),
            Some(msg::CHANNEL_FAILURE) => return Ok(Some(Err(()))),
            Some(msg::CHANNEL_WINDOW_ADJUST) => {
                ch.room = ch.room.saturating_add(f.u32().unwrap_or(0));
            }
            Some(msg::CHANNEL_DATA) => {
                let data = f.string().ok_or("a short data packet")?;
                ch.window_left = ch
                    .window_left
                    .checked_sub(u32::try_from(data.len()).unwrap_or(u32::MAX))
                    .ok_or("ssh: the server sent past the channel's window")?;
                self.pending.extend_from_slice(data);
            }
            Some(msg::CHANNEL_EXTENDED_DATA) => {
                let _ = f.u32();
                let data = f.string().ok_or("a short data packet")?;
                ch.window_left = ch
                    .window_left
                    .checked_sub(u32::try_from(data.len()).unwrap_or(u32::MAX))
                    .ok_or("ssh: the server sent past the channel's window")?;
                if ch.stderr.len() < 64 * 1024 {
                    ch.stderr.extend_from_slice(data);
                }
            }
            Some(msg::CHANNEL_EOF) => ch.eof = true,
            Some(msg::CHANNEL_CLOSE) => {
                ch.closed = true;
                ch.eof = true;
            }
            Some(msg::CHANNEL_REQUEST) => {
                let kind = f.string().unwrap_or_default().to_vec();
                let reply = f.bool().unwrap_or(false);
                if kind == b"exit-status" {
                    ch.exit = f.u32();
                }
                let theirs = ch.theirs;
                if reply {
                    self.send(&Out::of(msg::CHANNEL_FAILURE).u32(theirs).0)?;
                }
            }
            _ => {}
        }
        // Room given back once half the window is used.
        if let Some(ch) = self.channel.as_mut()
            && !ch.eof
            && ch.window_left < WINDOW / 2
        {
            let give = WINDOW - ch.window_left;
            ch.window_left = WINDOW;
            let theirs = ch.theirs;
            self.send(&Out::of(msg::CHANNEL_WINDOW_ADJUST).u32(theirs).u32(give).0)?;
        }
        Ok(None)
    }

    /// Sends `data` on the channel in use, within the room the server gives it, then its
    /// end.
    fn write_all(&mut self, mut data: &[u8]) -> Result<(), String> {
        while !data.is_empty() {
            let (room, max, theirs) = match &self.channel {
                Some(c) if !c.closed => (c.room, c.max_packet, c.theirs),
                _ => return Err("ssh: the session ended before its request was sent".into()),
            };
            if room == 0 {
                self.next_event()?;
                continue;
            }
            let n = usize::try_from(room.min(max)).unwrap_or(0).min(data.len());
            let (chunk, rest) = data.split_at(n);
            self.send(&Out::of(msg::CHANNEL_DATA).u32(theirs).string(chunk).0)?;
            if let Some(c) = self.channel.as_mut() {
                c.room -= u32::try_from(n).unwrap_or(0);
            }
            data = rest;
        }
        let theirs = self.channel.as_ref().map_or(0, |c| c.theirs);
        self.send(&Out::of(msg::CHANNEL_EOF).u32(theirs).0)
    }

    /// Ends the channel in use (CHANNEL_CLOSE), unless the server has.
    fn close(&mut self) {
        if let Some(c) = self.channel.take()
            && !c.closed
        {
            let _ = self.send(&Out::of(msg::CHANNEL_CLOSE).u32(c.theirs).0);
        }
        self.pending.clear();
    }
}

/// Verifies `sig` (RFC 4253 §6.6) over the exchange hash by the host key `k_s`, with the
/// algorithm negotiated.
fn verify_host_signature(k_s: &[u8], alg: &str, h: &[u8], sig: &[u8]) -> Result<(), String> {
    let bad = || "ssh: the server's host key signature is not valid".to_string();
    let mut s = Fields(sig);
    let (format, blob) = (s.string().ok_or_else(bad)?, s.string().ok_or_else(bad)?);
    if format != alg.as_bytes() {
        return Err(bad());
    }
    let mut k = Fields(k_s);
    let kind = k.string().ok_or_else(bad)?;
    let ok = match alg {
        "ssh-ed25519" if kind == b"ssh-ed25519" => {
            let public = k.string().ok_or_else(bad)?;
            signature::UnparsedPublicKey::new(&signature::ED25519, public)
                .verify(h, blob)
                .is_ok()
        }
        "ecdsa-sha2-nistp256" | "ecdsa-sha2-nistp384" | "ecdsa-sha2-nistp521" if kind == alg.as_bytes() => {
            let _curve = k.string().ok_or_else(bad)?;
            let point = k.string().ok_or_else(bad)?;
            let (len, verifier): (usize, &'static signature::EcdsaVerificationAlgorithm) = match alg {
                "ecdsa-sha2-nistp256" => (32, &signature::ECDSA_P256_SHA256_FIXED),
                "ecdsa-sha2-nistp384" => (48, &signature::ECDSA_P384_SHA384_FIXED),
                _ => (66, &signature::ECDSA_P521_SHA512_FIXED),
            };
            let mut rs = Fields(blob);
            let mut fixed = Vec::with_capacity(2 * len);
            for _ in 0..2 {
                let n = rs.string().ok_or_else(bad)?;
                let n = n
                    .get(n.iter().take_while(|b| **b == 0).count()..)
                    .unwrap_or_default();
                if n.len() > len {
                    return Err(bad());
                }
                fixed.extend(std::iter::repeat_n(0, len - n.len()));
                fixed.extend_from_slice(n);
            }
            signature::UnparsedPublicKey::new(verifier, point)
                .verify(h, &fixed)
                .is_ok()
        }
        "rsa-sha2-256" | "rsa-sha2-512" if kind == b"ssh-rsa" => {
            let (e, n) = (k.string().ok_or_else(bad)?, k.string().ok_or_else(bad)?);
            let strip = |v: &'_ [u8]| -> Vec<u8> {
                v.get(v.iter().take_while(|b| **b == 0).count()..)
                    .unwrap_or_default()
                    .to_vec()
            };
            let params = if alg == "rsa-sha2-256" {
                &signature::RSA_PKCS1_2048_8192_SHA256
            } else {
                &signature::RSA_PKCS1_2048_8192_SHA512
            };
            signature::RsaPublicKeyComponents {
                n: strip(n),
                e: strip(e),
            }
            .verify(params, h, blob)
            .is_ok()
        }
        _ => false,
    };
    if ok { Ok(()) } else { Err(bad()) }
}

/// The build's agent, asked for its keys and signatures: a socket the client forwards, or
/// the keys of files it serves (sshkey).
pub struct AgentConn<'a> {
    agent: &'a Agent<Key>,
    #[cfg(unix)]
    socket: Option<std::os::unix::net::UnixStream>,
}

impl<'a> AgentConn<'a> {
    pub fn new(agent: &'a Agent<Key>) -> AgentConn<'a> {
        AgentConn {
            agent,
            #[cfg(unix)]
            socket: None,
        }
    }

    fn ask(&mut self, request: &[u8]) -> Result<Vec<u8>, String> {
        match self.agent {
            Agent::Keys(keys) => Ok(sshkey::answer(keys, request)),
            #[cfg(unix)]
            Agent::Socket(path) => {
                let s = match &mut self.socket {
                    Some(s) => s,
                    None => self.socket.insert(
                        std::os::unix::net::UnixStream::connect(path)
                            .map_err(|e| format!("the SSH agent at {}: {e}", path.display()))?,
                    ),
                };
                let n = u32::try_from(request.len()).map_err(|_| "an agent request too long")?;
                s.write_all(&n.to_be_bytes())
                    .map_err(|e| format!("the SSH agent: {e}"))?;
                s.write_all(request).map_err(|e| format!("the SSH agent: {e}"))?;
                let mut len = [0u8; 4];
                s.read_exact(&mut len)
                    .map_err(|e| format!("the SSH agent: {e}"))?;
                let len = usize::try_from(u32::from_be_bytes(len)).unwrap_or(usize::MAX);
                if len > 256 * 1024 {
                    return Err("the SSH agent's answer is too long".into());
                }
                let mut answer = vec![0u8; len];
                s.read_exact(&mut answer)
                    .map_err(|e| format!("the SSH agent: {e}"))?;
                Ok(answer)
            }
            #[cfg(not(unix))]
            Agent::Socket(path) => Err(format!(
                "the SSH agent at {}: agents are reached on Unix alone",
                path.display()
            )),
        }
    }

    /// The agent's keys' blobs.
    fn identities(&mut self) -> Result<Vec<Vec<u8>>, String> {
        let answer = self.ask(&[11])?;
        let mut f = Fields(&answer);
        if f.byte() != Some(12) {
            return Err("the SSH agent would not list its keys".into());
        }
        let n = f.u32().unwrap_or(0);
        let mut keys = Vec::new();
        for _ in 0..n.min(1024) {
            let (Some(blob), Some(_)) = (f.string(), f.string()) else {
                break;
            };
            keys.push(blob.to_vec());
        }
        Ok(keys)
    }

    /// A signature of `data` by `blob`'s key; none where the agent refuses.
    fn sign(&mut self, blob: &[u8], data: &[u8], flags: u32) -> Result<Option<Vec<u8>>, String> {
        let request = Out::of(13).string(blob).string(data).u32(flags).0;
        let answer = self.ask(&request)?;
        let mut f = Fields(&answer);
        if f.byte() != Some(14) {
            return Ok(None);
        }
        Ok(f.string().map(<[u8]>::to_vec))
    }
}

/// A repository over SSH: one connection, a session for each request.
pub struct Ssh {
    conn: RefCell<Conn>,
    command: String,
}

impl Ssh {
    /// Connects to `url`'s host, checks it is the one known_hosts knows, and authenticates
    /// with `agent`'s keys.
    pub fn open(url: &str, agent: &Agent<Key>, patience: Duration) -> Result<Ssh, String> {
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_default();
        let target = Target::of_url(url, &user)?;
        let texts: Vec<Vec<u8>> = known_hosts_files()
            .iter()
            .filter_map(|p| std::fs::read(p).ok())
            .collect();
        let known = Known::of(&target.known_name(), &texts);
        let mut conn = Conn::connect(&target, &known, patience)?;
        conn.authenticate(&target.user, &target.host, &mut AgentConn::new(agent))?;
        Ok(Ssh {
            conn: RefCell::new(conn),
            command: target.command(),
        })
    }

    fn session(&self, body: Option<&[u8]>) -> Result<Box<dyn Read + '_>, String> {
        let mut conn = self
            .conn
            .try_borrow_mut()
            .map_err(|_| "ssh: a session is still open")?;
        conn.close();
        conn.exec(&self.command)?;
        let mut reader = Session { conn };
        if let Some(body) = body {
            // The advertisement first, to its flush.
            loop {
                match shards_git::pktline::read(&mut reader).map_err(|e| format!("the advertisement: {e}"))? {
                    Some(shards_git::pktline::Packet::Flush) => break,
                    Some(_) => {}
                    None => return Err(reader.failure("the server hung up")),
                }
            }
            reader.conn.write_all(body)?;
        } else {
            reader.conn.write_all(&[])?;
        }
        Ok(Box::new(reader))
    }
}

/// A session's output, read as it comes.
struct Session<'a> {
    conn: std::cell::RefMut<'a, Conn>,
}

impl Session<'_> {
    /// Why the session ended early: what the remote command said on stderr, if anything.
    fn failure(&self, fallback: &str) -> String {
        let said = self
            .conn
            .channel
            .as_ref()
            .map(|c| String::from_utf8_lossy(&c.stderr).trim().to_string())
            .unwrap_or_default();
        if said.is_empty() {
            fallback.to_string()
        } else {
            said
        }
    }
}

impl Read for Session<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if !self.conn.pending.is_empty() {
                let n = buf.len().min(self.conn.pending.len());
                for (b, p) in buf.iter_mut().zip(self.conn.pending.drain(..n)) {
                    *b = p;
                }
                return Ok(n);
            }
            // Its end: closed, or ended with its status known (which may follow its EOF).
            if self
                .conn
                .channel
                .as_ref()
                .is_none_or(|c| c.closed || c.eof && c.exit.is_some())
            {
                let failed = self
                    .conn
                    .channel
                    .as_ref()
                    .is_some_and(|c| c.exit.is_some_and(|e| e != 0));
                if failed {
                    return Err(std::io::Error::other(self.failure("the remote command failed")));
                }
                return Ok(0);
            }
            self.conn.next_event().map_err(std::io::Error::other)?;
        }
    }
}

impl Transport for Ssh {
    fn advertise(&self) -> Result<Box<dyn Read + '_>, String> {
        self.session(None)
    }

    fn command(&self, body: &[u8]) -> Result<Box<dyn Read + '_>, String> {
        self.session(Some(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_name_their_targets_as_git_reads_them() {
        let t = |u: &str| Target::of_url(u, "me").unwrap();
        let target = |user: &str, host: &str, port: u16, path: &str| Target {
            user: user.into(),
            host: host.into(),
            port,
            path: path.into(),
        };
        assert_eq!(
            t("git@github.com:org/repo.git"),
            target("git", "github.com", 22, "org/repo.git")
        );
        assert_eq!(
            t("ssh://git@Host:2222/a/b.git"),
            target("git", "host", 2222, "/a/b.git")
        );
        assert_eq!(t("ssh://host/~u/r"), target("me", "host", 22, "~u/r"));
        assert_eq!(t("git+ssh://h/r"), target("me", "h", 22, "/r"));
        assert_eq!(t("[::1]:r"), target("me", "::1", 22, "r"));
        assert_eq!(t("ssh://[::1]:22/r"), target("me", "::1", 22, "/r"));
        assert!(Target::of_url("ssh://-oProxyCommand=x/r", "me").is_err());
        assert!(is_ssh("git@github.com:org/repo.git"));
        assert!(is_ssh("ssh://h/r"));
        assert!(!is_ssh("https://h/r"));
        assert!(!is_ssh("./a:b"));
        assert_eq!(
            target("u", "h", 22, "it's!").command(),
            "git-upload-pack 'it'\\''s'\\!''"
        );
        assert_eq!(target("u", "h", 2222, "/r").known_name(), "[h]:2222");
    }

    /// Host keys' signatures over an exchange hash: each kind verified, and refused once
    /// a byte of it or of the hash changes, or under another algorithm.
    #[test]
    fn host_signatures_are_verified() {
        #[derive(serde::Deserialize)]
        struct Case {
            name: String,
            files: Vec<String>,
        }
        let cases: Vec<Case> = serde_json::from_str(include_str!("testdata/sshkey.json")).unwrap();
        let h = sha256(&[b"an exchange hash"]);
        for (name, alg, flags) in [
            ("ed25519-openssh", "ssh-ed25519", 0),
            ("ecdsa-p256-openssh", "ecdsa-sha2-nistp256", 0),
            ("ecdsa-p384-openssh", "ecdsa-sha2-nistp384", 0),
            ("ecdsa-p521-openssh", "ecdsa-sha2-nistp521", 0),
            ("rsa2048-openssh", "rsa-sha2-256", 2),
            ("rsa3072-openssh", "rsa-sha2-512", 4),
        ] {
            let file = &cases.iter().find(|c| c.name == name).unwrap().files[0];
            let ring = vec![sshkey::parse(file.as_bytes()).unwrap()];
            let mut fields_answer = sshkey::answer(&ring, &[11]);
            let blob = Fields(&fields_answer[5..]).string().unwrap().to_vec();
            fields_answer = sshkey::answer(&ring, &Out::of(13).string(&blob).string(&h).u32(flags).0);
            let sig = Fields(&fields_answer[1..]).string().unwrap().to_vec();
            verify_host_signature(&blob, alg, &h, &sig).unwrap();
            let mut tampered = sig.clone();
            *tampered.last_mut().unwrap() ^= 1;
            assert!(
                verify_host_signature(&blob, alg, &h, &tampered).is_err(),
                "{name}"
            );
            let mut other = h;
            other[0] ^= 1;
            assert!(verify_host_signature(&blob, alg, &other, &sig).is_err(), "{name}");
            assert!(verify_host_signature(&blob, "ssh-ed25519", &h, &sig).is_err() || alg == "ssh-ed25519");
        }
    }

    /// A server that sends anything before its KEXINIT, then says its exchange is strict,
    /// is refused (OpenSSH PROTOCOL §1.10, against Terrapin); one that is not strict is
    /// let go on.
    #[test]
    fn strict_key_exchange_refuses_what_comes_before_kexinit() {
        for (strict, refused) in [(true, true), (false, false)] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = std::thread::spawn(move || {
                let (mut c, _) = listener.accept().unwrap();
                c.write_all(b"SSH-2.0-test\r\n").unwrap();
                let packet = |payload: &[u8]| {
                    let mut pad = 8 - (5 + payload.len()) % 8;
                    if pad < 4 {
                        pad += 8;
                    }
                    let mut p = ((1 + payload.len() + pad) as u32).to_be_bytes().to_vec();
                    p.push(pad as u8);
                    p.extend_from_slice(payload);
                    p.extend(std::iter::repeat_n(0, pad));
                    p
                };
                c.write_all(&packet(&Out::of(msg::IGNORE).string(b"injected").0))
                    .unwrap();
                let kex: &[&str] = if strict {
                    &["curve25519-sha256", "kex-strict-s-v00@openssh.com"]
                } else {
                    &["curve25519-sha256"]
                };
                let mut kexinit = Out::of(msg::KEXINIT);
                kexinit.0.extend_from_slice(&[0; 16]);
                let kexinit = kexinit
                    .names(kex)
                    .names(&["ssh-ed25519"])
                    .names(&["aes128-gcm@openssh.com"])
                    .names(&["aes128-gcm@openssh.com"])
                    .names(MACS)
                    .names(MACS)
                    .names(&["none"])
                    .names(&["none"])
                    .names(&[])
                    .names(&[])
                    .byte(0)
                    .u32(0);
                c.write_all(&packet(&kexinit.0)).unwrap();
                // Held until the client hangs up.
                let mut sink = Vec::new();
                let _ = c.read_to_end(&mut sink);
            });
            let target = Target::of_url(&format!("ssh://u@127.0.0.1:{port}/r"), "u").unwrap();
            let err = Conn::connect(&target, &Known::default(), Duration::from_secs(2))
                .err()
                .unwrap();
            assert_eq!(
                err.contains("before its KEXINIT under strict key exchange"),
                refused,
                "strict {strict}: {err}"
            );
            server.join().unwrap();
        }
    }

    #[test]
    fn known_hosts_are_matched_as_openssh_matches_them() {
        use base64::Engine as _;
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        let key = |kind: &str, n: u8| Out::default().string(kind.as_bytes()).string(&[n; 32]).0;
        let (a, b, c) = (
            key("ssh-ed25519", 1),
            key("ssh-ed25519", 2),
            key("ecdsa-sha2-nistp256", 3),
        );
        let salt = [7u8; 20];
        let hashed = {
            let k = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &salt);
            format!(
                "|1|{}|{}",
                b64(&salt),
                b64(hmac::sign(&k, b"[h.example]:2222").as_ref())
            )
        };
        let text = format!(
            "# a comment\n\
             other.example ssh-ed25519 {}\n\
             *.example,!bad.example ssh-ed25519 {}\n\
             {hashed} ecdsa-sha2-nistp256 {}\n\
             @revoked * ssh-ed25519 {}\n\
             @cert-authority *.example ssh-ed25519 {}\n",
            b64(&b),
            b64(&a),
            b64(&c),
            b64(&b),
            b64(&c),
        );
        let texts = [text.into_bytes()];
        let known = Known::of("h.example", &texts);
        assert!(known.check("h.example", &a).is_ok());
        assert!(
            known
                .check("h.example", &c)
                .unwrap_err()
                .contains("not in known_hosts")
        );
        assert!(
            known
                .check("h.example", &key("ssh-ed25519", 9))
                .unwrap_err()
                .contains("has changed")
        );
        assert!(known.check("h.example", &b).unwrap_err().contains("revoked"));
        assert_eq!(known.algorithms().first(), Some(&"ssh-ed25519"));
        assert!(Known::of("bad.example", &texts).check("bad.example", &a).is_err());
        let port = Known::of("[h.example]:2222", &texts);
        assert!(port.check("[h.example]:2222", &c).is_ok());
        assert_eq!(port.algorithms().first(), Some(&"ecdsa-sha2-nistp256"));
        assert!(Known::of("h.example:2222", &texts).check("x", &c).is_err());
    }
}
