use std::{env, fs, path::PathBuf};

fn function(text: &str, name: &str) -> String {
    let start = text.find(&format!("fn {name}(")).expect("function present");
    let open = start + text[start..].find('{').expect("body present");
    let mut depth = 0;
    for (i, byte) in text.bytes().enumerate().skip(open) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return text[start..=i].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("unclosed function {name}");
}

fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../../../..");
    let mut extracted = String::new();
    for (path, name) in [
        ("crates/init/src/run.rs", "each_frame"),
        ("crates/shards/src/workload.rs", "log_record"),
        ("crates/shards/src/spec.rs", "now"),
    ] {
        let path = root.join(path);
        println!("cargo:rerun-if-changed={}", path.display());
        extracted.push_str(&function(&fs::read_to_string(path).unwrap(), name));
        extracted.push('\n');
    }
    let path = root.join("crates/shards/src/daemon/commands.rs");
    println!("cargo:rerun-if-changed={}", path.display());
    let commands = fs::read_to_string(path).unwrap();
    let start = commands.find("#[derive(Default)]\nstruct Log {").unwrap();
    let end = start + commands[start..].find("impl Line {").unwrap();
    extracted.push_str(&commands[start..end]);
    extracted.push_str("impl Log {\n");
    for name in ["read", "split", "take_lines"] {
        extracted.push_str(&function(&commands, name));
        extracted.push('\n');
    }
    extracted.push_str("}\n");
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("functions.rs"),
        extracted,
    )
    .unwrap();
}
