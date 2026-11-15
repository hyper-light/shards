//! A real Agentfile build through `shards_api::Builds` (D263): an ignored-unless-asked test
//! its runner runs where `SHARDS_API_BUILD=1`, `SHARDS_BIN` naming the `shards` to build with.
//! Its Agentfile copies a file into `scratch`, so no base image is pulled and no builder
//! microVM boots: what it holds to is the API's path, the request, the events, the image's
//! ID in the caller's home's store, and a failing step refused by its number. A component
//! (D270) is built alone the same way, from a pinned http source this test serves.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout
)]

use shards_api::{
    BuildEvent, BuildRefusal, BuildSpec, Builds, ComponentKind, ComponentSpec, S3Source, StepEnd,
};

fn asked() -> bool {
    let asked = std::env::var_os("SHARDS_API_BUILD").is_some_and(|v| v == "1");
    if !asked {
        println!("SKIP: SHARDS_API_BUILD=1 builds through a real shards");
    }
    asked
}

#[test]
fn an_agentfile_builds_through_the_api_into_the_callers_home() {
    if !asked() {
        return;
    }
    let dir = shards_testdir::TempDir::new("api-build").unwrap();
    let context = dir.path().join("ctx");
    std::fs::create_dir_all(&context).unwrap();
    std::fs::write(context.join("hello"), "hello\n").unwrap();
    std::fs::write(context.join("Agentfile"), "FROM scratch\nCOPY hello /hello\n").unwrap();
    std::fs::write(context.join("Broken"), "FROM scratch\nCOPY missing /missing\n").unwrap();
    let home = dir.path().join("home");
    let builds = Builds {
        binary: None,
        home: Some(home.clone()),
    };
    let spec = BuildSpec {
        context: context.clone(),
        tag: Some("api-build-test:1".into()),
        ..BuildSpec::default()
    };
    let mut events = Vec::new();
    let pinned = builds.build(&spec, &mut |e| events.push(e)).unwrap();
    println!("built {pinned:?}, {} events", events.len());
    let hex = pinned.digest.strip_prefix("sha256:").expect("a sha256 digest");
    assert_eq!(hex.len(), 64, "{pinned:?}");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BuildEvent::Step { name, .. } if name.contains("COPY hello"))),
        "{events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            BuildEvent::Ended {
                how: StepEnd::Done,
                ..
            }
        )),
        "{events:?}"
    );
    assert!(home.is_dir(), "the build wrote to the caller's home");
    let broken = BuildSpec {
        file: Some(context.join("Broken")),
        ..spec
    };
    match builds.build(&broken, &mut |_| {}) {
        Err(BuildRefusal::Step { name, message, .. }) => {
            println!("refused at {name:?}: {message}");
            assert!(name.contains("COPY missing"), "{name}");
        }
        other => panic!("{other:?}"),
    }
}

/// A USTAR archive of `entries` (POSIX.1-1988 ustar, as pax(1) writes it): each a path, its
/// mode, and its bytes, or none for a directory, every time `MTIME`, so that what the build
/// unpacks of it carries the archive's times alone.
fn ustar(entries: &[(&str, u32, Option<&[u8]>)]) -> Vec<u8> {
    const MTIME: u64 = 1_700_000_000;
    let mut out = Vec::new();
    for (name, mode, bytes) in entries {
        let mut h = [0u8; 512];
        let field = |h: &mut [u8; 512], at: usize, len: usize, text: &[u8]| {
            h[at..at + text.len().min(len)].copy_from_slice(&text[..text.len().min(len)]);
        };
        let octal = |n: u64, len: usize| format!("{n:0width$o}", width = len - 1).into_bytes();
        let size = bytes.map_or(0, |b| b.len() as u64);
        field(&mut h, 0, 100, name.as_bytes());
        field(&mut h, 100, 8, &octal(u64::from(*mode), 8));
        field(&mut h, 108, 8, &octal(0, 8));
        field(&mut h, 116, 8, &octal(0, 8));
        field(&mut h, 124, 12, &octal(size, 12));
        field(&mut h, 136, 12, &octal(MTIME, 12));
        h[156] = if bytes.is_some() { b'0' } else { b'5' };
        field(&mut h, 257, 6, b"ustar\0");
        field(&mut h, 263, 2, b"00");
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        field(&mut h, 148, 8, format!("{sum:06o}\0 ").as_bytes());
        out.extend_from_slice(&h);
        if let Some(b) = bytes {
            out.extend_from_slice(b);
            out.resize(out.len().next_multiple_of(512), 0);
        }
    }
    out.resize(out.len() + 1024, 0);
    out
}

/// Serves `body` to every GET on 127.0.0.1, one connection at a time: its port.
fn serve(body: Vec<u8>) -> u16 {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && conn.read_exact(&mut byte).is_ok() {
                head.push(byte[0]);
            }
            let _ = write!(
                conn,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = conn.write_all(&body);
        }
    });
    port
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let hex: String = sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
}

/// A harness built alone from an http source pinned by its checksum (D270), as its directive
/// is built in a whole build: an OSI artifact in the caller's home's store, its digest the same
/// for the same pin (every entry's time is the archive's); a source unpinned, refused in the
/// build's words; a context path, refused naming why. Owed to ROOT on HVF, KVM and WHP.
#[test]
fn a_component_builds_alone_from_its_directives_pinned_source() {
    if !asked() {
        return;
    }
    let dir = shards_testdir::TempDir::new("api-component").unwrap();
    let archive = ustar(&[
        (
            "harness.json",
            0o644,
            Some(br#"{"entrypoint": "codex", "options": ["--version"]}"#.as_slice()),
        ),
        ("bin/", 0o755, None),
        ("bin/codex", 0o755, Some(b"#!/bin/sh\necho codex v1\n".as_slice())),
    ]);
    let port = serve(archive.clone());
    let builds = Builds {
        binary: None,
        home: Some(dir.path().join("home")),
    };
    let spec = ComponentSpec {
        kind: ComponentKind::Harness,
        name: "codex".into(),
        source: format!("http://127.0.0.1:{port}/codex.tar"),
        checksum: Some(sha256(&archive)),
        redirects: Vec::new(),
        s3: S3Source::default(),
        platform: None,
        tag: "api-component-test:1".into(),
        secrets: Vec::new(),
        ssh: Vec::new(),
    };
    let mut events = Vec::new();
    let first = builds.component(&spec, &mut |e| events.push(e)).unwrap();
    println!("built {first:?}, {} events", events.len());
    let hex = first.digest.strip_prefix("sha256:").expect("a sha256 digest");
    assert_eq!(hex.len(), 64, "{first:?}");
    assert_eq!(builds.component(&spec, &mut |_| {}).unwrap(), first);
    let unpinned = ComponentSpec {
        checksum: None,
        ..spec.clone()
    };
    match builds.component(&unpinned, &mut |_| {}) {
        Err(BuildRefusal::Failed(message)) => assert!(message.contains("--checksum"), "{message}"),
        other => panic!("{other:?}"),
    }
    let path = ComponentSpec {
        source: "./codex".into(),
        checksum: None,
        ..spec
    };
    match builds.component(&path, &mut |_| {}) {
        Err(BuildRefusal::Failed(message)) => assert!(message.contains("context path"), "{message}"),
        other => panic!("{other:?}"),
    }
}
