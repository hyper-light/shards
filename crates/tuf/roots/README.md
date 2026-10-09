# Carried TUF roots

`sigstore/` is Sigstore's public-good TUF repository as moby/policy-helpers
v0.0.0-20260901104222-dd6c5499c491 carries it (`roots/tuf-root`, Apache License 2.0), the
version buildx v0.37.1 vendors: the root a fresh cache is seeded with, and the
timestamp, snapshot, targets and `trusted_root.json` it held then.
