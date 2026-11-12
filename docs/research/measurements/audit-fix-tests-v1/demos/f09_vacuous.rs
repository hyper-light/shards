// Finding 9: managed_output/tenant_exit.rs:254-262 asserted only that no `top` row carries the
// ended member's uid. The predicate below is that assertion's, verbatim.
fn old_predicate(stdout: &str, ended_uid: &str) -> bool {
    stdout
        .lines()
        .skip(1)
        .all(|line| line.split_whitespace().nth(2) != Some(ended_uid))
}

fn main() {
    let ended = "200001";
    let header = "PID PPID UID NLWP STAT COMMAND\n";
    let cases = [
        ("empty output", String::new()),
        ("header only: no member rows at all", header.to_string()),
        (
            "neighbor's live row, ended member absent",
            format!("{header}924 921 200002 1 S member\n"),
        ),
        (
            "columns differ: uid is not the third field",
            "PID UID STAT COMMAND\n924 200001 S member\n".to_string(),
        ),
        (
            "ended member left a zombie",
            format!("{header}924 921 200002 1 S member\n930 921 200001 1 Z member\n"),
        ),
    ];
    for (what, table) in cases {
        println!("{what}: old predicate passes = {}", old_predicate(&table, ended));
    }
}
