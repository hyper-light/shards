//! `shards rmi` as dockerd's containerd store removes images (moby docker-v29.3.1
//! daemon/containerd/image_delete.go, ImageDelete; image.go, resolveAllReferences;
//! soft_delete.go), and as docker/cli reports it (cli/command/image/remove.go).
//!
//! A record is a reference the store holds, its target what the reference resolved to:
//! the image's ID. Removal takes records away; the content no record names goes with the
//! store's next collection, which removal makes due. An image a container still uses
//! keeps a dangling record, `moby-dangling@<ID>`, as dockerd keeps one.
//!
//! Unlike dockerd: no image here has a parent or children (only the classic builder made
//! them), so `--no-prune` changes nothing.

use shards_image::reference::{AnyReference, Digest, Reference};

use crate::containers::State as Life;

/// containerd's name for an image a container keeps when its last name goes.
pub(super) const DANGLING: &str = "moby-dangling@";

/// A reference the store holds, and its image's ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Record {
    pub name: String,
    pub id: Digest,
}

impl Record {
    fn dangling(&self) -> bool {
        self.name == format!("{DANGLING}{}", self.id)
    }
}

/// A container, as image removal sees it: its ID, whether it runs, and its image's.
#[derive(Clone, Debug)]
pub(super) struct User {
    pub id: String,
    pub running: bool,
    pub image: Option<String>,
}

/// What removing one image did: each line `rmi` prints of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Removed {
    Untagged(String),
    Deleted(String),
}

/// Why an image could not go, and whether the CLI counts it as not found (which `-f`
/// forgives).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Refused {
    pub said: String,
    pub not_found: bool,
}

fn not_found(said: String) -> Refused {
    Refused {
        said,
        not_found: true,
    }
}

fn refused(said: String) -> Refused {
    Refused {
        said,
        not_found: false,
    }
}

/// What the store is asked to change.
pub(super) trait Records {
    fn untag(&mut self, name: &str) -> Result<(), String>;
    /// `name` comes to name what `existing` names.
    fn alias(&mut self, name: &str, existing: &str) -> Result<(), String>;
}

const RUNNING: u8 = 1;
const ACTIVE_REFERENCE: u8 = 2;
const STOPPED: u8 = 4;
const SOFT: u8 = ACTIVE_REFERENCE | STOPPED;

/// stringid.TruncateID.
fn short(id: &str) -> &str {
    let id = id.split_once(':').map_or(id, |(_, hex)| hex);
    id.get(..12).unwrap_or(id)
}

/// The familiar form of a record's name, as reference.FamiliarString writes it.
fn familiar(name: &str) -> String {
    match AnyReference::parse(name) {
        Ok(AnyReference::Named(r)) => r.familiar(),
        Ok(AnyReference::Digest(d)) => d.to_string(),
        Err(_) => name.to_string(),
    }
}

/// ErrImageDoesNotExist's words for `given`.
fn missing(given: &str) -> Refused {
    not_found(format!(
        "Error response from daemon: {}",
        super::images::not_found(given)
    ))
}

/// checkTruncatedID: `given`, without `sha256:`, if it is 4 to 64 lowercase hex digits.
fn truncated_id(given: &str) -> Option<&str> {
    let id = given.strip_prefix("sha256:").unwrap_or(given);
    ((4..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some(id)
}

/// TagNameOnly's string: `latest` for a name that names neither tag nor digest.
fn tagged(r: &Reference) -> String {
    let mut r = r.clone();
    if r.tag.is_none() && r.digest.is_none() {
        r.tag = Some("latest".into());
    }
    r.to_string()
}

/// resolveAllReferences: the record `given` names, if it names one, and every record of
/// its image.
fn resolve_all(records: &[Record], given: &str) -> Result<(Option<Record>, Vec<Record>), Refused> {
    let parsed =
        AnyReference::parse(given).map_err(|e| refused(format!("Error response from daemon: {e}")))?;
    let mut id: Option<Digest> = None;
    let mut found: Option<Record> = None;
    if let Some(prefix) = truncated_id(given) {
        match &parsed {
            AnyReference::Digest(d) => {
                found = records.iter().find(|r| r.name == d.to_string()).cloned();
                id = Some(found.as_ref().map_or_else(|| d.clone(), |r| r.id.clone()));
            }
            AnyReference::Named(named) => {
                let name = tagged(named);
                let matching: Vec<Record> = records
                    .iter()
                    .filter(|r| {
                        r.name == name
                            || (r.id.algorithm().name() == "sha256" && r.id.hex().starts_with(prefix))
                    })
                    .cloned()
                    .collect();
                if matching.is_empty() {
                    return Err(missing(given));
                }
                for r in &matching {
                    if r.name == name {
                        found = Some(r.clone());
                    }
                    match &id {
                        Some(d) if *d != r.id => {
                            return Err(not_found(
                                "Error response from daemon: ambiguous reference".into(),
                            ));
                        }
                        Some(_) => {}
                        None => id = Some(r.id.clone()),
                    }
                }
                if found.is_none() || matching.len() > 1 {
                    return Ok((found, matching));
                }
            }
        }
    } else {
        let AnyReference::Named(named) = &parsed else {
            return Err(refused(
                "Error response from daemon: invalid name reference".into(),
            ));
        };
        id = named.digest.clone();
        match records.iter().find(|r| r.name == tagged(named)) {
            Some(r) => {
                found = Some(r.clone());
                id = Some(r.id.clone());
            }
            None if id.is_none() => return Err(missing(given)),
            None => {}
        }
    }
    let all: Vec<Record> = records
        .iter()
        .filter(|r| Some(&r.id) == id.as_ref())
        .cloned()
        .collect();
    if all.is_empty() {
        return Err(missing(given));
    }
    Ok((found, all))
}

/// isImageIDPrefix.
fn id_prefix(id: &Digest, given: &str) -> bool {
    let full = id.to_string();
    full.starts_with(given) || id.hex().starts_with(given)
}

/// getSameReferences: the records of `all` in `named`'s repository with its tag (every
/// tag, for a name with a digest; the first record's, with no name), and those with a
/// digest alone while every tag agrees; dangling ones always.
fn same_references(named: Option<&Reference>, all: &[Record]) -> Vec<Record> {
    let mut named = named.cloned();
    let mut tag: Option<String> = None;
    let mut all_tags = false;
    if let Some(n) = &named {
        if let Some(t) = &n.tag {
            tag = Some(t.clone());
        } else if n.digest.is_some() {
            all_tags = true;
        }
    }
    let mut same = Vec::new();
    let mut digest_refs: Option<Vec<Record>> = Some(Vec::new());
    for r in all {
        if !r.dangling()
            && let Ok(repo) = Reference::parse_normalized(&r.name)
        {
            match &named {
                None => {
                    tag = repo.tag.clone();
                    named = Some(repo);
                }
                Some(n) if n.name() != repo.name() => continue,
                Some(_) if !all_tags => match &repo.tag {
                    Some(t) => match &tag {
                        None => tag = Some(t.clone()),
                        Some(want) if want != t => {
                            digest_refs = None;
                            continue;
                        }
                        Some(_) => {}
                    },
                    None => {
                        if let Some(refs) = &mut digest_refs {
                            refs.push(r.clone());
                        }
                        continue;
                    }
                },
                Some(_) => {}
            }
        }
        same.push(r.clone());
    }
    same.extend(digest_refs.unwrap_or_default());
    same
}

/// imageDeleteConflict's words.
fn conflict(reference: &str, hard: bool, message: &str) -> Refused {
    let force = if hard {
        "cannot be forced"
    } else {
        "must be forced"
    };
    refused(format!(
        "Error response from daemon: conflict: unable to delete {reference} ({force}) - {message}"
    ))
}

/// checkImageDeleteConflict.
fn check_conflict(id: &Digest, all: &[Record], users: &[User], mask: u8) -> Result<(), Refused> {
    let image = id.to_string();
    let using = |running: bool| {
        users
            .iter()
            .find(|u| u.running == running && u.image.as_deref() == Some(image.as_str()))
    };
    if mask & RUNNING != 0
        && let Some(u) = using(true)
    {
        return Err(conflict(
            short(&image),
            true,
            &format!("image is being used by running container {}", short(&u.id)),
        ));
    }
    if mask & STOPPED != 0
        && let Some(u) = using(false)
    {
        return Err(conflict(
            short(&image),
            false,
            &format!("image is being used by stopped container {}", short(&u.id)),
        ));
    }
    if mask & ACTIVE_REFERENCE != 0 && all.len() > 1 {
        return Err(conflict(
            short(&image),
            false,
            "image is referenced in multiple repositories",
        ));
    }
    Ok(())
}

/// ImageDelete: removes what `given` names, as `rmi` (with `force`) asks.
pub(super) fn delete(
    store: &mut dyn Records,
    records: &[Record],
    users: &[User],
    given: &str,
    force: bool,
) -> Result<Vec<Removed>, Refused> {
    let mut mask = if force { 0 } else { SOFT };
    let (found, all) = resolve_all(records, given)?;
    let id = match &found {
        None => {
            let Some(first) = all.first() else {
                return Err(missing(given));
            };
            let id = first.id.clone();
            let named = if id_prefix(&id, given) {
                None
            } else {
                Reference::parse_normalized(given).ok()
            };
            let same = same_references(named.as_ref(), &all);
            if same.is_empty() && named.is_some() {
                return Err(missing(given));
            }
            if same.len() == all.len() && !force {
                mask &= !ACTIVE_REFERENCE;
            }
            if named.is_some() && !same.is_empty() && same.len() != all.len() {
                return untag(store, &same);
            }
            id
        }
        Some(record) => {
            let id = record.id.clone();
            let explicit_dangling = given.starts_with(DANGLING) && record.dangling();
            if id_prefix(&id, given) || explicit_dangling {
                return delete_all(store, &id, &all, users, mask);
            }
            let parsed = Reference::parse_normalized(&record.name)
                .map_err(|e| refused(format!("Error response from daemon: {e}")))?;
            let same = same_references(Some(&parsed), &all);
            if same.len() != all.len() {
                return untag(store, &same);
            } else if all.len() > 1 && !force {
                mask &= !ACTIVE_REFERENCE;
            }
            let image = id.to_string();
            if let Some(u) = users.iter().find(|u| u.image.as_deref() == Some(image.as_str())) {
                let name = parsed.familiar();
                if !force {
                    return Err(conflict(
                        &name,
                        false,
                        &format!(
                            "container {} is using its referenced image {}",
                            short(&u.id),
                            short(&image)
                        ),
                    ));
                }
                // softImageDelete: the image stays, dangling, for its container.
                if all.len() == 1 {
                    store
                        .alias(&format!("{DANGLING}{id}"), &record.name)
                        .map_err(|e| refused(format!("Error response from daemon: {e}")))?;
                }
                store
                    .untag(&record.name)
                    .map_err(|e| refused(format!("Error response from daemon: {e}")))?;
                return Ok(vec![Removed::Untagged(name)]);
            }
            id
        }
    };
    delete_all(store, &id, &all, users, mask)
}

/// untagReferences.
fn untag(store: &mut dyn Records, same: &[Record]) -> Result<Vec<Removed>, Refused> {
    let mut removed = Vec::with_capacity(same.len());
    for r in same {
        store
            .untag(&r.name)
            .map_err(|e| refused(format!("Error response from daemon: {e}")))?;
        if Reference::parse_normalized(&r.name).is_ok() {
            removed.push(Removed::Untagged(familiar(&r.name)));
        }
    }
    Ok(removed)
}

/// deleteAll: every record of image `id`, each checked first, then the image.
fn delete_all(
    store: &mut dyn Records,
    id: &Digest,
    all: &[Record],
    users: &[User],
    extra: u8,
) -> Result<Vec<Removed>, Refused> {
    let mut removed = Vec::with_capacity(all.len() + 1);
    for r in all {
        check_conflict(id, all, users, RUNNING | extra)?;
        store
            .untag(&r.name)
            .map_err(|e| refused(format!("Error response from daemon: {e}")))?;
        if !r.dangling() {
            removed.push(Removed::Untagged(familiar(&r.name)));
        }
    }
    removed.push(Removed::Deleted(id.to_string()));
    Ok(removed)
}

/// The containers image removal checks against.
pub(super) fn users<'a>(containers: impl Iterator<Item = &'a crate::containers::Container>) -> Vec<User> {
    containers
        .map(|c| User {
            id: c.id.clone(),
            running: c.state == Life::Running,
            image: c.image_id.clone(),
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The store as the tests see it: its records, changed as removal asks.
    struct Fake(Vec<Record>);

    impl Records for Fake {
        fn untag(&mut self, name: &str) -> Result<(), String> {
            self.0.retain(|r| r.name != name);
            Ok(())
        }
        fn alias(&mut self, name: &str, existing: &str) -> Result<(), String> {
            let id = self.0.iter().find(|r| r.name == existing).unwrap().id.clone();
            self.0.push(Record {
                name: name.into(),
                id,
            });
            Ok(())
        }
    }

    fn digest(c: char) -> Digest {
        Digest::parse(&format!("sha256:{}", c.to_string().repeat(64))).unwrap()
    }

    fn record(name: &str, c: char) -> Record {
        Record {
            name: name.into(),
            id: digest(c),
        }
    }

    fn rmi(fake: &mut Fake, users: &[User], given: &str, force: bool) -> Result<Vec<Removed>, String> {
        let records = fake.0.clone();
        delete(fake, &records, users, given, force).map_err(|e| e.said)
    }

    fn user(running: bool, c: char) -> User {
        User {
            id: "f".repeat(64),
            running,
            image: Some(digest(c).to_string()),
        }
    }

    use Removed::{Deleted, Untagged};

    /// As measured against dockerd 29.3.1 (2026-10-02): a name of several untags; the
    /// last deletes; an ID with several names, or one a container uses, must be forced; a
    /// running container's cannot be; forced, a used image's last name goes and the image
    /// stays, dangling.
    #[test]
    fn images_go_as_dockerd_removes_them() {
        let a = |n| record(n, 'a');
        let mut fake = Fake(vec![a("docker.io/shp/a:1"), a("docker.io/shp/a:2")]);
        assert_eq!(
            rmi(&mut fake, &[], "shp/a:1", false),
            Ok(vec![Untagged("shp/a:1".into())])
        );
        assert_eq!(
            rmi(&mut fake, &[], "shp/a:2", false),
            Ok(vec![Untagged("shp/a:2".into()), Deleted(digest('a').to_string())])
        );
        assert!(fake.0.is_empty());
        assert_eq!(
            rmi(&mut fake, &[], "shp/nosuch:1", false),
            Err("Error response from daemon: No such image: shp/nosuch:1".into())
        );

        let b = |n| record(n, 'b');
        let mut fake = Fake(vec![b("docker.io/library/busybox:1.37"), b("docker.io/shp/b:1")]);
        let id = "bbbbbbbbbbbb";
        assert_eq!(
            rmi(&mut fake, &[], id, false),
            Err(format!(
                "Error response from daemon: conflict: unable to delete {id} (must be forced) - image is referenced in multiple repositories"
            ))
        );
        assert_eq!(fake.0.len(), 2);
        assert_eq!(
            rmi(&mut fake, &[], id, true),
            Ok(vec![
                Untagged("busybox:1.37".into()),
                Untagged("shp/b:1".into()),
                Deleted(digest('b').to_string())
            ])
        );

        let u = |n| record(n, 'c');
        let mut fake = Fake(vec![u("docker.io/shp/u:1")]);
        let running = [user(true, 'c')];
        assert_eq!(
            rmi(&mut fake, &running, "shp/u:1", false),
            Err("Error response from daemon: conflict: unable to delete shp/u:1 (must be forced) - container ffffffffffff is using its referenced image cccccccccccc".into())
        );
        assert_eq!(
            rmi(&mut fake, &running, "cccccccccccc", true),
            Err("Error response from daemon: conflict: unable to delete cccccccccccc (cannot be forced) - image is being used by running container ffffffffffff".into())
        );
        assert_eq!(
            rmi(&mut fake, &running, "shp/u:1", true),
            Ok(vec![Untagged("shp/u:1".into())])
        );
        let dangling = format!("{DANGLING}{}", digest('c'));
        assert_eq!(
            fake.0,
            [Record {
                name: dangling.clone(),
                id: digest('c')
            }]
        );
        // Stopped, its container's image goes with -f, and is not untagged by name.
        let stopped = [user(false, 'c')];
        assert_eq!(
            rmi(&mut fake, &stopped, "cccc", false),
            Err("Error response from daemon: conflict: unable to delete cccccccccccc (must be forced) - image is being used by stopped container ffffffffffff".into())
        );
        assert_eq!(
            rmi(&mut fake, &stopped, "cccc", true),
            Ok(vec![Deleted(digest('c').to_string())])
        );
        assert!(fake.0.is_empty());
    }

    /// A name with a digest takes every tag of its repository; a tag of another
    /// repository stays; a short ID shared by two images is ambiguous.
    #[test]
    fn names_with_digests_and_short_ids_resolve_as_dockerd_resolves_them() {
        let mut fake = Fake(vec![
            record("docker.io/library/alpine:3.22", 'a'),
            record("docker.io/library/alpine:latest", 'a'),
            record("docker.io/shp/x:1", 'a'),
        ]);
        let given = format!("alpine@{}", digest('a'));
        assert_eq!(
            rmi(&mut fake, &[], &given, false),
            Ok(vec![
                Untagged("alpine:3.22".into()),
                Untagged("alpine:latest".into())
            ])
        );
        assert_eq!(fake.0, [record("docker.io/shp/x:1", 'a')]);
        let mut fake = Fake(vec![
            Record {
                name: "docker.io/library/a:1".into(),
                id: Digest::parse(&format!("sha256:abcd1{}", "0".repeat(59))).unwrap(),
            },
            Record {
                name: "docker.io/library/b:1".into(),
                id: Digest::parse(&format!("sha256:abcd2{}", "0".repeat(59))).unwrap(),
            },
        ]);
        // A tag and its repository's digest are one name: its ID removes both unforced.
        let mut both = Fake(vec![
            record("docker.io/library/alpine:3.22", 'd'),
            record(&format!("docker.io/library/alpine@{}", digest('d')), 'd'),
        ]);
        assert_eq!(
            rmi(&mut both, &[], "dddddddd", false),
            Ok(vec![
                Untagged("alpine:3.22".into()),
                Untagged(format!("alpine@{}", digest('d'))),
                Deleted(digest('d').to_string())
            ])
        );
        assert_eq!(
            rmi(&mut fake, &[], "abcd", true),
            Err("Error response from daemon: ambiguous reference".into())
        );
        assert_eq!(
            rmi(&mut fake, &[], "abc", false),
            Err("Error response from daemon: No such image: abc:latest".into())
        );
        assert_eq!(fake.0.len(), 2);
    }
}
