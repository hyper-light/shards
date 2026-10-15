//! Images found in registries (D172), each registry asked as it documents:
//! - Docker Hub: its index's search (`/v1/search?q=&n=`), as dockerd asks it for `GET
//!   /images/search` (moby daemon/pkg/registry/search.go). A password login goes as
//!   Basic credentials, and an identity token as a bearer token for
//!   `registry:catalog:search`, which distribution's authorizer takes from the
//!   challenges of the index's `/v2/` (registry/client/auth/session.go, ModifyRequest);
//! - quay.io: its own search (`/api/v1/find/repositories`, in its API description);
//! - any other registry: its catalog (`/v2/_catalog`, distribution's), each repository
//!   whose name holds the term. dockerd asks every registry but Docker Hub for an
//!   index's search, which no registry serves now: a 404.
//!
//! And whether an image runs on this machine, as a pull would choose it
//! (`shards_image::platform`), and what its pull takes: from Docker Hub's API (a tag's
//! images, their platforms and sizes in one answer:
//! `/v2/namespaces/NS/repositories/REPO/tags/TAG`), or from the tag's manifests.

use std::io::Read as _;

use serde::Deserialize;
use shards_image::oci::{Descriptor, Index, Manifest, Platform, media};
use shards_image::platform::{self, Target};
use shards_image::reference::Reference;

use crate::auth::{Credentials, basic};
use crate::http::{Client, Redirects, Request};
use crate::registry::Registry;
use crate::url::Url;
use crate::{Error, printable};

/// Docker Hub's index, where its search is asked.
pub const HUB_INDEX: &str = "https://index.docker.io/v1/";
/// Docker Hub's API, where a tag's images are described.
pub const HUB_API: &str = "https://hub.docker.com/v2/";
/// The most of a search's answer read: a bound against a server that never ends one, a
/// page of tags' (`MAX_TAGS_PAGE`). The largest measured: Docker Hub's index, 100
/// results, 21 KB; quay.io's, a page of 10 with their READMEs, 54 KB; Docker Hub's tag
/// of `python:latest`, 6 KB (2026-10-14).
const MOST: u64 = 4 << 20;
/// The most of an image's config read for its platform: a manifest's bound.
const MOST_CONFIG: u64 = shards_image::oci::MAX_MANIFEST;

/// A repository found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    /// As `shards pull` takes it: Docker Hub's as its index names it, any other's with its
    /// host.
    pub name: String,
    pub description: String,
    pub stars: i64,
    pub official: bool,
    /// How many times it was pulled, where its registry counts it (Docker Hub's index
    /// does; the Engine API's result has no field for it).
    pub pulls: Option<i64>,
}

/// The registry a term is searched in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Searched {
    Hub,
    Quay,
    /// A registry's catalog, at this host.
    Catalog(String),
}

/// splitReposSearchTerm: the registry a term names (its first part, where that has a `.`
/// or `:` or is `localhost`; Docker Hub's otherwise), and what is searched for in it.
/// Docker Hub's official images are searched by their own names, without `library/`.
pub fn searched(term: &str) -> (Searched, &str) {
    fn hub(rest: &str) -> &str {
        rest.strip_prefix("library/").unwrap_or(rest)
    }
    let Some((first, rest)) = term
        .split_once('/')
        .filter(|(first, _)| first.contains('.') || first.contains(':') || *first == "localhost")
    else {
        return (Searched::Hub, hub(term));
    };
    match first {
        "docker.io" | "index.docker.io" | "registry-1.docker.io" => (Searched::Hub, hub(rest)),
        "quay.io" => (Searched::Quay, rest),
        host => (Searched::Catalog(host.to_string()), rest),
    }
}

/// Reads at most [`MOST`] of `response`, refusing more: what `url` answered.
fn answer(response: &mut crate::http::Response<'_>, url: &Url) -> Result<Vec<u8>, Error> {
    let mut body = Vec::new();
    response.take(MOST.saturating_add(1)).read_to_end(&mut body)?;
    if body.len() as u64 > MOST {
        return Err(Error::new(format!("{url}: an answer past {MOST} bytes")));
    }
    Ok(body)
}

/// What a server said of a refusal, where its body says anything: the first of its JSON
/// error fields, else its text, shown safely and cut short.
fn said(body: &[u8]) -> String {
    let value: Option<serde_json::Value> = serde_json::from_slice(body).ok();
    let field = value.as_ref().and_then(|v| {
        ["message", "error", "detail", "details"]
            .iter()
            .find_map(|k| v.get(*k).and_then(serde_json::Value::as_str))
            .or_else(|| v.pointer("/errors/0/message").and_then(serde_json::Value::as_str))
            .map(str::to_string)
    });
    let text = field.unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string());
    let shown = printable(&text);
    match shown.char_indices().nth(200) {
        Some((cut, _)) => format!("{}…", shown.get(..cut).unwrap_or_default()),
        None => shown,
    }
}

/// moby's refusal (`unexpected status code N`), with what the server said of it, which
/// moby leaves out (its TODO, search.go).
fn refused(status: u16, body: &[u8]) -> Error {
    let words = said(body);
    if words.is_empty() {
        Error::new(format!("unexpected status code {status}"))
    } else {
        Error::new(format!("unexpected status code {status}: {words}"))
    }
}

/// Docker Hub's index search at `index`, as many as `limit`: what its index finds of
/// `term`, with `credentials` as dockerd sends them.
pub fn hub(
    http: Client,
    index: &Url,
    term: &str,
    limit: usize,
    credentials: &Credentials,
) -> Result<Vec<Found>, Error> {
    #[derive(Deserialize)]
    struct Page {
        #[serde(default)]
        results: Option<Vec<Hit>>,
    }
    #[derive(Deserialize)]
    struct Hit {
        #[serde(default)]
        name: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        star_count: i64,
        #[serde(default)]
        is_official: bool,
        #[serde(default)]
        pull_count: Option<i64>,
    }
    let url = index
        .join("search")?
        .with_query_pair("q", term)?
        .with_query_pair("n", &limit.to_string())?;
    // The challenges of the index's own `/v2/`, for a bearer token: what distribution's
    // authorizer answers a `/v1/` request with.
    let registry = Registry::of_host(
        http,
        index.join("/v2/")?,
        credentials.clone(),
        vec!["registry:catalog:search".into()],
    )?;
    let bearer = match credentials {
        Credentials::IdentityToken(_) => {
            let base = index.join("/v2/")?;
            let (mut pinged, _) = registry.request("GET", &base, &[])?;
            let _ = answer(&mut pinged, &base);
            registry.authorization(&url)?
        }
        _ => None,
    };
    let authorization = match credentials {
        Credentials::Password { username, password } if !username.is_empty() => {
            Some(basic(username.as_bytes(), password.as_bytes()))
        }
        _ => bearer,
    };
    let headers = [("X-Docker-Token", "true"), ("Accept", "application/json")];
    let mut response = registry.client().follow(
        &Request {
            method: "GET",
            url: &url,
            headers: &headers,
            body: &[],
            file: None,
        },
        &|hop| Ok(authorization.clone().filter(|_| hop.same_origin(&url))),
        Redirects::SameOrigin,
    )?;
    let body = answer(&mut response, &url)?;
    if response.status != 200 {
        return Err(refused(response.status, &body));
    }
    let page: Page = serde_json::from_slice(&body)
        .map_err(|e| Error::new(format!("error decoding registry search results: {e}")))?;
    Ok(page
        .results
        .unwrap_or_default()
        .into_iter()
        .map(|h| Found {
            name: h.name,
            description: h.description.unwrap_or_default(),
            stars: h.star_count,
            official: h.is_official,
            pulls: h.pull_count,
        })
        .collect())
}

/// quay.io's search at `api` (`https://quay.io/api/v1/`), as many as `limit`, page by
/// page as it says it has more: the public repositories it finds of `term`, named with
/// `host`, each described by the first paragraph of its README.
pub fn quay(http: Client, api: &Url, host: &str, term: &str, limit: usize) -> Result<Vec<Found>, Error> {
    #[derive(Deserialize)]
    struct Page {
        #[serde(default)]
        results: Vec<Hit>,
        #[serde(default)]
        has_additional: bool,
    }
    #[derive(Deserialize)]
    struct Hit {
        #[serde(default)]
        kind: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        namespace: Option<Namespace>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        stars: Option<i64>,
    }
    #[derive(Deserialize)]
    struct Namespace {
        #[serde(default)]
        name: String,
    }
    let base = api.join("find/repositories")?.with_query_pair("query", term)?;
    let registry = Registry::of_host(http, api.join("/v2/")?, Credentials::Anonymous, Vec::new())?;
    let mut found = Vec::new();
    let mut page = 1usize;
    while found.len() < limit {
        let url = base.with_query_pair("page", &page.to_string())?;
        let (mut response, _) = registry.request("GET", &url, &[("Accept", "application/json")])?;
        let body = answer(&mut response, &url)?;
        if response.status != 200 {
            return Err(refused(response.status, &body));
        }
        let read: Page = serde_json::from_slice(&body)
            .map_err(|e| Error::new(format!("error decoding registry search results: {e}")))?;
        let before = found.len();
        found.extend(
            read.results
                .into_iter()
                .filter(|h| h.kind == "repository" && !h.name.is_empty())
                .filter_map(|h| {
                    let namespace = h.namespace.map(|n| n.name).filter(|n| !n.is_empty())?;
                    Some(Found {
                        name: format!("{host}/{namespace}/{}", h.name),
                        description: first_paragraph(&h.description.unwrap_or_default()),
                        stars: h.stars.unwrap_or_default(),
                        official: false,
                        pulls: None,
                    })
                }),
        );
        // A page that brings nothing ends it, whatever it says of more.
        if !read.has_additional || found.len() == before {
            break;
        }
        page = page.saturating_add(1);
    }
    found.truncate(limit);
    Ok(found)
}

/// A README's first paragraph, as text: its HTML tags, images and Markdown marks left
/// out, its links' text kept, its lines joined.
fn first_paragraph(markdown: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    for line in markdown.lines() {
        let text = plain(line);
        if text.is_empty() {
            if words.is_empty() {
                continue;
            }
            break;
        }
        words.push(text);
    }
    words.join(" ")
}

/// A line of Markdown as text: tags, images, link targets, references and the marks of
/// headings, quotes, lists and emphasis left out.
fn plain(line: &str) -> String {
    let trimmed = line.trim();
    // A link reference's definition (`[label]: url`) says nothing.
    if trimmed.starts_with('[') && trimmed.contains("]:") {
        return String::new();
    }
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // An HTML tag.
            '<' => {
                for t in chars.by_ref() {
                    if t == '>' {
                        break;
                    }
                }
            }
            // An image: its alt text and target both left out.
            '!' if chars.peek() == Some(&'[') => {
                let _ = chars.next();
                skip_bracketed(&mut chars, '[', ']');
                if chars.peek() == Some(&'(') {
                    let _ = chars.next();
                    skip_bracketed(&mut chars, '(', ')');
                } else if chars.peek() == Some(&'[') {
                    let _ = chars.next();
                    skip_bracketed(&mut chars, '[', ']');
                }
            }
            // A link's target; its text stays.
            ']' if chars.peek() == Some(&'(') => {
                let _ = chars.next();
                skip_bracketed(&mut chars, '(', ')');
            }
            ']' if chars.peek() == Some(&'[') => {
                let _ = chars.next();
                skip_bracketed(&mut chars, '[', ']');
            }
            '[' | ']' | '*' | '_' | '`' => {}
            c => out.push(c),
        }
    }
    out.trim()
        .trim_start_matches(['#', '>', '-', '+', '|'])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Skips to the bracket that closes one just opened, nested ones within it.
fn skip_bracketed(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, open: char, close: char) {
    let mut depth = 1usize;
    for c in chars.by_ref() {
        if c == open {
            depth = depth.saturating_add(1);
        } else if c == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return;
            }
        }
    }
}

/// A registry's catalog at `host`, page by page by its `Link`: the repositories whose
/// names hold `term`, ignoring case, as many as `limit`, with `credentials` for the host
/// as its pulls have them.
pub fn catalog(
    http: Client,
    host: &str,
    term: &str,
    limit: usize,
    credentials: Credentials,
) -> Result<Vec<Found>, Error> {
    #[derive(Deserialize)]
    struct Page {
        #[serde(default)]
        repositories: Option<Vec<String>>,
    }
    let base = Url::parse(&format!("https://{host}/v2/"))?;
    let registry = Registry::of_host(http, base.clone(), credentials, vec!["registry:catalog:*".into()])?;
    let wanted = term.to_lowercase();
    let mut url = base.join("_catalog")?;
    let mut read = std::collections::HashSet::new();
    let mut found = Vec::new();
    loop {
        if !read.insert(url.as_str().to_string()) {
            return Err(Error::new(format!(
                "{url}: the catalog's pages name one another in a loop"
            )));
        }
        let (mut response, _) = registry
            .request("GET", &url, &[("Accept", "application/json")])
            .map_err(|e| e.context(host))?;
        let next = response.header("link").map(|link| {
            link.split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        });
        let body = answer(&mut response, &url)?;
        if response.status != 200 {
            let why = said(&body);
            let status = response.status_text();
            return Err(Error::new(if why.is_empty() {
                format!("{host} serves no catalog to search: {status}")
            } else {
                format!("{host} serves no catalog to search: {status}: {why}")
            }));
        }
        let page: Page = serde_json::from_slice(&body).map_err(|e| Error::new(format!("{url}: {e}")))?;
        for name in page.repositories.unwrap_or_default() {
            if found.len() == limit {
                return Ok(found);
            }
            if name.to_lowercase().contains(&wanted) {
                found.push(Found {
                    name: format!("{host}/{name}"),
                    ..Found::default()
                });
            }
        }
        match next.filter(|n| !n.is_empty()) {
            Some(next) => url = url.join(&next)?,
            None => return Ok(found),
        }
    }
}

/// Whether an image runs on this machine, as a pull would choose its manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Here {
    /// It does: the platform a pull chooses, and its layers' compressed size, what the
    /// pull downloads but for a few small documents.
    Runs { platform: String, size: u64 },
    /// Its images are for these platforms alone.
    Elsewhere(Vec<String>),
    /// It has no such tag.
    Untagged,
}

/// `os/architecture[/variant]`, as OCI platforms are named.
fn named(p: &Platform) -> String {
    match p.variant.as_deref().filter(|v| !v.is_empty()) {
        Some(v) => format!("{}/{}/{v}", p.os, p.architecture),
        None => format!("{}/{}", p.os, p.architecture),
    }
}

/// What an index's images run on, best for `wanted` first, as a pull chooses: the chosen
/// one, or every platform it has. An entry without a platform, or with `unknown`'s (an
/// attestation, as BuildKit labels one), names none.
fn chosen(index: &Index, wanted: &[Target]) -> Result<Descriptor, Vec<String>> {
    let labelled: Vec<Descriptor> = index
        .manifests
        .iter()
        .filter(|d| {
            d.platform
                .as_ref()
                .is_some_and(|p| p.os != "unknown" && p.architecture != "unknown")
        })
        .cloned()
        .collect();
    let labelled_index = Index {
        schema_version: index.schema_version,
        media_type: index.media_type.clone(),
        manifests: labelled,
    };
    match platform::select(&labelled_index, wanted) {
        Some(d) => Ok(d.clone()),
        None => {
            let mut names: Vec<String> = labelled_index
                .manifests
                .iter()
                .filter_map(|d| d.platform.as_ref().map(named))
                .collect();
            names.sort();
            names.dedup();
            Err(names)
        }
    }
}

/// Docker Hub's image `name` at `tag`, from its API at `api`: its tag's images.
pub fn hub_here(http: Client, api: &Url, name: &str, tag: &str, wanted: &[Target]) -> Result<Here, Error> {
    #[derive(Deserialize)]
    struct Tag {
        #[serde(default)]
        images: Option<Vec<Image>>,
    }
    #[derive(Deserialize)]
    struct Image {
        #[serde(default)]
        architecture: String,
        #[serde(default)]
        os: String,
        #[serde(default)]
        variant: Option<String>,
        #[serde(default)]
        size: i64,
        #[serde(default)]
        digest: Option<String>,
    }
    let (namespace, repository) = name.rsplit_once('/').unwrap_or(("library", name));
    let url = api.join(&format!(
        "namespaces/{namespace}/repositories/{repository}/tags/{tag}"
    ))?;
    let registry = Registry::of_host(http, api.clone(), Credentials::Anonymous, Vec::new())?;
    let (mut response, _) = registry.request("GET", &url, &[("Accept", "application/json")])?;
    let body = answer(&mut response, &url)?;
    match response.status {
        200 => {}
        404 => return Ok(Here::Untagged),
        status => return Err(refused(status, &body)),
    }
    let read: Tag = serde_json::from_slice(&body).map_err(|e| Error::new(format!("{url}: {e}")))?;
    let index = Index {
        schema_version: 2,
        media_type: None,
        manifests: read
            .images
            .unwrap_or_default()
            .into_iter()
            .map(|i| Descriptor {
                media_type: String::new(),
                digest: i.digest.unwrap_or_default(),
                size: i.size,
                platform: Some(Platform {
                    architecture: i.architecture,
                    os: i.os,
                    variant: i.variant,
                    os_features: Vec::new(),
                }),
                annotations: std::collections::BTreeMap::new(),
            })
            .collect(),
    };
    Ok(match chosen(&index, wanted) {
        Ok(d) => Here::Runs {
            platform: d.platform.as_ref().map(named).unwrap_or_default(),
            size: d.size().unwrap_or_default(),
        },
        Err(platforms) => Here::Elsewhere(platforms),
    })
}

/// Any other registry's image `reference`: its tag's index or manifest as a pull resolves
/// it, its config where a manifest names no platform.
pub fn here(
    http: Client,
    reference: &Reference,
    credentials: Credentials,
    wanted: &[Target],
) -> Result<Here, Error> {
    let registry = Registry::new(http, reference, credentials)?;
    let tag = reference.tag.as_deref().unwrap_or("latest");
    let Some((kind, bytes)) = registry.document_at(tag)? else {
        return Ok(Here::Untagged);
    };
    let layers = |manifest: &Manifest| {
        manifest
            .layers
            .iter()
            .map(|l| l.size().unwrap_or_default())
            .fold(0u64, u64::saturating_add)
    };
    let indexed = kind == media::OCI_INDEX
        || kind == media::DOCKER_LIST
        || serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .is_some_and(|v| v.get("manifests").is_some());
    if indexed {
        let index: Index = serde_json::from_slice(&bytes).map_err(|e| Error::new(format!("{tag}: {e}")))?;
        return match chosen(&index, wanted) {
            Ok(d) => {
                let (_, bytes) = registry
                    .document_at(&d.digest)?
                    .ok_or_else(|| Error::new(format!("{}: gone from the registry", d.digest)))?;
                let manifest: Manifest =
                    serde_json::from_slice(&bytes).map_err(|e| Error::new(format!("{}: {e}", d.digest)))?;
                Ok(Here::Runs {
                    platform: d.platform.as_ref().map(named).unwrap_or_default(),
                    size: layers(&manifest),
                })
            }
            Err(platforms) => Ok(Here::Elsewhere(platforms)),
        };
    }
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|e| Error::new(format!("{tag}: {e}")))?;
    #[derive(Deserialize)]
    struct Config {
        #[serde(default)]
        architecture: String,
        #[serde(default)]
        os: String,
        #[serde(default)]
        variant: Option<String>,
    }
    let config: Config = serde_json::from_slice(&registry.small_blob(&manifest.config, MOST_CONFIG)?)
        .map_err(|e| Error::new(format!("{}: {e}", manifest.config.digest)))?;
    let p = Platform {
        architecture: config.architecture,
        os: config.os,
        variant: config.variant,
        os_features: Vec::new(),
    };
    Ok(if platform::runs(&p, wanted) {
        Here::Runs {
            platform: named(&p),
            size: layers(&manifest),
        }
    } else {
        Here::Elsewhere(vec![named(&p)])
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::testing::{After, Seen, route};

    fn client() -> Client {
        Client::new(
            Box::new(|_| crate::tls::client_config(Vec::new(), None)),
            "shards-test",
        )
    }

    fn json(status: &str, fields: &[(&str, &str)], body: &str) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\n");
        for (n, v) in fields {
            out.push_str(&format!("{n}: {v}\r\n"));
        }
        out.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        out.into_bytes()
    }

    fn target(arch: &str) -> Target {
        Target {
            os: "linux".into(),
            architecture: arch.into(),
            variant: String::new(),
        }
    }

    #[test]
    fn terms_name_the_registry_they_are_searched_in() {
        assert_eq!(searched("python"), (Searched::Hub, "python"));
        assert_eq!(searched("python3.12"), (Searched::Hub, "python3.12"));
        assert_eq!(searched("library/python"), (Searched::Hub, "python"));
        assert_eq!(searched("bitnami/python"), (Searched::Hub, "bitnami/python"));
        assert_eq!(searched("team/app.v2"), (Searched::Hub, "team/app.v2"));
        assert_eq!(searched("docker.io/library/python"), (Searched::Hub, "python"));
        assert_eq!(searched("index.docker.io/team/app"), (Searched::Hub, "team/app"));
        assert_eq!(searched("quay.io/prometheus"), (Searched::Quay, "prometheus"));
        assert_eq!(
            searched("localhost/app"),
            (Searched::Catalog("localhost".into()), "app")
        );
        assert_eq!(
            searched("127.0.0.1:5000/team"),
            (Searched::Catalog("127.0.0.1:5000".into()), "team")
        );
        assert_eq!(
            searched("registry.example/"),
            (Searched::Catalog("registry.example".into()), "")
        );
    }

    /// Docker Hub's index is asked as dockerd asks it: the term and limit escaped as Go
    /// escapes them, `X-Docker-Token`, a password login as Basic credentials, an identity
    /// token as the bearer token its token server gives for `registry:catalog:search`.
    #[test]
    fn docker_hubs_index_is_searched_as_dockerd_searches_it() {
        let tokens = route(None, |_: &Seen| {
            Some((
                json("200 OK", &[], r#"{"token":"t","access_token":"t"}"#),
                After::Keep,
            ))
        });
        let realm = format!(
            r#"Bearer realm="http://127.0.0.1:{}/token",service="reg",scope="registry:catalog:search""#,
            tokens.port
        );
        let index = route(None, move |seen: &Seen| {
            let auth = seen.header("authorization").unwrap_or_default().to_string();
            Some((
                match seen.target.as_str() {
                    "/v2/" if auth == "Bearer t" => json("200 OK", &[], "{}"),
                    "/v2/" => json("401 Unauthorized", &[("WWW-Authenticate", &realm)], "{}"),
                    t if t.starts_with("/v1/search?q=down") => json(
                        "503 Service Unavailable",
                        &[],
                        r#"{"message":"down for a moment"}"#,
                    ),
                    t => {
                        let token = seen.header("x-docker-token").unwrap_or_default();
                        json(
                            "200 OK",
                            &[],
                            &format!(
                                r#"{{"num_results":2,"results":[{{"name":"alpine","description":"{auth}|{token}|{t}","star_count":7,"is_official":true,"is_automated":false,"pull_count":12}},{{"name":"a/b","description":null}}]}}"#
                            ),
                        )
                    }
                },
                After::Keep,
            ))
        });
        let at = Url::parse(&format!("http://127.0.0.1:{}/v1/", index.port)).unwrap();
        let found = hub(client(), &at, "py thon/x*", 3, &Credentials::Anonymous).unwrap();
        assert_eq!(
            found,
            [
                Found {
                    name: "alpine".into(),
                    description: "|true|/v1/search?q=py+thon%2Fx%2A&n=3".into(),
                    stars: 7,
                    official: true,
                    pulls: Some(12),
                },
                Found {
                    name: "a/b".into(),
                    ..Found::default()
                },
            ]
        );
        let password = Credentials::Password {
            username: "u".into(),
            password: "p".into(),
        };
        let found = hub(client(), &at, "x", 1, &password).unwrap();
        assert!(
            found[0]
                .description
                .starts_with(&format!("{}|true|", basic(b"u", b"p"))),
            "{found:?}"
        );
        let found = hub(client(), &at, "x", 1, &Credentials::IdentityToken("r".into())).unwrap();
        assert!(found[0].description.starts_with("Bearer t|true|"), "{found:?}");
        assert!(
            tokens
                .requests()
                .iter()
                .any(|r| r.contains("scope=registry%3Acatalog%3Asearch") && r.contains("refresh_token=r")),
            "{:?}",
            tokens.requests()
        );
        // Refused: moby's words, and the index's.
        let e = hub(client(), &at, "down", 1, &Credentials::Anonymous).unwrap_err();
        assert_eq!(e.to_string(), "unexpected status code 503: down for a moment");
    }

    /// quay.io's search, page by page while it has more and a page brings any; each
    /// result named with the host, described by its README's first paragraph.
    #[test]
    fn quays_search_is_read_page_by_page() {
        let server = route(None, |seen: &Seen| {
            let page = seen.target.rsplit("page=").next().unwrap_or_default().to_string();
            let hit = |ns: &str, name: &str, description: &str| {
                format!(
                    r#"{{"kind":"repository","name":"{name}","namespace":{{"name":"{ns}"}},"description":{description},"stars":null}}"#
                )
            };
            let body = match page.as_str() {
                "1" => format!(
                    r#"{{"results":[{},{},{{"kind":"user","name":"someone"}}],"has_additional":true,"page":1}}"#,
                    hit(
                        "prometheus",
                        "prometheus",
                        r#""<h1 align=\"center\">\n  <img alt=\"P\" src=\"x.svg\"><br>Prometheus\n</h1>\n\nVisit it.""#
                    ),
                    hit(
                        "team",
                        "app",
                        r##""# The [app](https://a) [![CI](https://b/badge.svg)](https://c) **runs**\nwell `here`\n\nMore.""##
                    ),
                ),
                "2" => format!(
                    r#"{{"results":[{}],"has_additional":true,"page":2}}"#,
                    hit("team", "web", "null")
                ),
                _ => r#"{"results":[],"has_additional":true}"#.to_string(),
            };
            Some((json("200 OK", &[], &body), After::Keep))
        });
        let api = Url::parse(&format!("http://127.0.0.1:{}/api/v1/", server.port)).unwrap();
        let found = quay(client(), &api, "quay.io", "pro m", 10).unwrap();
        let names: Vec<(&str, &str)> = found
            .iter()
            .map(|f| (f.name.as_str(), f.description.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                ("quay.io/prometheus/prometheus", "Prometheus"),
                ("quay.io/team/app", "The app runs well here"),
                ("quay.io/team/web", ""),
            ]
        );
        let asked = server.requests();
        assert!(
            asked[0].starts_with("GET /api/v1/find/repositories?query=pro+m&page=1 "),
            "{asked:?}"
        );
        assert_eq!(asked.len(), 3, "{asked:?}");
        // As many as the limit, from the pages it takes.
        let before = server.requests().len();
        assert_eq!(quay(client(), &api, "quay.io", "p", 2).unwrap().len(), 2);
        assert_eq!(server.requests().len(), before + 1);
    }

    /// A registry's catalog, its pages followed by their `Link`, its names that hold the
    /// term ignoring case, as many as the limit; its credentials sent where it asks for
    /// them; and a registry that serves none says so.
    #[test]
    fn a_registrys_catalog_is_searched_for_names_that_hold_the_term() {
        let good = basic(b"u", b"p");
        let server = route(None, move |seen: &Seen| {
            let authorized = seen.header("authorization") == Some(good.as_str());
            Some((
                match seen.target.as_str() {
                    _ if !authorized => json(
                        "401 Unauthorized",
                        &[("WWW-Authenticate", r#"Basic realm="r""#)],
                        r#"{"errors":[{"code":"UNAUTHORIZED","message":"authentication required"}]}"#,
                    ),
                    "/v2/_catalog" => json(
                        "200 OK",
                        &[("Link", r#"</v2/_catalog?last=team%2Fweb&n=3>; rel="next""#)],
                        r#"{"repositories":["Team/app","team/web","other/x"]}"#,
                    ),
                    _ => json("200 OK", &[], r#"{"repositories":["teams/z"]}"#),
                },
                After::Keep,
            ))
        });
        let host = format!("127.0.0.1:{}", server.port);
        let password = || Credentials::Password {
            username: "u".into(),
            password: "p".into(),
        };
        let found = catalog(client(), &host, "TEAM", 10, password()).unwrap();
        let names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            [
                format!("{host}/Team/app"),
                format!("{host}/team/web"),
                format!("{host}/teams/z")
            ]
        );
        assert_eq!(catalog(client(), &host, "team", 1, password()).unwrap().len(), 1);
        let e = catalog(client(), &host, "team", 1, Credentials::Anonymous).unwrap_err();
        assert_eq!(
            e.to_string(),
            format!("{host}: authorization failed: no basic auth credentials")
        );
        // One that serves none.
        let none = route(None, |_: &Seen| {
            Some((
                json(
                    "404 Not Found",
                    &[],
                    r#"{"errors":[{"code":"NOT_FOUND","message":"page not found"}]}"#,
                ),
                After::Keep,
            ))
        });
        let host = format!("127.0.0.1:{}", none.port);
        let e = catalog(client(), &host, "team", 1, Credentials::Anonymous).unwrap_err();
        assert_eq!(
            e.to_string(),
            format!("{host} serves no catalog to search: 404 Not Found: page not found")
        );
    }

    /// Docker Hub's tag says what runs here, as a pull chooses it, attestations aside;
    /// what has no build for this machine names what it has; a missing tag is said.
    #[test]
    fn docker_hubs_tags_say_what_runs_here() {
        let server = route(None, |seen: &Seen| {
            Some((
                match seen.target.as_str() {
                    "/v2/namespaces/library/repositories/alpine/tags/latest" => json(
                        "200 OK",
                        &[],
                        r#"{"name":"latest","images":[{"os":"linux","architecture":"amd64","size":10,"digest":"sha256:a"},{"os":"linux","architecture":"arm64","variant":"v8","size":20,"digest":"sha256:b"},{"os":"unknown","architecture":"unknown","size":1},{"os":"linux","architecture":"386","size":30}]}"#,
                    ),
                    _ => json("404 Not Found", &[], r#"{"message":"tag not found"}"#),
                },
                After::Keep,
            ))
        });
        let api = Url::parse(&format!("http://127.0.0.1:{}/v2/", server.port)).unwrap();
        assert_eq!(
            hub_here(client(), &api, "alpine", "latest", &[target("arm64")]).unwrap(),
            Here::Runs {
                platform: "linux/arm64/v8".into(),
                size: 20
            }
        );
        assert_eq!(
            hub_here(
                client(),
                &api,
                "alpine",
                "latest",
                &[target("amd64"), target("386")]
            )
            .unwrap(),
            Here::Runs {
                platform: "linux/amd64".into(),
                size: 10
            }
        );
        assert_eq!(
            hub_here(client(), &api, "alpine", "latest", &[target("riscv64")]).unwrap(),
            Here::Elsewhere(vec![
                "linux/386".into(),
                "linux/amd64".into(),
                "linux/arm64/v8".into()
            ])
        );
        assert_eq!(
            hub_here(client(), &api, "bitnami/python", "latest", &[target("arm64")]).unwrap(),
            Here::Untagged
        );
        assert!(
            server
                .requests()
                .iter()
                .any(|r| r.starts_with("GET /v2/namespaces/bitnami/repositories/python/tags/latest ")),
            "{:?}",
            server.requests()
        );
    }

    /// Any other registry's tag: its index as a pull chooses from it, the chosen
    /// manifest's layers its size; a lone manifest by its config's platform; a missing
    /// tag said.
    #[test]
    fn a_tags_manifests_say_what_runs_here() {
        use sha2::{Digest as _, Sha256};
        let config = br#"{"architecture":"arm64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#;
        let config_digest = format!(
            "sha256:{}",
            Sha256::digest(config)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let lone = format!(
            r#"{{"schemaVersion":2,"mediaType":"{}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config_digest}","size":{}}},"layers":[{{"mediaType":"x","digest":"sha256:l3","size":3}}]}}"#,
            media::OCI_MANIFEST,
            config.len()
        );
        let index = format!(
            r#"{{"schemaVersion":2,"mediaType":"{}","manifests":[{{"mediaType":"{m}","digest":"sha256:aa","size":1,"platform":{{"os":"linux","architecture":"amd64"}}}},{{"mediaType":"{m}","digest":"sha256:bb","size":1,"platform":{{"os":"linux","architecture":"arm64","variant":"v8"}}}},{{"mediaType":"{m}","digest":"sha256:cc","size":1,"platform":{{"os":"unknown","architecture":"unknown"}}}}]}}"#,
            media::OCI_INDEX,
            m = media::OCI_MANIFEST
        );
        let chosen = format!(
            r#"{{"schemaVersion":2,"mediaType":"{}","config":{{"mediaType":"x","digest":"sha256:c","size":1}},"layers":[{{"mediaType":"x","digest":"sha256:l1","size":5}},{{"mediaType":"x","digest":"sha256:l2","size":7}}]}}"#,
            media::OCI_MANIFEST
        );
        let digest = config_digest.clone();
        let server = route(None, move |seen: &Seen| {
            let kind = |k: &str| [("Content-Type", k.to_string())];
            Some((
                match seen.target.as_str() {
                    "/v2/team/multi/manifests/latest" => {
                        let k = kind(media::OCI_INDEX);
                        json("200 OK", &[(k[0].0, &k[0].1)], &index)
                    }
                    "/v2/team/multi/manifests/sha256:bb" | "/v2/team/multi/manifests/sha256:aa" => {
                        let k = kind(media::OCI_MANIFEST);
                        json("200 OK", &[(k[0].0, &k[0].1)], &chosen)
                    }
                    "/v2/team/lone/manifests/latest" => {
                        let k = kind(media::OCI_MANIFEST);
                        json("200 OK", &[(k[0].0, &k[0].1)], &lone)
                    }
                    t if t == format!("/v2/team/lone/blobs/{digest}") => {
                        json("200 OK", &[], std::str::from_utf8(config).unwrap())
                    }
                    _ => json(
                        "404 Not Found",
                        &[],
                        r#"{"errors":[{"code":"MANIFEST_UNKNOWN"}]}"#,
                    ),
                },
                After::Keep,
            ))
        });
        let at = |name: &str| Reference::parse(&format!("127.0.0.1:{}/team/{name}", server.port)).unwrap();
        assert_eq!(
            here(client(), &at("multi"), Credentials::Anonymous, &[target("arm64")]).unwrap(),
            Here::Runs {
                platform: "linux/arm64/v8".into(),
                size: 12
            }
        );
        assert_eq!(
            here(
                client(),
                &at("multi"),
                Credentials::Anonymous,
                &[target("riscv64")]
            )
            .unwrap(),
            Here::Elsewhere(vec!["linux/amd64".into(), "linux/arm64/v8".into()])
        );
        assert_eq!(
            here(client(), &at("lone"), Credentials::Anonymous, &[target("arm64")]).unwrap(),
            Here::Runs {
                platform: "linux/arm64".into(),
                size: 3
            }
        );
        assert_eq!(
            here(client(), &at("lone"), Credentials::Anonymous, &[target("amd64")]).unwrap(),
            Here::Elsewhere(vec!["linux/arm64".into()])
        );
        assert_eq!(
            here(client(), &at("none"), Credentials::Anonymous, &[target("arm64")]).unwrap(),
            Here::Untagged
        );
    }
}
