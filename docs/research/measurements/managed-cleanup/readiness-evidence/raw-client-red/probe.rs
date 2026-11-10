
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdout};
fn kernel() -> &'static Path { Path::new("/dev/null") }
fn guest_init() -> &'static Path { Path::new("/dev/null") }
mod common {
    use std::process::Command;
    pub fn command() -> Command {
        let mut command = Command::new("python3");
        command.arg(std::env::var_os("CLEANUP_CHILD_HELPER").unwrap());
        command
    }
    pub fn interruptible(command: &mut Command) -> &mut Command { command }
}
use std::process::Stdio;
struct Client {
    child: Child,
    output: BufReader<ChildStdout>,
}
fn ready(home: &Path, args: &[&str]) -> Client {
    let mut child = common::interruptible(
        common::command()
            .args(args)
            .env("SHARDS_HOME", home)
            .env("SHARDS_KERNEL", kernel())
            .env("SHARDS_INIT", guest_init())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()),
    )
    .spawn()
    .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    assert_eq!(line, "ready\n", "{args:?}: {line:?}");
    Client { child, output }
}
fn finished(client: &mut Client) -> String {
    let mut rest = String::new();
    client.output.read_to_string(&mut rest).unwrap();
    let status = client.child.wait().unwrap();
    assert!(status.success(), "{status}: {rest}");
    rest
}

unsafe extern "C" { fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32; }
fn main() {
    let mode = std::env::args().nth(1).unwrap();
    match std::panic::catch_unwind(|| ready(Path::new("."), &[&mode])) {
        Ok(mut client) => {
            println!("CLIENT_READY");
            std::io::stdout().flush().unwrap();
            let mut control = String::new();
            std::io::stdin().read_line(&mut control).unwrap();
            println!("CLIENT_FINISHED {:?}", finished(&mut client));
        }
        Err(_) => {
            println!("READINESS_PANIC");
            std::io::stdout().flush().unwrap();
            let mut control = String::new();
            std::io::stdin().read_line(&mut control).unwrap();
            let pid: i32 = control.trim().parse().unwrap();
            let mut status = 0;
            // SAFETY: the controller supplies only the controlled direct child's PID;
            // the unmodified ready path dropped its handle without waiting for it.
            let waited = unsafe { waitpid(pid, &mut status, 0) };
            assert_eq!(waited, pid);
            println!("RESCUED_OWNED_CHILD {waited} status={status}");
        }
    }
}
