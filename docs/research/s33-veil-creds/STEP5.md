# Step 33, step 5: kiln's broker over kiln-seams' Secrets (CP-T8.3; D242)

Kiln goal #70 (8f474b45c7cd86559e0566f22f9d0c33), plan step 33 of kiln plan 01a1cd8e862e7169a074db3d5fa33f3d; step 5 of
the goal's plan 01a1d2b88be57304b972761e2a5c6781 (research 01a1d2b88be57304bf6b081fe46832e0: f35, f47, f53, f64, f72,
f73, f74). Policy: kiln origin/main eb91cc8f8, AGENTS.md sha256 16b05ef8…, CLAUDE.md 336cc4fb…, docs/ci.md 31977fb1…
(§0 read), docs/seams.md 3e9173f5…; role implementer (the goal's agent). Code in github.com/hyper-light/kiln.

## What landed

| Commit (kiln) | What |
|---|---|
| 9f11df492 | `KilnSecrets` (`crates/kiln/src/secrets/seam.rs`, migration `20261113-0600_cp_t8_seam_secrets`): kiln-seams' `Secrets` in kiln's store, each value sealed AES-256-GCM under the secrets service's data keys, bound to its name and version id; kiln-seams' `check_name`, `check_value` public |
| 5468e3329 | The run-lease broker (`crates/kiln/src/secrets/broker.rs`): leases to a run's key, `release`, `moved` (KeyReplaced), `capture`, single-flight `refresh` |
| 6b5e18495 | Its writes in kiln's own savepoints (`store::tx`): a store call already runs in one |
| 9974a48c9 | docs/seams.md: kiln's store an implementation of Secrets, passing every clause; the broker its user |
| 752ca8bbc | kiln main at the landing (a merge of kiln main 36f46a0c2) |
| 7d35c14bc, landed at 13991fea1 | ROOT's points (1) and (2): the broker's ledger kept in kiln's store (migration `20261113-1400_cp_t8_broker_ledger`), an ended run's key refused (`RunEnded`), `Broker::open` revoking what a restart left; a run's lease ending at its value's own end or its run's, never by the seam's `LEASE_MAX` (kiln-seams' own bound, its doc says so) |

## Tests: one host, adacachy2 (Linux x86_64), rust 1.99.1

kiln builds on the newest stable, as its CI image (`rust:1-trixie`) does; adacachy2's default 1.98.1 is refused by
kiln's dependencies. The carrier (`.claude/worktrees/s33-carrier` in Shards: `kiln.sh`, `broker.sh`,
`broker-mut.sh`, `broker-mut2.sh`) clones kiln blobless on the runner, checks out the bundled branch or kiln main
as landed, and runs at nice 10 with a target of its own.

| Run | Tree | Result |
|---|---|---|
| r418 | 6b5e18495 (first green) | kiln-seams 47/47 + doc_truth 3/3; kiln `secrets::` 10/10; clippy `-p kiln-seams -p kiln --all-targets -D warnings` clean |
| r477, r545, r610, r615 | the branch merged with kiln main 88668560b, 1100df202, b47e08adf, 8ed3ae450 | green each: kiln-seams 54/54 + 3/3, `secrets::` 10/10, clippy clean |
| r731 | kiln main 752ca8bbc as landed | green as above; each guard removed in turn red (below) |
| r850 | 7d35c14bc (the ledger kept) | green: kiln-seams 54/54 + 3/3, `secrets::` 11/11 (the restart test among them), clippy clean; then the guard removals G1-G3 below red before the run was ended by the session's restart |
| r15 | kiln main 13991fea1 as landed (7d35c14bc merged with kiln main 8d36e0d35) | the focused set and every guard removal below, on the landed tree |

The landing's last merge (kiln main 36f46a0c2, which touched none of these files) was pushed on ROOT's word ("merge
if it moved, then push") after r615; r731 then ran the whole set on the landed tree.

### Guards removed by hand, each failing its test (R10)

At 752ca8bbc (r731), in the runner's clone, restored after each:

| Guard removed | Its test | Result |
|---|---|---|
| G1 single flight: a refresh asked meanwhile calls its provider again | `cp_t8_3_a_refresh_is_single_flight` | red |
| G2 an ask under a replaced key leased (both checks) | `cp_t8_3_a_move_revokes_the_old_keys_leases_and_refuses_it_typed` | red |
| G3 a run's end revokes nothing | `cp_t8_3_a_runs_leases_end_with_it` | red |
| G4 a ciphertext not bound to its secret's name | `cp_t8_3_values_are_sealed_at_rest` | red |

At 7d35c14bc and on the landed 13991fea1 (r850, r15), the ledger's:

| Guard removed | Its test | Result |
|---|---|---|
| G1 single flight | `cp_t8_3_a_refresh_is_single_flight` | red (r850) |
| G2 an ask under a replaced key leased | `cp_t8_3_a_move_revokes_the_old_keys_leases_and_refuses_it_typed` | red (r850) |
| G3 an ask under an ended run's key leased | `cp_t8_3_a_runs_leases_end_with_it` | red (r850) |
| G4 a run's end revokes nothing | `cp_t8_3_a_runs_leases_end_with_it` | r15 (r850's pattern did not apply; fixed) |
| G5 opening revokes nothing a restart left | `cp_t8_3_a_restart_keeps_the_ledger` | r15 |
| G6 a ciphertext not bound to its secret's name | `cp_t8_3_values_are_sealed_at_rest` | r15 |

## Owed

- Hex answering a run's lease asks from the broker, telling it of a move, and giving `ImageSpec::lessor` whenever
  it sets credentials (step 6); kiln's API or link that Hex reaches the broker by.
- ROOT: CI on kiln main; the real runs of step 10.
