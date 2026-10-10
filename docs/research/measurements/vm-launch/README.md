# VM process launches (PM M166)

How long a VM process takes from its spawn to its first request for access, and what it
waits for. macOS only: Linux has no admission of a new executable.

- `launch.py VM KERNEL N W OUT [THRESHOLD [FRESH|herd]]`: N launches of `VM run --kernel
  KERNEL --grants 3`, W at once, each timed to its first request on descriptor 3 and
  ended. A launch slower than THRESHOLD seconds is sampled (sample(1)) while it waits.
  FRESH `1` launches a new copy of VM each time, and each copy again; `herd` launches one
  new copy by all W at once, N rounds.
- `stuck.py VM KERNEL OUT BACKLOG`: a new copy launched while BACKLOG others are being
  assessed, its task info read every 20 ms until it asks.

KERNEL need only be a file: the measurement ends at the first request. VM is the signed
`shards-vm` a build carries (`target/release/build/shards-*/out/helpers/shards-vm`).
Run with `python3 -I`.
