//! What a directory does with names that differ in case alone (D235): the same steps, one
//! result each, taken by a host test in a directory of its host's and by the test guest in
//! its view of a share of another, so that the guest's are held to the host's own.

extern crate std;

use std::format;
use std::fs;
use std::io;
use std::path::Path;
use std::string::{String, ToString};
use std::vec::Vec;

/// Each step's result, `STEP RESULT`: `ok`, the bytes read, the names listed, or the
/// error's kind.
pub fn steps(dir: &Path) -> Vec<String> {
    let said = |r: io::Result<String>| match r {
        Ok(s) => s,
        Err(e) => format!("{:?}", e.kind()),
    };
    let done = |r: io::Result<()>| said(r.map(|()| "ok".to_string()));
    let found = |name: &str| said(fs::symlink_metadata(dir.join(name)).map(|_| "ok".to_string()));
    let list = || -> io::Result<String> {
        let mut names: Vec<String> = fs::read_dir(dir)?
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        Ok(names.join(","))
    };
    let mut out = Vec::new();
    out.push(format!("write Foo {}", done(fs::write(dir.join("Foo"), "one"))));
    for name in ["Foo", "foo", "FOO"] {
        out.push(format!("stat {name} {}", found(name)));
    }
    let made = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join("foo"))
        .map(|_| ());
    out.push(format!("create-new foo {}", done(made)));
    out.push(format!("list {}", said(list())));
    out.push(format!("read FOO {}", said(fs::read_to_string(dir.join("FOO")))));
    out.push(format!("write foo {}", done(fs::write(dir.join("foo"), "two"))));
    out.push(format!("read Foo {}", said(fs::read_to_string(dir.join("Foo")))));
    out.push(format!(
        "rename Foo Bar {}",
        done(fs::rename(dir.join("Foo"), dir.join("Bar")))
    ));
    for name in ["foo", "FOO", "Foo", "Bar", "bar"] {
        out.push(format!("stat {name} {}", found(name)));
    }
    out.push(format!("list {}", said(list())));
    out.push(format!("remove BAR {}", done(fs::remove_file(dir.join("BAR")))));
    for name in ["Bar", "bar", "foo"] {
        out.push(format!("stat {name} {}", found(name)));
    }
    out.push(format!("read foo {}", said(fs::read_to_string(dir.join("foo")))));
    out.push(format!("mkdir Dir {}", done(fs::create_dir(dir.join("Dir")))));
    out.push(format!("mkdir dir {}", done(fs::create_dir(dir.join("dir")))));
    out.push(format!("stat DIR {}", found("DIR")));
    out.push(format!("list {}", said(list())));
    // Renames that change a name's case alone, a directory's and a file's.
    out.push(format!(
        "rename Dir dIR {}",
        done(fs::rename(dir.join("Dir"), dir.join("dIR")))
    ));
    out.push(format!("list {}", said(list())));
    out.push(format!("write low {}", done(fs::write(dir.join("low"), "three"))));
    out.push(format!(
        "rename low LOW {}",
        done(fs::rename(dir.join("low"), dir.join("LOW")))
    ));
    out.push(format!("list {}", said(list())));
    out.push(format!("read low {}", said(fs::read_to_string(dir.join("low")))));
    out
}
