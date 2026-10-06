//! shards' skill validator holds to the Agent Skills reference validator: every answer
//! skills-ref gave in testdata/skills-answers.json (scripts/skills/generate), over real
//! skills and crafted ones, shards gives, error for error. Where the frontmatter is no
//! YAML strictyaml reads, shards says so in its own words after the reference's prefix
//! (src/skill.rs): only that prefix is held.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::path::Path;

const YAML: &str = "Invalid YAML in frontmatter: ";

#[test]
fn skills_are_checked_as_the_reference_validator_checks_them() {
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    let answers: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&std::fs::read(data.join("skills-answers.json")).unwrap()).unwrap();
    assert!(answers.len() > 80, "the corpus is all there");
    let mut failures = Vec::new();
    for (case, want) in &answers {
        let dir = data.join("skills").join(case);
        let name = dir.file_name().unwrap().to_str().unwrap();
        let md = ["SKILL.md", "skill.md"]
            .iter()
            .find_map(|f| std::fs::read(dir.join(f)).ok());
        let got = shards_build::skill::validate(name, md.as_deref());
        let front = frontmatter(md.as_deref());
        if front != want["frontmatter"] {
            failures.push(format!(
                "{case} frontmatter\n  got:  {front}\n  want: {}",
                want["frontmatter"]
            ));
        }
        let want: Vec<String> = want["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e.as_str().unwrap().to_string())
            .collect();
        let held = |v: &[String]| -> Vec<String> {
            v.iter()
                .map(|e| {
                    if e.starts_with(YAML) {
                        YAML.to_string()
                    } else {
                        e.clone()
                    }
                })
                .collect()
        };
        if held(&got) != held(&want) {
            failures.push(format!("{case}\n  got:  {got:?}\n  want: {want:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The frontmatter as shards reads it, in the oracle's JSON: null where it reads none.
fn frontmatter(md: Option<&[u8]>) -> serde_json::Value {
    use shards_build::skill::{Yaml, parse_yaml};
    fn json(y: &Yaml) -> serde_json::Value {
        match y {
            Yaml::Str(s) => s.clone().into(),
            Yaml::Seq(items) => items.iter().map(json).collect(),
            Yaml::Map(m) => m
                .iter()
                .map(|(k, v)| (k.clone(), json(v)))
                .collect::<serde_json::Map<_, _>>()
                .into(),
        }
    }
    let Some(text) = md.and_then(|b| std::str::from_utf8(b).ok()) else {
        return serde_json::Value::Null;
    };
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    if !text.starts_with("---") {
        return serde_json::Value::Null;
    }
    let parts: Vec<&str> = text.splitn(3, "---").collect();
    let Some(front) = parts.get(1).filter(|_| parts.len() == 3) else {
        return serde_json::Value::Null;
    };
    parse_yaml(front).map_or(serde_json::Value::Null, |y| json(&y))
}
