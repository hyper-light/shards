//! `shards pull` as docker/cli asks for it (cli/command/image/pull.go, runPull) and
//! dockerd's containerd store pulls (moby daemon/containerd/image_pull.go, PullImage):
//! the reference read, `-a` checked against a tag, the default tag said, the platform
//! read; then each tag of the repository, or the one named, said as `docker pull` says
//! it; then the name. An image our microVMs run is made ready to boot as it is pulled:
//! its root filesystem built. Beyond Docker, `--output-agentfile` writes the image's
//! Agentfile (docs/architecture/AGENTFILE_ARCH.md §10).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use shards_image::platform::{self as image_platform, Target};
use shards_image::reference::Reference;
use shards_ipc::Progress;
use shards_registry::pull::Event;

/// How often a client on a terminal hears how much of a layer has arrived: often enough
/// for motion at its frame rate, rarely enough that a 10 GB pull sends a few messages a
/// layer a second, not one for each read.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// `shards pull NAME[:TAG|@DIGEST]`, `-a` for every tag of the repository.
    pub(super) fn pull(
        &self,
        parsed: &shards_cmdline::flags::Parsed,
        asker: &super::commands::Asker,
        reply: &super::commands::Reply<'_>,
    ) -> u8 {
        let refuse = |said: &str| {
            reply.err(said);
            1
        };
        let given = parsed.args.first().map(String::as_str).unwrap_or_default();
        let (all, quiet) = (parsed.bool("all-tags"), parsed.bool("quiet"));
        // On a colour terminal the client shows the pull its own way, from its steps;
        // anywhere else it prints `docker pull`'s lines.
        let show = asker.terminal && asker.color && !quiet;
        let say = |line: &str| {
            if !quiet && !show {
                reply.out(line);
            }
        };
        // What the CLI checks before it asks dockerd, in its words.
        let mut named = match Reference::parse_normalized(given) {
            Ok(r) => r,
            Err(e) => return refuse(&e.to_string()),
        };
        let name_only = named.tag.is_none() && named.digest.is_none();
        if all && !name_only {
            return refuse("tag can't be used with --all-tags/-a");
        }
        if !all && name_only {
            named.tag = Some("latest".into());
            say("Using default tag: latest");
        }
        let targets = match targets(parsed.string("platform")) {
            Ok(t) => t,
            Err(e) => return refuse(&e),
        };
        let env = |k: &str| shards_ipc::env_value(&asker.registry_env, k);
        let (status, _) = self.cancellable(asker.client, reply.0, |cancel| {
            let wanted = if all {
                let tags = crate::pull::registry(&named, Some(cancel), &env)
                    .and_then(|r| r.tags().map_err(|e| e.to_string()));
                match tags {
                    Ok(tags) => tags
                        .into_iter()
                        .map(|t| Reference {
                            tag: Some(t),
                            ..named.clone()
                        })
                        .collect(),
                    Err(e) => return refuse(&format!("Error response from daemon: {e}")),
                }
            } else {
                vec![named.clone()]
            };
            for reference in &wanted {
                let downloaded = AtomicBool::new(false);
                // Each layer's bytes so far, and when the client last heard of them.
                let arrived: Mutex<HashMap<String, (u64, Option<Instant>)>> = Mutex::new(HashMap::new());
                let report = |event: Event<'_>| {
                    if let Event::Layer(_) = event {
                        downloaded.store(true, Ordering::Relaxed);
                    }
                    if show {
                        if let Some(p) = shown(&event, &arrived) {
                            reply.progress(&p);
                        }
                        return;
                    }
                    match event {
                        Event::Present(d) => {
                            say(&format!("{}: Already exists", crate::pull::short(&d.to_string())))
                        }
                        Event::Layer(d) => say(&format!(
                            "{}: Download complete",
                            crate::pull::short(&d.to_string())
                        )),
                        Event::Manifest(..) | Event::Progress(..) | Event::Building | Event::Pulling => {}
                    }
                };
                let pulling = |line: &str| {
                    if show {
                        reply.progress(&Progress::Pulling {
                            reference: reference.to_string(),
                            repository: reference.path.clone(),
                        });
                    } else {
                        say(line);
                    }
                };
                let pulled = crate::pull::fetch(
                    &self.home,
                    reference,
                    &targets,
                    &report,
                    &pulling,
                    Some(cancel),
                    &env,
                );
                let (pulled, same) = match pulled {
                    Ok(p) => p,
                    Err(e) if all => {
                        return refuse(&format!(
                            "Error response from daemon: error pulling {reference}: {e}"
                        ));
                    }
                    Err(e) => return refuse(&format!("Error response from daemon: {e}")),
                };
                // Docker's: up to date when the tag already named this image, or when a
                // digest's content was all here.
                let up_to_date = same || (reference.digest.is_some() && !downloaded.load(Ordering::Relaxed));
                if show {
                    reply.progress(&Progress::Done {
                        digest: pulled.resolved.to_string(),
                        unchanged: up_to_date,
                        bootable: pulled.rootfs.is_some(),
                    });
                }
                say(&format!("Digest: {}", pulled.resolved));
                let status = if up_to_date {
                    "Image is up to date for"
                } else {
                    "Downloaded newer image for"
                };
                say(&format!("Status: {status} {}", reference.familiar()));
            }
            0
        });
        if status == 0 {
            // The CLI's last line, said even when quiet: the name pulled, a tag's or the
            // repository's.
            reply.out(&named.to_string());
        }
        status
    }
}

/// A pull's event as a client on a terminal hears it, if it hears it now: a layer's
/// bytes at most every [`PROGRESS_EVERY`], summed in `arrived`.
fn shown(event: &Event<'_>, arrived: &Mutex<HashMap<String, (u64, Option<Instant>)>>) -> Option<Progress> {
    Some(match event {
        Event::Present(d) => Progress::Have(d.to_string()),
        Event::Layer(d) => Progress::Verified(d.to_string()),
        Event::Manifest(_, layers) => Progress::Layers(
            layers
                .iter()
                .map(|l| (l.digest.clone(), u64::try_from(l.size).unwrap_or(0)))
                .collect(),
        ),
        Event::Building => Progress::Building,
        Event::Pulling => return None,
        Event::Progress(d, n) => {
            let key = d.to_string();
            let mut held = arrived.lock().unwrap_or_else(PoisonError::into_inner);
            let entry = held.entry(key.clone()).or_insert((0, None));
            entry.0 = entry.0.saturating_add(*n);
            let now = Instant::now();
            if entry
                .1
                .is_some_and(|last| now.duration_since(last) < PROGRESS_EVERY)
            {
                return None;
            }
            entry.1 = Some(now);
            Progress::Bytes(key, entry.0)
        }
    })
}

/// The platforms to pull for: `--platform`'s, read as containerd's `platforms.Parse`
/// reads it (v1.0.0-rc.5, which docker/cli v29.8.1 vendors), or else our guests'. A
/// specifier of an OS or an architecture alone takes the rest from our guests' platform,
/// where the CLI takes it from the machine it runs on: a client on macOS would otherwise
/// ask for `darwin` images, which no Linux daemon has.
fn targets(given: &str) -> Result<Vec<Target>, String> {
    if given.is_empty() {
        return Ok(image_platform::guest());
    }
    let guest = image_platform::guest();
    let ours = guest.first().ok_or("no platform for this host's guests")?;
    let build = shards_dockerfile::platform::Platform::new(&ours.os, &ours.architecture);
    let parsed = shards_dockerfile::platform::parse(given.as_bytes(), &build)
        .map_err(|e| String::from_utf8_lossy(&e).into_owned())?;
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    Ok(vec![image_platform::normalize(&shards_image::oci::Platform {
        os: text(&parsed.os),
        architecture: text(&parsed.architecture),
        variant: Some(text(&parsed.variant)).filter(|v| !v.is_empty()),
        ..shards_image::oci::Platform::default()
    })])
}
