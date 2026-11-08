//! Per-registry TLS material from `certs.d`, read as dockerd reads it (moby docker-v29.8.1
//! `daemon/pkg/registry/registry.go`, `loadTLSConfig`):
//! - `*.crt` files hold CA certificates, trusted beside the platform's roots;
//! - a `*.cert` with the `*.key` of the same name is a client certificate;
//! - entries are taken in name order, and a missing directory holds nothing.
//!
//! The directories are Docker's (docker/docs `engine/security/certificates.md`):
//! - Docker Desktop's `~/.docker/certs.d`;
//! - the rootless engine's `$XDG_CONFIG_HOME/docker/certs.d`;
//! - the native engine's `/etc/docker/certs.d`, or `%PROGRAMDATA%\docker\certs.d` on
//!   Windows.
//!
//! Each holds one directory per `host[:port]`; on Windows the colon is dropped.

use std::path::{Path, PathBuf};

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::Error;
use crate::credentials::Env;

/// A client certificate: its chain, and its key.
pub type ClientCertificate = (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>);

/// What `certs.d` holds for a registry.
#[derive(Debug, Default)]
pub struct Material {
    pub roots: Vec<CertificateDer<'static>>,
    /// The first client certificate, by name. dockerd offers every one and Go's TLS picks;
    /// rustls takes one.
    pub client: Option<ClientCertificate>,
}

/// The material for `host` (`host[:port]`, as a reference names it).
pub fn load(host: &str, env: Env<'_>) -> Result<Material, Error> {
    let name = if cfg!(windows) {
        host.replace(':', "")
    } else {
        host.to_string()
    };
    let mut material = Material::default();
    for dir in dirs(env) {
        read(&dir.join(&name), &mut material)?;
    }
    Ok(material)
}

fn dirs(env: Env<'_>) -> Vec<PathBuf> {
    let home = if cfg!(windows) {
        env("USERPROFILE")
    } else {
        env("HOME")
    }
    .filter(|h| !h.is_empty());
    let mut dirs: Vec<PathBuf> = home
        .iter()
        .map(|h| Path::new(h).join(".docker").join("certs.d"))
        .collect();
    if cfg!(windows) {
        if let Some(data) = env("PROGRAMDATA").filter(|d| !d.is_empty()) {
            dirs.push(Path::new(&data).join("docker").join("certs.d"));
        }
    } else {
        let config = env("XDG_CONFIG_HOME")
            .filter(|c| !c.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| Path::new(h).join(".config")));
        dirs.extend(config.map(|c| c.join("docker").join("certs.d")));
        dirs.push(PathBuf::from("/etc/docker/certs.d"));
    }
    dirs
}

/// dockerd's `loadTLSConfig` for one directory.
fn read(dir: &Path, material: &mut Material) -> Result<(), Error> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::from(e).context(dir.display())),
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let has = |n: &str| names.iter().any(|x| x == n);
    for name in &names {
        let path = dir.join(name);
        if name.ends_with(".crt") {
            // Like Go's AppendCertsFromPEM, what does not parse is passed over.
            let certs = CertificateDer::pem_file_iter(&path)
                .map_err(|e| Error::new(format!("{}: {e}", path.display())))?;
            material.roots.extend(certs.filter_map(Result::ok));
        } else if let Some(stem) = name.strip_suffix(".cert") {
            let key = format!("{stem}.key");
            if !has(&key) {
                return Err(Error::new(format!(
                    "missing key {key} for client certificate {name}. CA certificates must use the extension .crt"
                )));
            }
            if material.client.is_none() {
                let chain = CertificateDer::pem_file_iter(&path)
                    .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
                    .map_err(|e| Error::new(format!("{}: {e}", path.display())))?;
                let key = PrivateKeyDer::from_pem_file(dir.join(&key))
                    .map_err(|e| Error::new(format!("{}: {e}", dir.join(&key).display())))?;
                material.client = Some((chain, key));
            }
        } else if let Some(stem) = name.strip_suffix(".key")
            && !has(&format!("{stem}.cert"))
        {
            return Err(Error::new(format!(
                "missing client certificate {stem}.cert for key {name}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;

    use rustls::pki_types::ServerName;
    use rustls::{ClientConnection, ServerConnection, StreamOwned};

    use crate::testing::demanding_registry;
    use crate::tls::client_config;

    fn home(name: &str) -> shards_testdir::TempDir {
        shards_testdir::TempDir::new(&format!("certs-{name}")).unwrap()
    }

    fn pem(der: &CertificateDer<'_>) -> String {
        use base64::Engine as _;
        let body = base64::engine::general_purpose::STANDARD.encode(der.as_ref());
        let lines: Vec<&str> = body
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            lines.join("\n")
        )
    }

    /// A certs.d directory for `host` in a fresh home, and an environment naming that home.
    fn certs_d(
        name: &str,
        host: &str,
        files: &[(&str, &str)],
    ) -> (shards_testdir::TempDir, HashMap<String, String>) {
        let home = home(name);
        let dir = home.join(".docker").join("certs.d").join(if cfg!(windows) {
            host.replace(':', "")
        } else {
            host.to_string()
        });
        std::fs::create_dir_all(&dir).unwrap();
        for (file, content) in files {
            std::fs::write(dir.join(file), content).unwrap();
        }
        let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let env = HashMap::from([(key.to_string(), home.to_string_lossy().into_owned())]);
        (home, env)
    }

    #[test]
    fn registries_are_reached_with_their_certs_d_material() {
        let (ca, client_pem, key_pem, server) = demanding_registry();
        let (home, env) = certs_d(
            "full",
            "shards-test.invalid:5000",
            &[
                ("ca.crt", &pem(&ca)),
                ("client.cert", &client_pem),
                ("client.key", &key_pem),
                ("notes.txt", "ignored"),
            ],
        );
        let material = load("shards-test.invalid:5000", &|k| env.get(k).cloned()).unwrap();
        assert_eq!(material.roots.len(), 1);
        assert!(material.client.is_some());
        let serve = |server: Arc<rustls::ServerConfig>| {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                let (tcp, _) = listener.accept().unwrap();
                let mut tls = StreamOwned::new(ServerConnection::new(server).unwrap(), tcp);
                let mut hello = [0u8; 5];
                if tls.read_exact(&mut hello).is_ok() {
                    let _ = tls.write_all(b"world");
                    let _ = tls.flush();
                }
            });
            port
        };
        let talk = |config: Arc<rustls::ClientConfig>, port: u16| -> std::io::Result<[u8; 5]> {
            let conn = ClientConnection::new(config, ServerName::try_from("localhost").unwrap())
                .map_err(std::io::Error::other)?;
            let mut tls = StreamOwned::new(conn, TcpStream::connect(("127.0.0.1", port))?);
            tls.write_all(b"hello")?;
            let mut reply = [0u8; 5];
            tls.read_exact(&mut reply)?;
            Ok(reply)
        };
        let with = client_config(material.roots.clone(), material.client).unwrap();
        assert_eq!(&talk(with, serve(server.clone())).unwrap(), b"world");
        let without = client_config(material.roots, None).unwrap();
        assert!(
            talk(without, serve(server)).is_err(),
            "the server demands a certificate"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn halves_of_client_certificates_are_refused() {
        let (home, env) = certs_d("halves", "r.example", &[("client.cert", "x")]);
        assert!(
            load("r.example", &|k| env.get(k).cloned())
                .unwrap_err()
                .to_string()
                .contains("missing key client.key")
        );
        let _ = std::fs::remove_dir_all(&home);
        let (home, env) = certs_d("halves-key", "r.example", &[("client.key", "x")]);
        assert!(
            load("r.example", &|k| env.get(k).cloned())
                .unwrap_err()
                .to_string()
                .contains("missing client certificate")
        );
        let _ = std::fs::remove_dir_all(&home);
        let env: HashMap<String, String> = HashMap::new();
        assert!(
            load("nothing.example", &|k| env.get(k).cloned())
                .unwrap()
                .roots
                .is_empty()
        );
    }
}
