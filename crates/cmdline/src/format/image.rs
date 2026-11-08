//! `images`' rows (formatter/image.go's imageContext, ImageWrite and NewImageFormat).

use shards_template::Value;

use super::reference::parse_normalized_named;
use super::units::{human_size, human_size_precision};
use super::{Clock, Context, Ctx, DEFAULT_QUIET, Header, Methods, RAW, TABLE, truncate_id};

/// An image as `images` lists it: the fields of moby's image.Summary the CLI prints.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Image {
    pub id: String,
    pub repo_tags: Vec<String>,
    pub repo_digests: Vec<String>,
    /// Seconds since the epoch.
    pub created: i64,
    pub size: i64,
    /// -1 where not worked out.
    pub shared_size: i64,
    /// -1 where not worked out.
    pub containers: i64,
}

const DEFAULT_TABLE: &str = "table {{.Repository}}\t{{.Tag}}\t{{.ID}}\t{{if .CreatedSince }}{{.CreatedSince}}{{else}}N/A{{end}}\t{{.Size}}";
const DEFAULT_TABLE_DIGEST: &str = "table {{.Repository}}\t{{.Tag}}\t{{.Digest}}\t{{.ID}}\t{{if .CreatedSince }}{{.CreatedSince}}{{else}}N/A{{end}}\t{{.Size}}";

/// NewImageFormat: the format `images` writes with, from `--format` (`table` when none),
/// `--quiet` and `--digests`.
pub fn format(source: &str, quiet: bool, digest: bool) -> String {
    match source {
        TABLE if quiet => DEFAULT_QUIET.into(),
        TABLE if digest => DEFAULT_TABLE_DIGEST.into(),
        TABLE => DEFAULT_TABLE.into(),
        RAW if quiet => "image_id: {{.ID}}".into(),
        RAW if digest => {
            "repository: {{ .Repository }}\ntag: {{.Tag}}\ndigest: {{.Digest}}\nimage_id: {{.ID}}\n\
                          created_at: {{.CreatedAt}}\nvirtual_size: {{.Size}}\n"
                .into()
        }
        RAW => {
            "repository: {{ .Repository }}\ntag: {{.Tag}}\nimage_id: {{.ID}}\ncreated_at: {{.CreatedAt}}\n\
                virtual_size: {{.Size}}\n"
                .into()
        }
        _ if super::is_table(source) && digest && !source.contains("{{.Digest}}") => {
            format!("{source}\t{{{{.Digest}}}}")
        }
        _ => source.into(),
    }
}

/// ImageWrite: the images, a row for each repository and tag (and digest, with
/// `digest` or a format showing digests), as `ctx.format` says.
pub fn write(ctx: &Context<'_>, digest: bool, images: &[Image], out: &mut String) -> Result<(), String> {
    let need_digest = digest || ctx.format.contains("{{.Digest}}");
    let mut rows = Vec::new();
    for img in images {
        for (repo, tag, dgst) in repo_rows(img, digest, need_digest) {
            rows.push(Ctx::value(Row::new(img, ctx.trunc, ctx.clock, repo, tag, dgst)));
        }
    }
    super::write(ctx, &HEADER, rows, out)
}

/// isDangling.
pub(super) fn is_dangling(img: &Image) -> bool {
    if img.repo_tags.is_empty() && img.repo_digests.is_empty() {
        return true;
    }
    img.repo_tags == ["<none>:<none>"] && img.repo_digests == ["<none>@<none>"]
}

/// imageFormat and imageFormatTaggedAndDigest: each repository's tags (with each digest,
/// when shown), then the repositories with only digests. Go ranges over maps for these;
/// here repositories come in the order the image names them first.
fn repo_rows(img: &Image, digest: bool, need_digest: bool) -> Vec<(String, String, String)> {
    let none = || "<none>".to_string();
    if is_dangling(img) {
        return vec![(none(), none(), none())];
    }
    let mut repo_tags: Vec<(String, Vec<String>)> = Vec::new();
    let mut repo_digests: Vec<(String, Vec<String>)> = Vec::new();
    let add = |map: &mut Vec<(String, Vec<String>)>, repo: String, v: String| match map
        .iter_mut()
        .find(|(r, _)| *r == repo)
    {
        Some((_, vs)) => vs.push(v),
        None => map.push((repo, vec![v])),
    };
    for r in &img.repo_tags {
        if let Some(r) = parse_normalized_named(r)
            && !r.tag.is_empty()
        {
            add(&mut repo_tags, r.familiar_name(), r.tag.clone());
        }
    }
    for r in &img.repo_digests {
        if let Some(r) = parse_normalized_named(r)
            && !r.digest.is_empty()
        {
            add(&mut repo_digests, r.familiar_name(), r.digest.clone());
        }
    }
    let mut rows = Vec::new();
    for (repo, tags) in repo_tags {
        let mut digests = Vec::new();
        if let Some(i) = repo_digests.iter().position(|(r, _)| *r == repo) {
            digests = repo_digests.remove(i).1;
        }
        if !need_digest {
            digests.clear();
        }
        for tag in tags {
            if digests.is_empty() {
                rows.push((repo.clone(), tag, none()));
                continue;
            }
            for d in &digests {
                rows.push((repo.clone(), tag.clone(), d.clone()));
            }
        }
    }
    for (repo, digests) in repo_digests {
        if digest {
            for d in digests {
                rows.push((repo.clone(), none(), d));
            }
        } else {
            rows.push((repo, none(), String::new()));
        }
    }
    rows
}

pub(super) static HEADER: Header = Header(&[
    ("Containers", "CONTAINERS"),
    ("CreatedAt", "CREATED AT"),
    ("CreatedSince", "CREATED"),
    ("Digest", "DIGEST"),
    ("ID", "IMAGE ID"),
    ("Repository", "REPOSITORY"),
    ("SharedSize", "SHARED SIZE"),
    ("Size", "SIZE"),
    ("Tag", "TAG"),
    ("UniqueSize", "UNIQUE SIZE"),
]);

/// imageContext, with its times worked out from the clock.
#[derive(Debug)]
pub(super) struct Row {
    i: Image,
    trunc: bool,
    repo: String,
    tag: String,
    digest: String,
    created_at: String,
    created_since: String,
}

impl Row {
    pub(super) fn new(
        i: &Image,
        trunc: bool,
        clock: &Clock<'_>,
        repo: String,
        tag: String,
        digest: String,
    ) -> Row {
        let created = i128::from(i.created) * 1_000_000_000;
        // time.Unix(created, 0).IsZero: January 1, year 1.
        let created_since = if i.created == -62_135_596_800 {
            String::new()
        } else {
            clock.ago(created)
        };
        Row {
            i: i.clone(),
            trunc,
            repo,
            tag,
            digest,
            created_at: clock.string(created),
            created_since,
        }
    }
}

impl Methods for Row {
    fn type_name(&self) -> &'static str {
        "*formatter.imageContext"
    }
    const METHODS: &'static [&'static str] = &[
        "Containers",
        "CreatedAt",
        "CreatedSince",
        "Digest",
        "ID",
        "Repository",
        "SharedSize",
        "Size",
        "Tag",
        "UniqueSize",
    ];
    const HEADER: &'static Header = &HEADER;

    #[allow(clippy::cast_precision_loss)]
    fn get(&self, name: &str) -> Option<Value> {
        let i = &self.i;
        let s = match name {
            "Containers" if i.containers == -1 => "N/A".into(),
            "Containers" => i.containers.to_string(),
            "CreatedAt" => self.created_at.clone(),
            "CreatedSince" => self.created_since.clone(),
            "Digest" => self.digest.clone(),
            "ID" if self.trunc => truncate_id(&i.id),
            "ID" => i.id.clone(),
            "Repository" => self.repo.clone(),
            "SharedSize" if i.shared_size == -1 => "N/A".into(),
            "SharedSize" => human_size(i.shared_size as f64),
            "Size" => human_size_precision(i.size as f64, 3),
            "Tag" => self.tag.clone(),
            "UniqueSize" if i.size == -1 || i.shared_size == -1 => "N/A".into(),
            "UniqueSize" => human_size(i.size.wrapping_sub(i.shared_size) as f64),
            _ => return None,
        };
        Some(Value::String(s))
    }
}
