//! An OSI artifact (AGENT, HARNESS or MCP; D54) from the OCI layout a named build
//! context gives it as (D113), laid out as shards' frontend has BuildKit lay it out: in an
//! exec step whose root is the frontend's own image, with no network. BuildKit fetches
//! each layer blob from the client's layout by digest (`oci-layout+blob`) and mounts it;
//! `shards frontend osi SPEC` holds each to what an artifact's layer may hold, as
//! `shards build` holds one (agent::check_layer), and applies them in order, as an
//! image's layers are applied, at `/out`.
//!
//! SPEC is a JSON object, `{"kind": "agent", "layers": [{"digest", "mediaType"}...]}`, the
//! layers mounted at `/layers/<i>/blob`. A refusal, or nothing, is written to
//! `/report/finding`, so that the build fails in `shards build`'s words.

use std::io::{self, Write as _};
use std::path::Path;
use std::process::ExitCode;

use shards_image::osi::Kind;

/// The step's spec, as the frontend writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Spec {
    pub kind: Kind,
    /// Each layer's digest and media type, in the manifest's order.
    pub layers: Vec<(String, String)>,
}

impl Spec {
    pub(crate) fn json(&self) -> String {
        let layers: Vec<serde_json::Value> = self
            .layers
            .iter()
            .map(|(d, m)| serde_json::json!({ "digest": d, "mediaType": m }))
            .collect();
        serde_json::json!({ "kind": self.kind.word(), "layers": layers }).to_string()
    }

    fn read(text: &str) -> Result<Spec, String> {
        let v: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("osi spec: {e}"))?;
        let kind = match v.get("kind").and_then(|k| k.as_str()) {
            Some("agent") => Kind::Agent,
            Some("harness") => Kind::Harness,
            Some("mcp") => Kind::Mcp,
            other => return Err(format!("osi spec: no kind {other:?}")),
        };
        let layers = v
            .get("layers")
            .and_then(|l| l.as_array())
            .ok_or("osi spec: no layers")?
            .iter()
            .map(|l| {
                let s = |k: &str| l.get(k).and_then(|v| v.as_str()).map(str::to_string);
                Ok((
                    s("digest").ok_or("osi spec: a layer without its digest")?,
                    s("mediaType").ok_or("osi spec: a layer without its media type")?,
                ))
            })
            .collect::<Result<_, String>>()?;
        Ok(Spec { kind, layers })
    }
}

/// `shards frontend osi SPEC`, inside BuildKit: the content laid out at `/out`, a refusal
/// written to `/report/finding`; a spec that does not read fails the step.
pub(crate) fn run(spec: &str) -> ExitCode {
    let spec = match Spec::read(spec) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(io::stderr(), "shards frontend osi: {e}");
            return ExitCode::FAILURE;
        }
    };
    let finding = lay_out(&spec, Path::new("/layers"), Path::new("/out"))
        .err()
        .unwrap_or_default();
    match std::fs::write("/report/finding", finding) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(io::stderr(), "shards frontend osi: /report/finding: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The layers under `layers` held to what an artifact's layer may hold, every one before
/// any is applied, then applied in order over `out`.
pub(crate) fn lay_out(spec: &Spec, layers: &Path, out: &Path) -> Result<(), String> {
    let open = |i: usize, digest: &str| {
        let p = layers.join(i.to_string()).join("blob");
        std::fs::File::open(&p)
            .map(io::BufReader::new)
            .map_err(|e| format!("{digest}: {e}"))
    };
    for (i, (digest, media)) in spec.layers.iter().enumerate() {
        // Each blob is the one its digest names, as a fetch into the store holds one.
        let got = sha256_of(open(i, digest)?).map_err(|e| format!("{digest}: {e}"))?;
        if &got != digest {
            return Err(format!("got digest {got}, expected {digest}"));
        }
        crate::agent::check_layer(spec.kind, media, digest, open(i, digest)?)?;
    }
    for (i, (digest, media)) in spec.layers.iter().enumerate() {
        let reader = crate::agent::layer_reader(spec.kind, media, digest, open(i, digest)?)?;
        shards_archive::apply_layer(reader, out, &shards_archive::UnpackOptions::default())
            .map_err(|e| format!("{digest}: {e}"))?;
    }
    // The tree's root, which no layer names, at the time `shards build` gives an
    // artifact's: the epoch. A COPY of the tree takes its root's time for where it goes.
    let epoch = std::fs::FileTimes::new()
        .set_accessed(std::time::UNIX_EPOCH)
        .set_modified(std::time::UNIX_EPOCH);
    std::fs::File::open(out)
        .and_then(|f| f.set_times(epoch))
        .map_err(|e| format!("{}: {e}", out.display()))?;
    Ok(())
}

/// `sha256:` and the hex of what `r` reads.
pub(crate) fn sha256_of(mut r: impl io::Read) -> io::Result<String> {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(buf.get(..n).unwrap_or_default());
    }
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("sha256:{hex}"))
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A tar of `entries`: (name, typeflag, mode, linkname, content).
    fn tar(entries: &[(&str, u8, i64, &str, &[u8])]) -> Vec<u8> {
        use shards_archive::tar::{Header, Writer};
        let mut w = Writer::new(Vec::new());
        for (name, typeflag, mode, link, data) in entries {
            let h = Header {
                name: name.as_bytes().to_vec(),
                typeflag: *typeflag,
                mode: *mode,
                size: data.len() as i64,
                linkname: link.as_bytes().to_vec(),
                mtime: shards_archive::tar::Time::unix(1_700_000_000, 0),
                // This test's own: a layer applied as root keeps the archive's owners,
                // and only root may give a file another's.
                // SAFETY: plain getters.
                uid: i64::from(unsafe { libc::getuid() }),
                // SAFETY: as above.
                gid: i64::from(unsafe { libc::getgid() }),
                ..Header::default()
            };
            w.write_header(&h).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap()
    }

    /// The spec of the blobs under `layers`, each by its own digest.
    fn spec_of(layers: &Path, n: usize) -> Spec {
        Spec {
            kind: Kind::Agent,
            layers: (0..n)
                .map(|i| {
                    let blob = std::fs::File::open(layers.join(i.to_string()).join("blob")).unwrap();
                    (
                        sha256_of(blob).unwrap(),
                        "application/vnd.osi.agent.content.v1.tar".into(),
                    )
                })
                .collect(),
        }
    }

    /// A spec reads back as it was written.
    #[test]
    fn a_spec_reads_back_as_written() {
        let s = Spec {
            kind: Kind::Harness,
            layers: vec![(
                "sha256:00".into(),
                "application/vnd.osi.harness.content.v1.tar+gzip".into(),
            )],
        };
        assert_eq!(Spec::read(&s.json()).unwrap(), s);
        assert!(Spec::read(r#"{"kind":"image","layers":[]}"#).is_err());
    }

    /// An artifact's layers are laid out in order, each held first to what an artifact
    /// may hold, in `shards build`'s words: a set-ID file refused, nothing laid out.
    #[test]
    fn layers_are_held_then_laid_out_as_shards_build_holds_them() {
        let root = std::env::temp_dir().join(format!("shards-osi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let layers = root.join("layers");
        let out = root.join("out");
        std::fs::create_dir_all(layers.join("0")).unwrap();
        std::fs::create_dir_all(layers.join("1")).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(
            layers.join("0/blob"),
            tar(&[
                ("bin/", b'5', 0o755, "", b""),
                ("bin/run", b'0', 0o755, "", b"#!/bin/sh\n"),
            ]),
        )
        .unwrap();
        std::fs::write(
            layers.join("1/blob"),
            tar(&[("data.txt", b'0', 0o644, "", b"data\n")]),
        )
        .unwrap();
        lay_out(&spec_of(&layers, 2), &layers, &out).unwrap();
        assert_eq!(std::fs::read(out.join("bin/run")).unwrap(), b"#!/bin/sh\n");
        assert_eq!(std::fs::read(out.join("data.txt")).unwrap(), b"data\n");
        // The root at the epoch, as shards build gives an artifact's.
        assert_eq!(
            std::fs::metadata(&out).unwrap().modified().unwrap(),
            std::time::UNIX_EPOCH
        );

        // A blob not the one its digest names: refused, nothing laid out.
        let held = spec_of(&layers, 2);
        std::fs::write(
            layers.join("1/blob"),
            tar(&[("data.txt", b'0', 0o644, "", b"other\n")]),
        )
        .unwrap();
        let other = spec_of(&layers, 2);
        let out1 = root.join("out1");
        std::fs::create_dir_all(&out1).unwrap();
        assert_eq!(
            lay_out(&held, &layers, &out1),
            Err(format!(
                "got digest {}, expected {}",
                other.layers[1].0, held.layers[1].0
            ))
        );
        assert!(
            std::fs::read_dir(&out1).unwrap().next().is_none(),
            "nothing laid out"
        );

        let out2 = root.join("out2");
        std::fs::create_dir_all(&out2).unwrap();
        std::fs::write(layers.join("1/blob"), tar(&[("bin/su", b'0', 0o4755, "", b"x")])).unwrap();
        assert_eq!(
            lay_out(&spec_of(&layers, 2), &layers, &out2),
            Err("bin/su: set-ID bits, which nothing in an agent's directory may have".to_string())
        );
        assert!(
            std::fs::read_dir(&out2).unwrap().next().is_none(),
            "nothing laid out"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
