# Operation pool: bounded native operation records (ROOT piece 2)

ROOT coordinator, POLICY_READ main bff3015182ad754274f1048ce393ac12cb6cd91f, rules df2acf47….
Implements the owner-side slot and fence bookkeeping of
[the bounded records decision](../../../managed-native-bounded-records-decision-v1.md)
(branch p/managed-native-records-v2, 3a754b839). Consumer: the runtime lane's native Owner,
replacing its unbounded `jobs: Vec` (ROOT audit finding F5). Not yet consumed; no wire,
issuer or helper change here.

`crates/ipc/src/operation_pool.rs` (sha256 2939be3a…, Linux only, beside `locked_region`):
`Pool<T>::acquire(population)` takes exactly `population` slots of `Entry<T>` from one locked
region (checked size arithmetic; refuses an unrepresentable population before mapping; any
cleanup failure is returned as the region's `Retained`) and initializes every slot Free.
`prepare(id, record)` refuses `Duplicate` at or below the fence or for an ID holding a slot,
`ResourcesUnavailable` when every slot is Live or Retired above the fence, and hands the
record back. `retire` moves the record out to its owner and keeps the ID; `fence(through)` is
monotone (`State` below), frees Retired slots at or below it and never a Live one;
`release` refuses while any record is Live. Drop drops Live records in the acquiring process
only. No allocation after acquisition; lookups are a linear scan of the population
(not measured; an index is a later measured choice, R14).

Runner 451397, Linux 7.2.9-2-cachyos x86_64, rustc 1.98.1, synced worktree, -j2:
- `cargo test -p shards-ipc --lib --target x86_64-unknown-linux-gnu operation_pool`: 8 passed
  (tests sha256 502a7a52…), including 1,000 churn rounds around a stuck operation holding
  (1 Live, 0 Retired, 3 Free) every round.
- `cargo fmt -p shards-ipc --check` 0; `cargo clippy --all-targets -- -D warnings` 0 for
  x86_64-unknown-linux-gnu, x86_64-unknown-linux-musl and aarch64-unknown-linux-musl.
- Mutation: deleting the at-or-below-fence `Duplicate` refusal fails
  `retired_slots_refuse_duplicates_until_a_fence_reuses_them` ("a fenced ID was admitted
  again"); restored hash 2939be3a… passes 8/8 again.

R09: this is new functionality; the defect it addresses (the Owner retaining every Job)
must still be shown red in the runtime's Owner when the runtime consumes the pool.
Open: Fence/Fenced codec and helper accounting (native lane), issuer fence (audit lane,
user-held), guest coverage report and daemon admission (ROOT), VM and musl-guest runs.
