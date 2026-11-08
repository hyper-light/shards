use std::process::Command;
fn main() {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let triple = format!("{arch}-unknown-linux-musl");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let rustc = std::env::var("RUSTC").unwrap_or_default();
    let rustc_v = Command::new(&rustc).arg("-V").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let toolchain = std::env::var("RUSTUP_TOOLCHAIN").unwrap_or_else(|_| "unset".into());
    let exp_config = std::env::var("EXP_CONFIG").unwrap_or_else(|_| "unset".into());
    let o = Command::new("cargo")
        .current_dir(&root)
        .args(["build", "-q", "-p", "exp-guest", "--profile", "guest", "--target", &triple])
        .arg("--target-dir").arg(out.join("guest-target"))
        .output().unwrap();
    let bin = out.join("guest-target").join(&triple).join("guest").join("exp-guest");
    let (ok, size) = match std::fs::read(&bin) { Ok(b) if o.status.success() => { std::fs::write(out.join("guest.bin"), &b).unwrap(); (true, b.len()) }, _ => { std::fs::write(out.join("guest.bin"), b"").unwrap(); (false, 0) } };
    let err = String::from_utf8_lossy(&o.stderr).lines().filter(|l| l.contains("error")).take(2).collect::<Vec<_>>().join(" | ");
    std::fs::write(out.join("report.txt"), format!("outer rustc: {rustc_v}\nRUSTUP_TOOLCHAIN in build.rs: {toolchain}\nEXP_CONFIG in build.rs: {exp_config}\nnested guest build ok: {ok}, embedded {size} bytes\nnested stderr errors: {err}\n")).unwrap();
    println!("cargo:rerun-if-changed=../guest/src/main.rs");
}
