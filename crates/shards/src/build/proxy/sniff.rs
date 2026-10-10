//! What Go's server says a response's body is where its handler said nothing (D110):
//! net/http's `DetectContentType` (Go 1.26.3, sniff.go), the signatures of the WHATWG MIME
//! Sniffing Standard in Go's order, read in at most the first 512 bytes. BuildKit's proxy
//! passes an upstream's fields on to Go's server, which sniffs a body that came with no
//! `Content-Type` (nor a `Content-Encoding`), and so does shards' proxy.

/// The most of a body read to say what it is (`sniffLen`).
pub const LEN: usize = 512;

enum Sig {
    /// An HTML tag, its letters of any case, then a tag-terminating byte.
    Html(&'static [u8]),
    /// Bytes that, masked, are the pattern; whitespace first passed over where `skip_ws`.
    Masked {
        mask: &'static [u8],
        pat: &'static [u8],
        skip_ws: bool,
        ct: &'static str,
    },
    /// Bytes the data starts with.
    Exact(&'static [u8], &'static str),
    /// An MP4 `ftyp` box naming an `mp4` brand.
    Mp4,
    /// No byte of a binary file's.
    Text,
}

const fn masked(mask: &'static [u8], pat: &'static [u8], ct: &'static str) -> Sig {
    Sig::Masked {
        mask,
        pat,
        skip_ws: false,
        ct,
    }
}

const RIFF_MASK: &[u8] = b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF";

/// `sniffSignatures`.
const SIGS: &[Sig] = &[
    Sig::Html(b"<!DOCTYPE HTML"),
    Sig::Html(b"<HTML"),
    Sig::Html(b"<HEAD"),
    Sig::Html(b"<SCRIPT"),
    Sig::Html(b"<IFRAME"),
    Sig::Html(b"<H1"),
    Sig::Html(b"<DIV"),
    Sig::Html(b"<FONT"),
    Sig::Html(b"<TABLE"),
    Sig::Html(b"<A"),
    Sig::Html(b"<STYLE"),
    Sig::Html(b"<TITLE"),
    Sig::Html(b"<B"),
    Sig::Html(b"<BODY"),
    Sig::Html(b"<BR"),
    Sig::Html(b"<P"),
    Sig::Html(b"<!--"),
    Sig::Masked {
        mask: b"\xFF\xFF\xFF\xFF\xFF",
        pat: b"<?xml",
        skip_ws: true,
        ct: "text/xml; charset=utf-8",
    },
    Sig::Exact(b"%PDF-", "application/pdf"),
    Sig::Exact(b"%!PS-Adobe-", "application/postscript"),
    // UTF BOMs.
    masked(
        b"\xFF\xFF\x00\x00",
        b"\xFE\xFF\x00\x00",
        "text/plain; charset=utf-16be",
    ),
    masked(
        b"\xFF\xFF\x00\x00",
        b"\xFF\xFE\x00\x00",
        "text/plain; charset=utf-16le",
    ),
    masked(
        b"\xFF\xFF\xFF\x00",
        b"\xEF\xBB\xBF\x00",
        "text/plain; charset=utf-8",
    ),
    // Images.
    Sig::Exact(b"\x00\x00\x01\x00", "image/x-icon"),
    Sig::Exact(b"\x00\x00\x02\x00", "image/x-icon"),
    Sig::Exact(b"BM", "image/bmp"),
    Sig::Exact(b"GIF87a", "image/gif"),
    Sig::Exact(b"GIF89a", "image/gif"),
    masked(
        b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF\xFF\xFF",
        b"RIFF\x00\x00\x00\x00WEBPVP",
        "image/webp",
    ),
    Sig::Exact(b"\x89PNG\x0D\x0A\x1A\x0A", "image/png"),
    Sig::Exact(b"\xFF\xD8\xFF", "image/jpeg"),
    // Audio and video, in the standard's order.
    masked(RIFF_MASK, b"FORM\x00\x00\x00\x00AIFF", "audio/aiff"),
    masked(b"\xFF\xFF\xFF", b"ID3", "audio/mpeg"),
    masked(b"\xFF\xFF\xFF\xFF\xFF", b"OggS\x00", "application/ogg"),
    masked(
        b"\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF",
        b"MThd\x00\x00\x00\x06",
        "audio/midi",
    ),
    masked(RIFF_MASK, b"RIFF\x00\x00\x00\x00AVI ", "video/avi"),
    masked(RIFF_MASK, b"RIFF\x00\x00\x00\x00WAVE", "audio/wave"),
    Sig::Mp4,
    Sig::Exact(b"\x1A\x45\xDF\xA3", "video/webm"),
    // Fonts: 34 bytes of anything, then "LP".
    masked(
        b"\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\xFF\xFF",
        b"\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0LP",
        "application/vnd.ms-fontobject",
    ),
    Sig::Exact(b"\x00\x01\x00\x00", "font/ttf"),
    Sig::Exact(b"OTTO", "font/otf"),
    Sig::Exact(b"ttcf", "font/collection"),
    Sig::Exact(b"wOFF", "font/woff"),
    Sig::Exact(b"wOF2", "font/woff2"),
    // Archives.
    Sig::Exact(b"\x1F\x8B\x08", "application/x-gzip"),
    Sig::Exact(b"PK\x03\x04", "application/zip"),
    Sig::Exact(b"Rar!\x1A\x07\x00", "application/x-rar-compressed"),
    Sig::Exact(b"Rar!\x1A\x07\x01\x00", "application/x-rar-compressed"),
    Sig::Exact(b"\x00\x61\x73\x6D", "application/wasm"),
    Sig::Text,
];

/// Whitespace as the standard has it (0xWS).
fn is_ws(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | 0x0c | b'\r' | b' ')
}

impl Sig {
    fn matches(&self, data: &[u8], first_non_ws: usize) -> Option<&'static str> {
        let rest = data.get(first_non_ws..).unwrap_or_default();
        match self {
            Sig::Html(h) => {
                if rest.len() < h.len() + 1 {
                    return None;
                }
                let same = h.iter().zip(rest).all(|(&b, &d)| {
                    let d = if b.is_ascii_uppercase() { d & 0xDF } else { d };
                    b == d
                });
                // A tag-terminating byte (0xTT) after it.
                (same && matches!(rest.get(h.len()), Some(b' ' | b'>'))).then_some("text/html; charset=utf-8")
            }
            Sig::Masked {
                mask,
                pat,
                skip_ws,
                ct,
            } => {
                let data = if *skip_ws { rest } else { data };
                if mask.len() != pat.len() || data.len() < pat.len() {
                    return None;
                }
                pat.iter()
                    .zip(mask.iter())
                    .zip(data)
                    .all(|((&p, &m), &d)| d & m == p)
                    .then_some(*ct)
            }
            Sig::Exact(sig, ct) => data.starts_with(sig).then_some(*ct),
            Sig::Mp4 => {
                let size = data
                    .get(..4)
                    .and_then(|b| <[u8; 4]>::try_from(b).ok())
                    .map(|b| u32::from_be_bytes(b) as usize)?;
                if data.len() < 12 || data.len() < size || size % 4 != 0 || data.get(4..8) != Some(b"ftyp") {
                    return None;
                }
                // The major brand's version (bytes 12 to 16) is passed over.
                (8..size)
                    .step_by(4)
                    .filter(|&at| at != 12)
                    .any(|at| data.get(at..at + 3) == Some(b"mp4"))
                    .then_some("video/mp4")
            }
            Sig::Text => (!rest.iter().any(|&b| {
                b <= 0x08 || b == 0x0B || (0x0E..=0x1A).contains(&b) || (0x1C..=0x1F).contains(&b)
            }))
            .then_some("text/plain; charset=utf-8"),
        }
    }
}

/// `DetectContentType`: what `data` is, by at most its first [`LEN`] bytes; else
/// `application/octet-stream`.
pub fn content_type(data: &[u8]) -> &'static str {
    let data = data.get(..LEN).unwrap_or(data);
    let first_non_ws = data.iter().position(|&b| !is_ws(b)).unwrap_or(data.len());
    SIGS.iter()
        .find_map(|s| s.matches(data, first_non_ws))
        .unwrap_or("application/octet-stream")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Held to Go 1.26.3's DetectContentType, as scripts/proxy/generate records it.
    #[test]
    fn bodies_are_sniffed_as_go_sniffs_them() {
        use base64::Engine as _;
        let oracle: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/proxy-oracle.json")).unwrap_or_default();
        let cases = oracle
            .get("sniff")
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(cases.len() >= 80, "{}", cases.len());
        for case in cases {
            let data = match case.get("data") {
                Some(serde_json::Value::String(s)) => s.as_bytes().to_vec(),
                Some(v) => base64::engine::general_purpose::STANDARD
                    .decode(v.get("base64").and_then(|b| b.as_str()).unwrap_or_default())
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            assert_eq!(
                Some(content_type(&data)),
                case.get("type").and_then(|t| t.as_str()),
                "{:?}",
                String::from_utf8_lossy(&data)
            );
        }
    }
}
