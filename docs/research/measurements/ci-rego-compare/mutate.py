#!/usr/bin/env python3
"""One text-removal mutant per fix, each run against the test named for it, then restored.

Run from the repository's root: python3 -I docs/research/measurements/ci-rego-compare/mutate.py
Each mutant must fail its test; a mutant whose test passes is a guard no test holds. A
source is restored by writing it back, which gives it a new mtime, so cargo rebuilds it.
"""
import pathlib
import subprocess
import sys
import time

ROOT = pathlib.Path.cwd()
NUMBER = "crates/rego/src/number.rs"
VALUE = "crates/rego/src/value.rs"
NUMBERS = "crates/rego/src/funcs/collections/numbers.rs"
ENCODING = "crates/rego/src/funcs/encoding.rs"
PARSER = "crates/rego/src/parser.rs"

LIB = ["cargo", "test", "-p", "shards-rego", "--lib"]
BUILTINS = ["cargo", "test", "-p", "shards-rego", "--test", "builtins", "numbers_with_huge"]

MUTANTS = [
    (
        "comparison: the exact product made for every power of five a u32 holds",
        VALUE,
        ".ok().filter(|n| u64::from(*n) <= q.bits() / 2)",
        ".ok()",
        LIB + ["--", "value::tests::numbers_compare_exactly_at_every_distance"],
    ),
    (
        "alignment: Go's, to the lower exponent however far",
        NUMBER,
        "let at = x.exp.min(y.exp).max(floor);",
        "let at = x.exp.min(y.exp);",
        LIB + ["--", "number::tests::alignment_is_bounded_by_the_precision_not_the_gap"],
    ),
    (
        "alignment: the borrow of what the smaller dropped",
        NUMBER,
        "let mant = if dropped { d - 1u32 } else { d };",
        "let mant = d;",
        LIB + ["--", "number::tests::sums_and_differences_round_as_the_exact_ones"],
    ),
    (
        "pow5: the squaring after the factor's last use",
        NUMBER,
        "        n >>= 1;\n        if n > 0 {\n            f = Float::mul(&f, &f, fprec)?;\n        }\n",
        "        n >>= 1;\n        f = Float::mul(&f, &f, fprec)?;\n",
        LIB + ["--", "number::tests::huge_exponents_read_as_go_reads_them"],
    ),
    (
        "parse: Go's z / +Inf, zero",
        NUMBER,
        "            Err(_) if e5 < 0 => return Ok(Float::zero(prec, neg)),\n",
        "",
        LIB + ["--", "number::tests::huge_exponents_read_as_go_reads_them"],
    ),
    (
        "text: the bound on numbers",
        NUMBER,
        "        if !self.is_zero() && !(-MAX_EXPONENT..=MAX_EXPONENT).contains(&self.go_exp()) {\n"
        "            return Err(TOO_BIG);\n        }\n",
        "",
        LIB + ["--", "number::tests::texts_and_integers_end_at_opas_bound"],
    ),
    (
        "int: the bound on numbers",
        NUMBER,
        "        if self.go_exp() > MAX_EXPONENT {\n            return Err(TOO_BIG);\n        }\n",
        "",
        LIB + ["--", "number::tests::texts_and_integers_end_at_opas_bound"],
    ),
    (
        "bits.lsh: the bound on numbers, before the shift",
        NUMBERS,
        "        if i64::try_from(bits).map_or(true, |b| b > number::MAX_EXPONENT) {\n"
        "            return Err(num_err(number::TOO_BIG));\n        }\n",
        "",
        BUILTINS,
    ),
    (
        "units.parse: the bound on numbers",
        ENCODING,
        "    if r.mant_exp() > number::MAX_EXPONENT {\n"
        "        return Err(fail(number::TOO_BIG.0));\n    }\n",
        "",
        BUILTINS,
    ),
    (
        "parser: Go's ±Inf, too big",
        PARSER,
        "            Err(e) if e == number::OVERFLOW => true,\n",
        "",
        ["cargo", "test", "-p", "shards-rego", "--test", "parse"],
    ),
]


def run(cmd, log):
    start = time.monotonic()
    p = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, timeout=1800)
    out = p.stdout + p.stderr
    lines = [l for l in out.splitlines() if l.startswith(("test ", "test result", "thread "))]
    log.write(f"$ {' '.join(cmd)}\nexit {p.returncode} after {time.monotonic() - start:.1f} s\n")
    log.write("\n".join(lines[:12]) + "\n")
    return p.returncode


def main():
    out = ROOT / "docs/research/measurements/ci-rego-compare/logs/mutants.log"
    out.parent.mkdir(parents=True, exist_ok=True)
    rev = subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True).stdout.strip()
    failed = []
    with out.open("w") as log:
        log.write(f"mutants of the working tree on {rev}\n\n")
        for name, path, old, new, cmd in MUTANTS:
            f = ROOT / path
            text = f.read_text()
            if text.count(old) != 1:
                sys.exit(f"{name}: the text to remove is in {path} {text.count(old)} times")
            log.write(f"## {name} ({path})\n")
            f.write_text(text.replace(old, new))
            try:
                code = run(cmd, log)
            finally:
                f.write_text(text)
            verdict = "killed" if code != 0 else "SURVIVED"
            log.write(f"{verdict}\n\n")
            log.flush()
            if code == 0:
                failed.append(name)
        log.write("restored; the unmutated tests:\n")
        code = run(LIB + ["--", "number::tests", "value::tests"], log)
        code |= run(BUILTINS, log)
        log.write(f"{'green' if code == 0 else 'RED'}\n")
    print(out.read_text())
    if failed or code:
        sys.exit(f"survived: {failed}; unmutated green: {code == 0}")


if __name__ == "__main__":
    main()
