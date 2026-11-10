// Private pure tests of the exact existing Usage declaration/parser extraction.
// No STATS request, OS observation, exporter or management actor runs here.
use super::Usage;

#[test]
fn complete_numeric_sample_preserves_all_fields() {
    let u = Usage::parse(
        "cpu 1\nsystem 2\nonline 3\nmemory 4\ninactive_file 5\nlimit 6\npids 7\nread 8\nwrite 9\nrx 10\ntx 11\n",
    );
    assert_eq!(
        (
            u.cpu,
            u.system,
            u.online,
            u.memory,
            u.inactive_file,
            u.limit,
            u.pids,
            u.read,
            u.write,
            u.rx,
            u.tx,
        ),
        (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11)
    );
}

#[test]
fn absent_and_explicit_zero_fields_are_indistinguishable() {
    let absent = Usage::parse("");
    let zero = Usage::parse(
        "cpu 0\nsystem 0\nonline 0\nmemory 0\ninactive_file 0\nlimit 0\npids 0\nread 0\nwrite 0\nrx 0\ntx 0\n",
    );
    assert_eq!(absent, zero);
    assert_eq!(absent, Usage::default());
}

#[test]
fn invalid_and_overflowing_values_become_zero() {
    let u = Usage::parse("memory unavailable\npids 18446744073709551616\ncpu -1\n");
    assert_eq!(u, Usage::default());
}

#[test]
fn duplicate_fields_use_the_last_value() {
    let u = Usage::parse("memory 10\nmemory 20\n");
    assert_eq!(u.memory, 20);
}

#[test]
fn incomplete_lines_leave_other_fields_at_default() {
    let u = Usage::parse("cpu 7\nmemory\npids \n");
    assert_eq!(u.cpu, 7);
    assert_eq!(u.memory, 0);
    assert_eq!(u.pids, 0);
}

#[test]
fn unrecognized_fields_do_not_create_known_values() {
    assert_eq!(Usage::parse("unrecognized 100\n"), Usage::default());
}

#[test]
fn numeric_u64_max_is_preserved_without_an_overflow_status() {
    let u = Usage::parse("cpu 18446744073709551615\n");
    assert_eq!(u.cpu, u64::MAX);
}
