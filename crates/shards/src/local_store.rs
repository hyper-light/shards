//! Making shards' microVMs visible in the machine's local image store: Docker's, found as
//! the Docker CLI finds its engine, where there is one. A microVM goes there as an OCI
//! artifact, its EROFS disk the one layer (shards_image::save::microvm), named
//! `shards.local/` and the image's familiar name, so that it stands beside Docker's own
//! images and replaces none of them; `docker images` lists it and `docker run` will not
//! run it (Docker 29.3.1, measured 2026-10-04). Where no store answers, nothing is made
//! (shards' own local registry is to come).
//!
//! The engine is found as docker/cli finds it (cli/command/cli.go, resolveDockerEndpoint;
//! cli/context/store): `DOCKER_HOST`, else `DOCKER_CONTEXT`'s endpoint, else the config's
//! `currentContext`'s, else `/var/run/docker.sock`. Only a Unix socket is spoken to.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use shards_image::reference::{Digest, Reference};

/// Where a microVM is named in the local store: `shards.local/` and the familiar name.
pub fn name(reference: &str) -> String {
    let familiar = Reference::parse_normalized(reference)
        .map(|r| r.tag_name_only().familiar())
        .unwrap_or_else(|_| reference.to_string());
    format!("shards.local/{familiar}")
}

/// The local engine's socket, if one is named and there.
pub fn engine(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let unix = |host: &str| host.strip_prefix("unix://").map(PathBuf::from);
    if let Some(host) = env("DOCKER_HOST").filter(|h| !h.is_empty()) {
        return unix(&host).filter(|p| p.exists());
    }
    let home = env("HOME").map(PathBuf::from);
    let config = env("DOCKER_CONFIG")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".docker")));
    let context = env("DOCKER_CONTEXT").filter(|c| !c.is_empty()).or_else(|| {
        let text = std::fs::read(config.as_ref()?.join("config.json")).ok()?;
        let v: serde_json::Value = serde_json::from_slice(&text).ok()?;
        v.get("currentContext")?.as_str().map(str::to_string)
    });
    if let Some(context) = context.filter(|c| c != "default")
        && let Some(config) = &config
    {
        use sha2::{Digest as _, Sha256};
        let id: String = Sha256::digest(context.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let meta = std::fs::read(config.join("contexts/meta").join(id).join("meta.json")).ok()?;
        let v: serde_json::Value = serde_json::from_slice(&meta).ok()?;
        let host = v.pointer("/Endpoints/docker/Host")?.as_str()?;
        return unix(host).filter(|p| p.exists());
    }
    Some(PathBuf::from("/var/run/docker.sock")).filter(|p| p.exists())
}

/// Bytes written as HTTP/1.1 chunks.
struct Chunked<W: Write>(W);

impl<W: Write> Write for Chunked<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        write!(self.0, "{:x}\r\n", buf.len())?;
        self.0.write_all(buf)?;
        self.0.write_all(b"\r\n")?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// Puts the microVM whose disk is `disk`, made from image `source` of `reference`, for
/// `platform`, in the engine at `socket` (the Engine API's `POST /images/load`).
pub fn publish(
    socket: &Path,
    reference: &str,
    source: &Digest,
    platform: &str,
    disk: &Path,
) -> Result<String, String> {
    let named = name(reference);
    let stream = UnixStream::connect(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    let mut out = std::io::BufWriter::with_capacity(1 << 20, &stream);
    out.write_all(
        b"POST /images/load?quiet=1 HTTP/1.1\r\nHost: docker\r\nUser-Agent: shards\r\nContent-Type: application/x-tar\r\nTransfer-Encoding: chunked\r\n\r\n",
    )
    .map_err(|e| e.to_string())?;
    let mut chunked = Chunked(out);
    shards_image::save::microvm(&named, disk, source, platform, &mut chunked).map_err(|e| e.to_string())?;
    let mut out = chunked.0;
    out.write_all(b"0\r\n\r\n")
        .and_then(|()| out.flush())
        .map_err(|e| e.to_string())?;
    drop(out);
    let mut reader = BufReader::new(&stream);
    let mut status = String::new();
    reader.read_line(&mut status).map_err(|e| e.to_string())?;
    let code = status.split(' ').nth(1).unwrap_or_default();
    if code != "200" {
        // What the engine said, for the log.
        let mut rest = String::new();
        let _ = Read::read_to_string(&mut (&mut reader).take(4096), &mut rest);
        return Err(format!(
            "the local image store refused {named}: {} {}",
            status.trim(),
            rest.trim()
        ));
    }
    // The answer streams JSON lines; a failure to load is said in one of them.
    let mut body = String::new();
    let _ = Read::read_to_string(&mut (&mut reader).take(1 << 16), &mut body);
    if let Some(error) = body.split("\"error\":").nth(1) {
        return Err(format!("the local image store refused {named}: {}", error.trim()));
    }
    Ok(named)
}

/// Removes the microVM named for `reference` from the engine at `socket`, if it is there.
pub fn unpublish(socket: &Path, reference: &str) -> Result<(), String> {
    let named = name(reference);
    let stream = UnixStream::connect(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    let path: String = named
        .bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            b => format!("%{b:02X}"),
        })
        .collect();
    (&stream)
        .write_all(format!("DELETE /images/{path} HTTP/1.1\r\nHost: docker\r\nUser-Agent: shards\r\nConnection: close\r\n\r\n").as_bytes())
        .map_err(|e| e.to_string())?;
    let mut status = String::new();
    BufReader::new(&stream)
        .read_line(&mut status)
        .map_err(|e| e.to_string())?;
    match status.split(' ').nth(1).unwrap_or_default() {
        "200" | "404" => Ok(()),
        _ => Err(format!(
            "the local image store would not remove {named}: {}",
            status.trim()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_microvm_is_named_beside_its_image() {
        assert_eq!(
            name("docker.io/library/ubuntu:latest"),
            "shards.local/ubuntu:latest"
        );
        assert_eq!(name("ghcr.io/a/b:1"), "shards.local/ghcr.io/a/b:1");
        assert_eq!(name("alpine"), "shards.local/alpine:latest");
    }

    #[test]
    fn the_engine_is_found_as_the_cli_finds_it() {
        let dir = std::env::temp_dir().join(format!("shards-engine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sock = dir.join("docker.sock");
        std::fs::create_dir_all(
            dir.join(
                ".docker/contexts/meta/fe9c6bd7a66301f49ca9b6a70b217107cd1284598bfc254700c989b916da791e",
            ),
        )
        .unwrap_or(());
        std::fs::write(&sock, b"").unwrap_or(());
        std::fs::write(
            dir.join(".docker/config.json"),
            br#"{"currentContext":"desktop-linux"}"#,
        )
        .unwrap_or(());
        let meta = format!(
            r#"{{"Endpoints":{{"docker":{{"Host":"unix://{}"}}}}}}"#,
            sock.display()
        );
        std::fs::write(
            dir.join(".docker/contexts/meta/fe9c6bd7a66301f49ca9b6a70b217107cd1284598bfc254700c989b916da791e/meta.json"),
            meta,
        )
        .unwrap_or(());
        let home = dir.display().to_string();
        let env = |k: &str| (k == "HOME").then(|| home.clone());
        assert_eq!(engine(&env), Some(sock.clone()));
        let host = format!("unix://{}", sock.display());
        let env = |k: &str| (k == "DOCKER_HOST").then(|| host.clone());
        assert_eq!(engine(&env), Some(sock));
        let env = |k: &str| (k == "DOCKER_HOST").then(|| "tcp://1.2.3.4:2375".to_string());
        assert_eq!(engine(&env), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
