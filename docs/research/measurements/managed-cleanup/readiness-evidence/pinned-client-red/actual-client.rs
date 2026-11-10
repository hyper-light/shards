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
