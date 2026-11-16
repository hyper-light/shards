# memory R03 lane: learned values for review findings memory-06..15

Implementer: Claude (takeover of the Codex memory lane), role implementer, policy at
main 97f404be8 (AGENTS.md f66d304c…, development-rules.md df2acf47…,
development-evidence.md 61613a24…). PM M581 records the method and results.

- `carrier-run.sh`: applies a patch over the base in a blobless clone kept on the runner
  (`~/g121-tmp/mem-r03`, a cold target there) and runs the focused commands given.
- `slice1-mutants.py`: puts each guard's old literal or rule back (the hundredfold look,
  lmkd's 140 ms, avg10, a 2 s repeat, re-tell and pass, no learned patience, a 4 s
  Saying window, a 1 ms read floor, no peak-period rule, a 1 ms grower pace, a give-up at
  the second window). Run from the tree with `python3 slice1-mutants.py`; each guard's test
  then fails, as `slice1.log` shows.
- `slice1.log`: the focused tests and clippy of slice 1 (memory-06, 07, 08, 10, 12, 13),
  green and mutated, on adas-mac-studio.local (macOS 26, arm64) and adacachy2 (CachyOS,
  x86_64).
