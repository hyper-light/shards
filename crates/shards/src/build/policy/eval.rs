//! `shards buildx policy eval --print`: a source's policy input as buildx v0.37.1 prints it
//! (commands/policy/eval.go runEval): built from the source, the fields asked for with
//! `--fields` resolved (each material's provenance before its fields), what is still
//! unknown summarized, and the fields that name nothing unknown said to be invalid.

use std::collections::BTreeSet;

use shards_dockerfile::platform::Platform;

use super::input::Input;
use super::signatures::Trust;
use super::{Meta, Resolve, Source, provenance};

/// What `--print` prints and says: the input, the fields asked for that name nothing
/// unknown (`invalid fields`), and what is left unknown (`unresolved fields`).
pub struct Printed {
    pub input: Input,
    pub invalid: Vec<String>,
    pub unresolved: Vec<String>,
}

/// runEval's `--print` loop: at most four rounds, ending when the unknowns stop changing.
pub fn print_input(
    source: &Source,
    platform: &Platform,
    fields: &[String],
    resolver: &dyn Resolve,
    trust: &Trust,
) -> Result<Printed, String> {
    let mut quiet = |_: _, _: &str| {};
    let mut input =
        provenance::source_to_input(source, &Meta::default(), Some(platform), Some(trust), &mut quiet)?;
    let mut attempts = 5;
    let mut last: Option<Vec<String>> = None;
    let mut trimmed: Vec<String>;
    let mut invalid: Vec<String> = Vec::new();
    let mut reloaded = BTreeSet::new();
    loop {
        attempts -= 1;
        if attempts <= 0 {
            return Err("maximum attempts reached for resolving source metadata".into());
        }
        trimmed = input
            .unknown_refs()
            .iter()
            .map(|u| u.strip_prefix("input.").unwrap_or(u).to_string())
            .collect();
        if last.as_ref() == Some(&trimmed) {
            break;
        }
        last = Some(trimmed.clone());
        let to_reload;
        (to_reload, invalid) = select_reload_fields(fields, &trimmed);
        reloaded.extend(to_reload.iter().cloned());
        if !to_reload.is_empty() {
            let (retry, next) = provenance::resolve_input_unknowns(
                &mut input,
                source,
                &to_reload,
                Some(platform),
                Some(platform),
                Some(resolver),
                Some(trust),
                &mut quiet,
            )?;
            if let Some(request) = next {
                let meta = resolver.resolve(source, &request)?;
                input = provenance::source_to_input(source, &meta, Some(platform), Some(trust), &mut quiet)?;
                continue;
            }
            if retry {
                continue;
            }
        }
        break;
    }
    invalid.retain(|f| !reloaded.contains(f));
    let unresolved = summarize_unknowns(&trimmed, fields);
    sanitize(&mut input);
    Ok(Printed {
        input,
        invalid,
        unresolved,
    })
}

/// selectReloadFields: the unknowns the fields asked for need resolved (a material's
/// field its provenances too, or the nearest unknown above it), and the fields that need
/// nothing, in the order they were asked.
fn select_reload_fields(fields: &[String], unknowns: &[String]) -> (Vec<String>, Vec<String>) {
    let mut reload = BTreeSet::new();
    let mut invalid = Vec::new();
    for field in fields {
        if let Some(prereq) = material_prerequisites(field) {
            let mut added = false;
            for p in prereq {
                if unknowns.contains(&p) {
                    reload.insert(p);
                    added = true;
                }
            }
            if unknowns.contains(field) {
                reload.insert(field.clone());
                added = true;
            } else if let Some(ancestor) = unknown_ancestor(field, unknowns) {
                reload.insert(ancestor);
                added = true;
            }
            if !added {
                invalid.push(field.clone());
            }
            continue;
        }
        if unknowns.contains(field) {
            reload.insert(field.clone());
            continue;
        }
        invalid.push(field.clone());
    }
    (reload.into_iter().collect(), invalid)
}

/// findUnknownAncestor: the field itself where it is unknown, else the longest unknown
/// it is inside (`a.b` of `a.b.c` or `a.b[0]`).
fn unknown_ancestor(field: &str, unknowns: &[String]) -> Option<String> {
    let mut best: Option<&String> = None;
    for u in unknowns {
        if field == u {
            return Some(u.clone());
        }
        let inside = field
            .strip_prefix(u.as_str())
            .is_some_and(|rest| rest.starts_with('.') || rest.starts_with('['));
        if inside && best.is_none_or(|b| u.len() > b.len()) {
            best = Some(u);
        }
    }
    best.cloned()
}

/// materialFieldPrerequisites: for a field of a material, the provenance of the image and
/// of each material on the way to it, sorted; none for any other field.
fn material_prerequisites(field: &str) -> Option<Vec<String>> {
    const SEG: &str = ".image.provenance.materials[";
    const PROVENANCE: &str = ".image.provenance";
    if !field.starts_with(SEG.get(1..).unwrap_or_default()) {
        return None;
    }
    let mut out = BTreeSet::from([PROVENANCE.get(1..).unwrap_or_default().to_string()]);
    // collectMaterialPrerequisites: each nested material's, the first not (its segment
    // starts the field, without the dot searched for).
    let mut start = 0;
    while let Some(i) = field.get(start..).and_then(|rest| rest.find(SEG)) {
        let at = start + i;
        if let Some(head) = field.get(..at) {
            out.insert(format!("{head}{PROVENANCE}"));
        }
        start = at + SEG.len();
    }
    Some(out.into_iter().collect())
}

/// summarizeEvalUnknowns: of the fields asked for, those still unknown or inside an
/// unknown; with none asked, every unknown down to its source's field
/// (summarizeUnknownField), sorted.
fn summarize_unknowns(unknowns: &[String], requested: &[String]) -> Vec<String> {
    if unknowns.is_empty() {
        return Vec::new();
    }
    let mut out = BTreeSet::new();
    if !requested.is_empty() {
        for field in requested {
            if unknowns.contains(field) {
                out.insert(field.clone());
            } else if let Some(a) = unknown_ancestor(field, unknowns) {
                out.insert(a);
            }
        }
        return out.into_iter().collect();
    }
    for u in unknowns {
        out.insert(summarize_field(u));
    }
    out.into_iter().collect()
}

fn summarize_field(field: &str) -> String {
    if let Some((base, _)) = field.split_once(".materials[") {
        return format!("{base}.materials");
    }
    if field.starts_with("materials[") {
        return "materials".into();
    }
    let parts: Vec<&str> = field.split('.').collect();
    match parts.as_slice() {
        [a, b, ..] => format!("{a}.{b}"),
        _ => field.to_string(),
    }
}

/// sanitizePrintInput: no depth printed, the materials' neither.
fn sanitize(input: &mut Input) {
    input.env.depth = 0;
    if let Some(p) = input.image.as_mut().and_then(|i| i.provenance.as_mut()) {
        for m in &mut p.materials {
            sanitize(m);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }

    /// buildx's own cases (commands/policy/eval_test.go TestSelectReloadFields,
    /// TestFilterInvalidFields, TestMaterialFieldPrerequisites), each as it asserts it.
    #[test]
    fn fields_are_selected_as_buildx_selects_them() {
        let unknowns = s(&[
            "image.provenance",
            "image.provenance.materials[0].image.hasProvenance",
            "git.tag",
        ]);
        let select = |f: &[&str], u: &[String]| select_reload_fields(&s(f), u);
        // exact match
        assert_eq!(select(&["git.tag"], &unknowns), (s(&["git.tag"]), vec![]));
        // ancestor mapping
        assert_eq!(
            select(&["image.provenance.materials[0].image.labels"], &unknowns),
            (s(&["image.provenance"]), vec![])
        );
        // dedupe mapped reloads
        assert_eq!(
            select(
                &[
                    "image.provenance.materials[0].image.labels",
                    "image.provenance.materials[0].image.user"
                ],
                &unknowns
            ),
            (s(&["image.provenance"]), vec![])
        );
        // invalid fields reported
        assert_eq!(
            select(&["image.labels", "foo.bar"], &unknowns),
            (vec![], s(&["image.labels", "foo.bar"]))
        );
        // mix exact mapped invalid
        let (reload, invalid) = select(
            &[
                "git.tag",
                "image.provenance.materials[0].image.env",
                "no.such.field",
            ],
            &unknowns,
        );
        assert_eq!(sorted(reload), s(&["git.tag", "image.provenance"]));
        assert_eq!(invalid, s(&["no.such.field"]));
        // nested material prerequisites
        assert_eq!(
            select(
                &["image.provenance.materials[0].image.provenance.materials[1].image.labels"],
                &unknowns
            ),
            (s(&["image.provenance"]), vec![])
        );
        // material field after provenance loaded
        let only = s(&["image.provenance.materials[0].image.hasProvenance"]);
        assert_eq!(
            select(&["image.provenance.materials[0].image.hasProvenance"], &only),
            (only.clone(), vec![])
        );

        assert_eq!(material_prerequisites("image.provenance"), None);
        assert_eq!(
            material_prerequisites("image.provenance.materials[0].image.labels"),
            Some(s(&["image.provenance"]))
        );
        assert_eq!(
            material_prerequisites(
                "image.provenance.materials[0].image.provenance.materials[1].image.labels"
            ),
            Some(s(&[
                "image.provenance",
                "image.provenance.materials[0].image.provenance"
            ]))
        );
    }

    /// TestSanitizePrintInputClearsDepthRecursively.
    #[test]
    fn printed_inputs_have_no_depth() {
        let mut inner = Input::default();
        inner.env.depth = 2;
        let mut mid = Input::default();
        mid.env.depth = 3;
        mid.env.target = "app".into();
        mid.image = Some(super::super::input::Image {
            provenance: Some(Box::new(provenance::Provenance {
                materials: vec![inner],
                ..provenance::Provenance::default()
            })),
            ..super::super::input::Image::default()
        });
        let mut top = Input::default();
        top.env.depth = 7;
        top.env.filename = "Dockerfile".into();
        top.image = Some(super::super::input::Image {
            provenance: Some(Box::new(provenance::Provenance {
                materials: vec![mid],
                ..provenance::Provenance::default()
            })),
            ..super::super::input::Image::default()
        });
        sanitize(&mut top);
        assert_eq!(top.env.depth, 0);
        assert_eq!(top.env.filename, "Dockerfile");
        let mid = &top.image.as_ref().unwrap().provenance.as_ref().unwrap().materials[0];
        assert_eq!(mid.env.depth, 0);
        assert_eq!(mid.env.target, "app");
        let inner = &mid.image.as_ref().unwrap().provenance.as_ref().unwrap().materials[0];
        assert_eq!(inner.env.depth, 0);
    }

    /// summarizeEvalUnknowns, as eval.go defines it.
    #[test]
    fn unknowns_are_summarized_as_buildx_does() {
        let unknowns = s(&[
            "image.checksum",
            "image.labels",
            "git.ref",
            "image.provenance.materials[2].git.commit",
        ]);
        assert_eq!(
            summarize_unknowns(&unknowns, &[]),
            s(&[
                "git.ref",
                "image.checksum",
                "image.labels",
                "image.provenance.materials"
            ])
        );
        assert_eq!(
            summarize_unknowns(
                &unknowns,
                &s(&[
                    "image.labels",
                    "image.provenance.materials[2].git.commit.x",
                    "nope"
                ])
            ),
            s(&["image.labels", "image.provenance.materials[2].git.commit"])
        );
        assert_eq!(summarize_field("materials[0].image"), "materials");
        assert_eq!(summarize_field("local"), "local");
    }
}
