//! A real Agentfile build through `shards_api::Builds` (D263): an ignored-unless-asked test
//! its runner runs where `SHARDS_API_BUILD=1`, `SHARDS_BIN` naming the `shards` to build with.
//! Its Agentfile copies a file into `scratch`, so no base image is pulled and no builder
//! microVM boots: what it holds to is the API's path, the request, the events, the image's
//! ID in the caller's home's store, and a failing step refused by its number.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout
)]

use shards_api::{BuildEvent, BuildRefusal, BuildSpec, Builds, StepEnd};

fn asked() -> bool {
    let asked = std::env::var_os("SHARDS_API_BUILD").is_some_and(|v| v == "1");
    if !asked {
        println!("SKIP: SHARDS_API_BUILD=1 builds through a real shards");
    }
    asked
}

#[test]
fn an_agentfile_builds_through_the_api_into_the_callers_home() {
    if !asked() {
        return;
    }
    let dir = shards_testdir::TempDir::new("api-build").unwrap();
    let context = dir.path().join("ctx");
    std::fs::create_dir_all(&context).unwrap();
    std::fs::write(context.join("hello"), "hello\n").unwrap();
    std::fs::write(context.join("Agentfile"), "FROM scratch\nCOPY hello /hello\n").unwrap();
    std::fs::write(context.join("Broken"), "FROM scratch\nCOPY missing /missing\n").unwrap();
    let home = dir.path().join("home");
    let builds = Builds {
        binary: None,
        home: Some(home.clone()),
    };
    let spec = BuildSpec {
        context: context.clone(),
        tag: Some("api-build-test:1".into()),
        ..BuildSpec::default()
    };
    let mut events = Vec::new();
    let pinned = builds.build(&spec, &mut |e| events.push(e)).unwrap();
    println!("built {pinned:?}, {} events", events.len());
    let hex = pinned.digest.strip_prefix("sha256:").expect("a sha256 digest");
    assert_eq!(hex.len(), 64, "{pinned:?}");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BuildEvent::Step { name, .. } if name.contains("COPY hello"))),
        "{events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            BuildEvent::Ended {
                how: StepEnd::Done,
                ..
            }
        )),
        "{events:?}"
    );
    assert!(home.is_dir(), "the build wrote to the caller's home");
    let broken = BuildSpec {
        file: Some(context.join("Broken")),
        ..spec
    };
    match builds.build(&broken, &mut |_| {}) {
        Err(BuildRefusal::Step { name, message, .. }) => {
            println!("refused at {name:?}: {message}");
            assert!(name.contains("COPY missing"), "{name}");
        }
        other => panic!("{other:?}"),
    }
}
