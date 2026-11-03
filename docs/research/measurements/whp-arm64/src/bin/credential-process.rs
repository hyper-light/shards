//! Compare AWS credential command quoting with the Windows shell (M414 follow-up).

#[cfg(not(windows))]
fn main() {
    println!("credential-process runs on Windows");
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    let root = std::env::var_os("SystemRoot").ok_or("SystemRoot is absent")?;
    let shell = std::path::Path::new(&root).join("System32").join("cmd.exe");
    let quoted = format!("\"{}\" /D /C echo quoted-path", shell.display());
    let cases = [
        (
            "credentials",
            r#"echo {"Version":1,"AccessKeyId":"%AWS_MARK%","SecretAccessKey":"fixture"}"#,
            0,
            r#"{"Version":1,"AccessKeyId":"listed","SecretAccessKey":"fixture"}"#,
        ),
        ("quoted-program", quoted.as_str(), 0, "quoted-path"),
        ("exit-status", "echo no >&2 & exit /b 3", 3, ""),
    ];
    for (name, command, code, expected) in cases {
        for raw in [false, true] {
            let mut child = Command::new("cmd.exe");
            if raw {
                // cmd /S removes the enclosing pair; the configured command's own
                // quotes and shell syntax remain intact (CommandExt::raw_arg docs).
                child.args(["/D", "/S", "/C"]).raw_arg(format!("\"{command}\""));
            } else {
                child.args(["/C", command]);
            }
            let output = child.env("AWS_MARK", "listed").output()?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            println!(
                "{name} raw={raw} code={:?} stdout={stdout:?} stderr={:?}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            );
            if raw && (output.status.code() != Some(code) || stdout.trim() != expected) {
                return Err(format!("{name}: raw shell command did not match").into());
            }
        }
    }
    Ok(())
}
