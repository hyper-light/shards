//! `shards search image` (D172), and `shards search` as Docker says it: images found in
//! registries, each asked through what it documents (shards_registry::search), Docker
//! Hub's unless the term names another; and what docker's search leaves unsaid, whether
//! each runs on this machine and what its pull takes. A microVM boots this machine's
//! architecture, so an image without a build for it does not run here, and its size is
//! what its first pull downloads.
//!
//! A search is the CLI's own, as a login is: no daemon is started for it. `GET
//! /images/search` runs the same search in the daemon ([`find`]).

use std::ffi::OsString;
use std::io::Write as _;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};

use shards_cmdline::commands::SEARCH;
use shards_cmdline::flags;
use shards_cmdline::format::search::{self as table, Here as Said};
use shards_cmdline::format::{Clock, Context};
use shards_image::reference::Reference;
use shards_registry::search::{self, Found, Here, Searched};
use shards_registry::url::Url;

use crate::filters::Filters;

/// What dockerd searches for when given no limit (defaultSearchLimit).
const DEFAULT_LIMIT: i64 = 25;
/// The filters a search takes (acceptedSearchFilterTags).
const ACCEPTED: &[&str] = &["is-automated", "is-official", "stars"];
/// quay.io's API.
const QUAY_API: &str = "https://quay.io/api/v1/";
/// Lookups at once: the most measured with none throttled by Docker Hub (PM M234).
const LOOKUPS: usize = 4;

/// What a search finds its credentials, certificates and proxies in: shared by its
/// lookups' threads.
pub(crate) type Env<'a> = &'a (dyn Fn(&str) -> Option<String> + Sync);

/// A search refused: its words, and whether for what it was asked (the Engine API's
/// 400) or by its registry (500).
#[derive(Debug)]
pub(crate) struct Refusal {
    // Read by the Engine API's search, which is Unix's alone as yet.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub asked: bool,
    pub text: String,
}

/// The host whose credentials and certificates a search of `searched` uses.
pub(crate) fn host(searched: &Searched) -> &str {
    match searched {
        Searched::Hub => "docker.io",
        Searched::Quay => "quay.io",
        Searched::Catalog(host) => host,
    }
}

/// The registry a search of `term` asks, as people name it: what its page says.
#[cfg(unix)]
fn registry_named(term: &str) -> String {
    match search::searched(term).0 {
        Searched::Hub => "Docker Hub".into(),
        Searched::Quay => "quay.io".into(),
        Searched::Catalog(host) => host,
    }
}

/// What `term` finds, as many as `limit` (dockerd's 25 for 0), filtered as dockerd
/// filters a search (daemon/pkg/registry/search.go, Search): `is-official` and `stars`
/// kept, `is-automated=true` finding nothing, any other filter refused.
pub(crate) fn find(term: &str, limit: i64, filters: &Filters, env: Env<'_>) -> Result<Vec<Found>, Refusal> {
    let asked = |text: String| Refusal { asked: true, text };
    filters.validate(ACCEPTED).map_err(asked)?;
    if filters.bool_or("is-automated", false).map_err(asked)? {
        return Ok(Vec::new());
    }
    let official = filters.bool_or("is-official", false).map_err(asked)?;
    let mut stars = 0i64;
    for given in filters.get("stars") {
        let n: i64 = given.parse().map_err(|_| {
            asked(format!(
                "invalid filter 'stars={given}': strconv.Atoi: parsing {}: invalid syntax",
                shards_cmdline::go::quote(given)
            ))
        })?;
        stars = stars.max(n);
    }
    if term.contains("://") {
        return Err(asked(format!(
            "invalid repository name: repository name ({term}) should not have a scheme"
        )));
    }
    let limit = if limit == 0 { DEFAULT_LIMIT } else { limit };
    let Some(limit) = usize::try_from(limit).ok().filter(|l| (1..=100).contains(l)) else {
        return Err(asked(format!("limit {limit} is outside the range of [1, 100]")));
    };
    let (searched, rest) = search::searched(term);
    let at = host(&searched);
    let failed = |e: shards_registry::Error| Refusal {
        asked: false,
        text: e.to_string(),
    };
    let http = crate::pull::host_client(at, env).map_err(failed)?;
    // As a pull finds them; none where none are found, as docker searches without them.
    let credentials = shards_registry::credentials::lookup(at, env)
        .map(|(c, _)| c)
        .unwrap_or_default();
    let url = |u: &str| Url::parse(u).map_err(failed);
    let found = match &searched {
        Searched::Hub => search::hub(http, &url(search::HUB_INDEX)?, rest, limit, &credentials),
        Searched::Quay => search::quay(http, &url(QUAY_API)?, "quay.io", rest, limit),
        Searched::Catalog(host) => search::catalog(http, host, rest, limit, credentials),
    }
    .map_err(failed)?;
    Ok(found
        .into_iter()
        .filter(|f| !filters.contains("is-official") || f.official == official)
        .filter(|f| !filters.contains("stars") || f.stars >= stars)
        .collect())
}

/// Whether `found` runs here, its `latest` as a run that names no tag pulls it: Docker
/// Hub's by its API, any other's by its tag's manifests.
pub(crate) fn here(found: &Found, env: Env<'_>) -> Result<Here, String> {
    let (searched, _) = search::searched(&found.name);
    let at = host(&searched);
    let http = crate::pull::host_client(at, env).map_err(|e| e.to_string())?;
    let wanted = shards_image::platform::guest();
    match searched {
        Searched::Hub => {
            let api = Url::parse(search::HUB_API).map_err(|e| e.to_string())?;
            search::hub_here(http, &api, &found.name, "latest", &wanted)
        }
        Searched::Quay | Searched::Catalog(_) => {
            let reference = Reference::parse(&found.name).map_err(|e| e.to_string())?;
            let credentials = shards_registry::credentials::lookup(at, env)
                .map(|(c, _)| c)
                .unwrap_or_default();
            search::here(http, &reference, credentials, &wanted)
        }
    }
    .map_err(|e| e.to_string())
}

/// Looks each of `found` up ([`here`]), [`LOOKUPS`] at once, each told to `each` as it
/// answers: a slow one holds up only its own.
fn look_up(found: &[Found], env: Env<'_>, mut each: impl FnMut(usize, Result<Here, String>)) {
    let next = AtomicUsize::new(0);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let mut spawned = 0usize;
        for _ in 0..LOOKUPS.min(found.len()) {
            let tx = tx.clone();
            let next = &next;
            let started = std::thread::Builder::new()
                .name("shards-search".into())
                .spawn_scoped(scope, move || {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(f) = found.get(i) else {
                            return;
                        };
                        if tx.send((i, here(f, env))).is_err() {
                            return;
                        }
                    }
                });
            if started.is_ok() {
                spawned += 1;
            }
        }
        drop(tx);
        if spawned == 0 {
            // No thread could start: each looked up here, in turn.
            for (i, f) in found.iter().enumerate() {
                each(i, here(f, env));
            }
            return;
        }
        for (i, answered) in rx {
            each(i, answered);
        }
    });
}

/// What the format module says of a lookup's answer.
fn said(answered: Result<Here, String>) -> Said {
    match answered {
        Ok(Here::Runs { platform, size }) => Said::Runs { platform, size },
        Ok(Here::Elsewhere(platforms)) => Said::Elsewhere(platforms),
        Ok(Here::Untagged) => Said::Untagged,
        Err(why) => Said::Unknown(why),
    }
}

/// Whether a template names one of shards' own fields, which a lookup fills.
fn names_lookups(format: &str) -> bool {
    ["RunsHere", "Platform", "Size"]
        .iter()
        .any(|m| format.contains(&format!(".{m}")))
}

/// `text` on stderr, and status 1, as the Docker CLI fails a command; in a panel under
/// search's head on a colour terminal.
fn failed(text: &str) -> ExitCode {
    #[cfg(unix)]
    if let Some(p) = crate::cli::look::styled_err() {
        let first = text.lines().next().unwrap_or(text);
        crate::cli::look::error(&p, "search", first, &[]);
        return ExitCode::from(1);
    }
    let _ = writeln!(std::io::stderr(), "{text}");
    ExitCode::from(1)
}

/// `shards search [OPTIONS] TERM`: Docker's table, its output piped; on a colour
/// terminal, shards' page, each result's lookup filled in as it answers.
pub fn search(args: impl Iterator<Item = OsString>) -> ExitCode {
    let mut argv = Vec::new();
    for a in args {
        match a.into_string() {
            Ok(a) => argv.push(a),
            Err(a) => return failed(&format!("{a:?} is not UTF-8")),
        }
    }
    let parsed = match crate::cli::read(&SEARCH, "shards search", &argv, &flags::value) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let _ = write!(std::io::stdout(), "{}", parsed.notices);
    let given = parsed.many("filter");
    if given
        .iter()
        .any(|f| f.split_once('=').map_or(f.as_str(), |(k, _)| k) == "is-automated")
    {
        let _ = writeln!(
            std::io::stderr(),
            "WARNING: the \"is-automated\" filter is deprecated, and searching for \"is-automated=true\" will not yield any results in future."
        );
    }
    let filters = Filters::from_flags(given);
    let term = parsed.args.first().map(String::as_str).unwrap_or_default();
    let env = |k: &str| std::env::var(k).ok();
    let found = match find(term, parsed.int("limit"), &filters, &env) {
        Ok(found) => found,
        Err(refusal) => return failed(&format!("shards: {}", refusal.text)),
    };
    let format = parsed.string("format");
    #[cfg(unix)]
    if format.is_empty()
        && let Some(_paint) = crate::cli::look::styled()
    {
        return live(term, &found, &env);
    }
    let mut rows: Vec<table::Found> = found
        .iter()
        .map(|f| table::Found {
            name: f.name.clone(),
            description: f.description.clone(),
            star_count: f.stars,
            is_official: f.official,
            here: None,
            pulls: f.pulls,
        })
        .collect();
    if names_lookups(format) {
        look_up(&found, &env, |i, answered| {
            if let Some(row) = rows.get_mut(i) {
                row.here = Some(said(answered));
            }
        });
    }
    let zone = |_: i64| shards_cmdline::format::Zone {
        offset: 0,
        name: "UTC".into(),
    };
    let clock = Clock { now: 0, zone: &zone };
    let format = table::format(format);
    let ctx = Context {
        format: &format,
        trunc: !parsed.bool("no-trunc"),
        east_asian: shards_cmdline::width::east_asian(|name| std::env::var(name).ok()),
        clock: &clock,
    };
    let mut out = String::new();
    let written = table::write(&ctx, &rows, &mut out);
    let _ = std::io::stdout().write_all(out.as_bytes());
    match written {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => failed(&e),
    }
}

/// shards' page of `found` on a colour terminal, drawn at once and again as each result's
/// lookup answers.
#[cfg(unix)]
fn live(term: &str, found: &[Found], env: Env<'_>) -> ExitCode {
    let mut answers: Vec<Option<Result<Here, String>>> = vec![None; found.len()];
    let mut frame = shards_tui::frame::Frame::new();
    let sheet = |answers: &[Option<Result<Here, String>>]| {
        let mut sheet = shards_ipc::Sheet::new("search");
        let pending = answers.iter().filter(|a| a.is_none()).count();
        sheet.record(&[
            ("term", term.to_string()),
            ("registry", registry_named(term)),
            ("pending", pending.to_string()),
        ]);
        for (f, answer) in found.iter().zip(answers) {
            let (here, platform, size, why) = match answer {
                None => ("looking", String::new(), 0, String::new()),
                Some(Ok(Here::Runs { platform, size })) => ("runs", platform.clone(), *size, String::new()),
                Some(Ok(Here::Elsewhere(platforms))) => ("elsewhere", platforms.join(", "), 0, String::new()),
                Some(Ok(Here::Untagged)) => ("untagged", String::new(), 0, String::new()),
                Some(Err(why)) => ("unknown", String::new(), 0, why.clone()),
            };
            sheet.record(&[
                ("name", f.name.clone()),
                ("description", f.description.clone()),
                ("stars", f.stars.to_string()),
                ("official", f.official.to_string()),
                ("here", here.to_string()),
                ("platform", platform),
                ("size", size.to_string()),
                ("why", why),
            ]);
        }
        sheet
    };
    crate::cli::show_live(&sheet(&answers), &mut frame);
    look_up(found, env, |i, answered| {
        if let Some(slot) = answers.get_mut(i) {
            *slot = Some(answered);
        }
        crate::cli::show_live(&sheet(&answers), &mut frame);
    });
    ExitCode::SUCCESS
}
