//! GitHub's attestations of a download, as buildx v0.37.1 reads them for
//! `github_attestation` (policy/funcs.go readGitHubAttestationBundles): the repository's
//! SLSA provenance attestations of the digest from GitHub's API, each bundle inline or
//! behind its `bundle_url` (snappy-compressed where it ends `.json.sn`), each fetched as
//! an HTTP source of its own step.
//!
//! buildx logs a `bundle_url` that fails whole, its signed query included; shards logs it
//! as its step shows it, without the query (D106).

use std::collections::HashMap;

use shards_sigstore::godec::{Dec, Elem, GoSlice};
use shards_sigstore::tlog::gojson::{self, JValue};

use super::snappy;

/// slsa1.PredicateSLSAProvenance.
const SLSA_V1: &str = "https://slsa.dev/provenance/v1";

/// What fetches a source: its step's name, its URL and the `Accept` it asks with.
pub type Fetch<'a> = dyn Fn(String, String, Option<&'static str>) -> Result<Vec<u8>, String> + 'a;

/// readGitHubAttestationBundles: the bundles GitHub holds for `repo`'s attestations of
/// `dgst`, inline ones first; `log` says what was found and what failed.
pub fn bundles(
    repo: &str,
    dgst: &str,
    fetch: &Fetch<'_>,
    log: &dyn Fn(String),
) -> Result<Vec<Vec<u8>>, String> {
    let escaped =
        String::from_utf8_lossy(&shards_dockerfile::url::query_escape(SLSA_V1.as_bytes())).into_owned();
    let u = format!("https://api.github.com/repos/{repo}/attestations/{dgst}?predicate_type={escaped}");
    let raw = fetch(
        format!("[policy] fetch GitHub attestation {repo}@{dgst}"),
        u.clone(),
        Some("application/vnd.github+json"),
    )
    .map_err(|e| format!("read GitHub attestation response: {e}"))?;
    let (mut bundles, urls) = from_response(&raw);
    log(format!(
        "fetched {} inline bundles and {} bundle URLs from {u}",
        bundles.len(),
        urls.len()
    ));
    for bu in urls {
        let shown = strip_raw_query(&bu);
        let got = match fetch(
            format!("[policy] fetch GitHub attestation bundle {shown}"),
            bu.clone(),
            None,
        ) {
            Ok(b) => b,
            Err(e) => {
                log(format!("failed reading bundle_url {shown}: {e}"));
                continue;
            }
        };
        let got = shards_dockerfile::go::trim_space(&got);
        if got.is_empty() || got == b"null" {
            continue;
        }
        if should_decode_snappy(&bu) {
            match snappy::decode(got) {
                Ok(d) => bundles.push(d),
                Err(e) => log(format!("failed decoding snappy bundle_url {shown}: {e}")),
            }
        } else {
            bundles.push(got.to_vec());
        }
    }
    Ok(bundles)
}

/// An attestation as buildx decodes it: its bundle (a json.RawMessage) and its URL.
#[derive(Default)]
struct Attestation<'r> {
    bundle: Option<&'r [u8]>,
    bundle_url: String,
}

/// struct { Bundle json.RawMessage; BundleURL string } on a 64-bit Go: 24 and 16 bytes.
const ATTESTATION: Elem = Elem {
    size: 40,
    noscan: false,
};

/// githubAttestationBundlesFromResponse: the inline bundles and the bundle URLs of the
/// API's answer, as json.Unmarshal reads it into buildx's struct; none where it cannot.
pub fn from_response(raw: &[u8]) -> (Vec<Vec<u8>>, Vec<String>) {
    let t = shards_dockerfile::go::trim_space(raw);
    if t.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let Ok((v, spans)) = gojson::unmarshal_raw(t) else {
        return (Vec::new(), Vec::new());
    };
    // Each value's bytes, by where it is in the tree.
    let mut order = Vec::new();
    gojson::pre_order(&v, &mut order);
    let raw_of: HashMap<*const JValue, &[u8]> = order
        .iter()
        .zip(&spans)
        .filter_map(|(x, &(start, end))| Some((std::ptr::from_ref(*x), t.get(start..end)?)))
        .collect();
    let mut list: GoSlice<Attestation<'_>> = GoSlice::default();
    // Any error leaves no bundles, so the types' names in its words go unread.
    let mut d = Dec::new();
    d.object(
        &v,
        "",
        "struct",
        &[("attestations", "attestations")],
        |d, _, x| {
            d.slice(x, &mut list, "[]struct", ATTESTATION, |d, e, a| {
                let fields = [
                    ("bundle", "attestations.bundle"),
                    ("bundle_url", "attestations.bundle_url"),
                ];
                d.object(e, "", "struct", &fields, |d, i, m| match i {
                    0 => a.bundle = raw_of.get(&std::ptr::from_ref(m)).copied(),
                    _ => d.string(m, &mut a.bundle_url, "string"),
                });
            });
        },
    );
    if !d.ok() {
        return (Vec::new(), Vec::new());
    }
    let mut out = Vec::new();
    let mut urls = Vec::new();
    for a in list.items() {
        // A value's span has no white space about it, which buildx trims.
        if let Some(b) = a.bundle
            && !b.is_empty()
            && b != b"null"
        {
            out.push(b.to_vec());
        }
        if !a.bundle_url.is_empty() {
            urls.push(a.bundle_url.clone());
        }
    }
    (out, urls)
}

/// shouldDecodeSnappyBundleURL: whether the URL's path ends `.json.sn`.
pub fn should_decode_snappy(raw: &str) -> bool {
    shards_dockerfile::url::parse(raw.as_bytes()).is_ok_and(|u| u.path.ends_with(b".json.sn"))
}

/// stripRawQuery: the URL without its query, as url.URL prints it.
pub fn strip_raw_query(raw: &str) -> String {
    match shards_dockerfile::url::parse(raw.as_bytes()) {
        Ok(mut u) => {
            u.raw_query.clear();
            String::from_utf8_lossy(&u.string()).into_owned()
        }
        Err(_) => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use base64::Engine as _;

    use super::*;

    fn cases() -> serde_json::Value {
        serde_json::from_str(include_str!("../testdata/github-attestation.json")).unwrap()
    }

    /// Every block golang/snappy decodes, and every one it refuses, as it does: what
    /// `Encode` makes, those cut short, grown and changed, and each tag's every form.
    #[test]
    fn snappy_blocks_decode_as_golang_snappy_decodes_them() {
        let b64 = base64::engine::general_purpose::STANDARD;
        for c in cases()["snappy"].as_array().unwrap() {
            let input = b64.decode(c["in"].as_str().unwrap()).unwrap();
            let want = match c["out"].as_str() {
                Some(out) => Ok(b64.decode(out).unwrap()),
                None => Err(c["err"].as_str().unwrap()),
            };
            assert_eq!(snappy::decode(&input), want, "{}", c["in"]);
        }
    }

    /// The API's answers as buildx's struct reads them: names folded, members repeated
    /// into what an earlier one decoded, raw bundles as written, and nothing at all from
    /// one with an error anywhere.
    #[test]
    fn responses_are_read_as_buildx_reads_them() {
        for c in cases()["responses"].as_array().unwrap() {
            let (bundles, urls) = from_response(c["in"].as_str().unwrap().as_bytes());
            let bundles: Vec<String> = bundles
                .into_iter()
                .map(|b| String::from_utf8(b).unwrap())
                .collect();
            let want = |k: &str| -> Vec<String> {
                c[k].as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_string())
                    .collect()
            };
            assert_eq!((bundles, urls), (want("bundles"), want("urls")), "{}", c["in"]);
        }
    }

    #[test]
    fn bundle_urls_are_read_as_buildx_reads_them() {
        for c in cases()["urls"].as_array().unwrap() {
            let url = c["url"].as_str().unwrap();
            assert_eq!(should_decode_snappy(url), c["snappy"].as_bool().unwrap(), "{url}");
            assert_eq!(strip_raw_query(url), c["stripped"].as_str().unwrap(), "{url}");
        }
    }
}
