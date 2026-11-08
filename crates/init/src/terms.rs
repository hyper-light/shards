//! The TERM a command with a pty is told where its environment names none and its client
//! said what its terminal is (`Spec::terms`, D16): the first of those types its root has a
//! description of, which its programs read its keys and colours by.

use std::path::{Path, PathBuf};

/// The first of `terms` the root at `root` describes (`/` for the run's own, which the
/// standby looks from), else the last: `TERM=NAME`, as the command's environment holds it.
/// A description is an entry where ncurses looks with the command's environment, `env`
/// (terminfo(5), "Fetching Compiled Descriptions"): TERMINFO, HOME's `.terminfo`, each of
/// TERMINFO_DIRS (an empty one the system's), then /etc/terminfo, /lib/terminfo,
/// /usr/share/terminfo and /usr/lib/terminfo; each filed under its first character, or that
/// character's hex code (term(5), "Storage"). A name that could name no entry is passed by
/// (`shards_abi::run::terminal_name`), as one the daemon would not have given.
pub fn term(root: &Path, terms: &[Vec<u8>], env: &[Vec<u8>]) -> Option<Vec<u8>> {
    let var = |name: &[u8]| {
        env.iter().rev().find_map(|kv| {
            kv.strip_prefix(name)
                .and_then(|rest| rest.strip_prefix(b"="))
                .filter(|v| !v.is_empty())
        })
    };
    let path = |bytes: &[u8]| PathBuf::from(String::from_utf8_lossy(bytes).into_owned());
    let system = [
        "/etc/terminfo",
        "/lib/terminfo",
        "/usr/share/terminfo",
        "/usr/lib/terminfo",
    ];
    let mut dirs: Vec<PathBuf> = var(b"TERMINFO").map(path).into_iter().collect();
    dirs.extend(var(b"HOME").map(|h| path(h).join(".terminfo")));
    if let Some(list) = var(b"TERMINFO_DIRS") {
        for d in list.split(|&b| b == b':') {
            if d.is_empty() {
                dirs.extend(system.map(PathBuf::from));
            } else {
                dirs.push(path(d));
            }
        }
    }
    dirs.extend(system.map(PathBuf::from));
    let described = |name: &[u8]| {
        let Some(&first) = name.first() else {
            return false;
        };
        let file = String::from_utf8_lossy(name).into_owned();
        dirs.iter().any(|dir| {
            // Within the root, wherever the environment names.
            let dir = root.join(dir.strip_prefix("/").unwrap_or(dir));
            [char::from(first).to_string(), format!("{first:02x}")]
                .iter()
                .any(|sub| dir.join(sub).join(&file).is_file())
        })
    };
    let valid: Vec<&Vec<u8>> = terms
        .iter()
        .filter(|t| shards_abi::run::terminal_name(t))
        .collect();
    let chosen = valid.iter().find(|t| described(t)).or(valid.last())?;
    Some([&b"TERM="[..], chosen].concat())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn terms(list: &[&str]) -> Vec<Vec<u8>> {
        list.iter().map(|t| t.as_bytes().to_vec()).collect()
    }

    /// The first of the client's types the root describes, wherever ncurses would find it,
    /// filed by letter or by hex code; else the last; never one that names no entry.
    #[test]
    fn the_first_type_the_root_describes_is_the_commands() {
        let root = shards_testdir::TempDir::new("terms").unwrap();
        let file = |at: &str| {
            let p = root.join(at);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"entry").unwrap();
        };
        let said = |list: &[&str], env: &[&str]| {
            let env: Vec<Vec<u8>> = env.iter().map(|e| e.as_bytes().to_vec()).collect();
            term(&root, &terms(list), &env).map(|t| String::from_utf8(t).unwrap())
        };
        let client = ["xterm-ghostty", "xterm-256color", "xterm"];
        // None described: the last.
        assert_eq!(said(&client, &[]).unwrap(), "TERM=xterm");
        file("usr/share/terminfo/x/xterm-256color");
        assert_eq!(said(&client, &[]).unwrap(), "TERM=xterm-256color");
        // Filed by hex code, as on a filesystem whose names ignore case.
        file("etc/terminfo/78/xterm-ghostty");
        assert_eq!(said(&client, &[]).unwrap(), "TERM=xterm-ghostty");
        // Where the command's environment says ncurses looks.
        let alacritty = ["alacritty", "xterm"];
        assert_eq!(said(&alacritty, &[]).unwrap(), "TERM=xterm");
        file("opt/ti/a/alacritty");
        assert_eq!(said(&alacritty, &["TERMINFO=/opt/ti"]).unwrap(), "TERM=alacritty");
        assert_eq!(
            said(&alacritty, &["TERMINFO_DIRS=/x:/opt/ti"]).unwrap(),
            "TERM=alacritty"
        );
        file("home/u/.terminfo/a/alacritty");
        assert_eq!(said(&alacritty, &["HOME=/home/u"]).unwrap(), "TERM=alacritty");
        // A name past what an entry may be filed under is passed by, described or not.
        file("usr/share/terminfo/x/x..");
        assert_eq!(said(&["../x", "..", "xterm"], &[]).unwrap(), "TERM=xterm");
        assert_eq!(said(&["../escape"], &[]), None);
    }
}
