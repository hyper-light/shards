# Coordinator caller build and lint records

Root r472 compiled the full actual `managed_output` integration caller at
`cfc3f2b17a0397f044d93ec5cd3b77ffc7886599` with normal real carried helpers:

```sh
cargo test --locked -j4 --profile test-release -p shards \
  --test managed_output --no-run
```

The build passed. The executable SHA, compiler, source hashes, command, original
exit and log are retained under `compile`. This is a compile result, not a real
VM, resource sample or performance result. No fake init/server was supplied.

Root r481's matrix attempt stopped at its clean-check before fetch or lint:
seven untracked macOS `.DS_Store` metadata files existed in the private clone.
Their bytes were moved into the original runner record and preserved; names,
lengths and hashes are retained. They were not deleted or included as source.
No lint ran in that attempt.

Root r485 then ran the full `scripts/lint` entrypoint at exact combined
`a5b428551c80ebcc0ee268f0ca22bf8359a729b4`. It failed with exit101 on four
`clippy::expect_used` errors in the new test fixture. The original log and exit
remain under `matrix-red`; this is not a passing seven-target gate.
The focused source fix `ed403164fa1965b1af913a289cae94bb73a9a9c9` preserves
the three checked-representability assertions and the checked metadata failure
message using the surrounding test style. Production code and lint settings are
unchanged. Actual closing checks are owed separately.

Each transfer directory contains its own manifest and verified archive identity.
Ten regular records were verified in each transfer. Native metadata files remain
on the runner; executable bytes were excluded. Root retains the original archives.
Full policy receipt is f5cec365 / rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Root owns all audit, actual VM, resource, performance and final matrix work.
