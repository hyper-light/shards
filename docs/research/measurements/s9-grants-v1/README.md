# Step 9: a member's destinations by name (D236), and whom a published port reaches (D231)

Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 9: explicit grants let a member reach
kiln's MCP and its providers by name, default deny, the same checks at build and at run;
and, from Hex step 15's finding, each port an orchestrator publishes names the member that
receives it. The decision and its alternatives are architecture.md D236 (and D231's
*Ports and grants*); this is the implementer's record. ROOT owns the real-VM E2E,
measurements and the merge gate's suites.

## Identity

| | |
|---|---|
| Policy | `origin/main` `451672250`; `docs/development-rules.md` sha256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`; role implementer ([POLICY_READ.json](POLICY_READ.json)) |
| Source | branch `p/s9-grants-v1` from `origin/main` `a7dbf1446`; origin/main `ad600dd52` merged in at `b5883e8e0` |
| Hosts | adacachy (Linux 7.2.9 CachyOS, x86_64, 32 CPUs); adas-mac-studio.local-2 (macOS, arm64, 32 CPUs); the Windows 11 guest on adacachy (`g121-win11`, x86_64, 8 vCPUs) |
| Toolchain | Rust 1.98.1 (`rust-toolchain.toml`), zig 0.16.0 (`scripts/install-zig`) |

## What changed

- **Destinations** (D236). `NETWORK --egress=HOST:PORT` beside ports: TCP; a host's name,
  an IPv4 address, or `host.docker.internal`, the host itself. One grammar for it and D192's
  network option (`shards_dns::destination`). The build and the daemon's reading of the
  normalized Agentfile refuse alike (`agentfile::connections`, `agentfile::held`); `reach`
  counts a destination as a reach past the microVM; the label
  `vnd.osi.agentfile.destinations` lists them. The network process names them beside the
  remote MCP servers and takes the host itself as the gateway at that TCP port alone, to
  the host's loopback there, never asking a resolver its name. Init gives each member its
  destinations as it gives remote MCP servers, names the host in its hosts file at eth0's
  gateway, and keeps the run's own processes and its engine's members off the gateway by TCP
  but its resolver's port. shards-api narrows them (`Narrowing.destinations`).
- **Whom a published port reaches** (D231). `Bound.to`: each member the image gives the
  port coming in, by declared index, at its own port; none for the run's own command.
  `read ports` and the run host share the rule (`agentfile::receiving`). shards-api's
  channel is version 3 for both.

## Reproduction (R09)

The gap is the bring-up's finding H2, reproduced live there on adacachy's KVM
(docs/research/measurements/harness-bringup-v1, `a_real_codex_starts_as_a_harness_and_reaches_its_provider`):
a harness granted `--egress=443` reached any address on 443, and with `--dns` any name. This
lane ran no microVM (an implementer's light scope); the live runs that show the fix are
owed to ROOT (below).

## Light tests

Each run the crates the change touches, their whole unit and integration tests (shards-dns,
shards-agentfile, shards-net, shards-api, shards-init on Linux), and the shards binary's
tests of the code it touches (`--bin shards -- api_run:: agentfile:: daemon::commands
networks::`). `SHARDS_HELPERS=skip`; on macOS and Windows empty placeholders stand for the
guest binaries `build.rs` embeds, which these tests never read.

| OS, arch | Revision | Result |
|---|---|---|
| Linux x86_64 (adacachy) | `b5883e8e0` (merged) | dns 5; agentfile 43, 36, 1, 3, 12; net 63; api 93, 16; init 200; shards binary 31: all pass ([records/linux-x86_64.txt](records/linux-x86_64.txt)) |
| Linux x86_64 (adacachy) | `6576aca74` | init 200, with the MCP form of the host; init's clippy `-D warnings` for the host, x86_64 and aarch64 musl; `cargo fmt --all --check` |
| macOS arm64 (Mac Studio) | `b5883e8e0` | dns 5; agentfile 43, 36, 1, 3, 12; net 61; shards binary 31: pass. api: 82 of 84, the two failing D215's and D233's, as at the base (below); its image tests 13 of 13 at `6576aca74` |
| Windows x86_64 (guest) | `b5883e8e0` | dns 5; agentfile 43, 36, 1, 3, 12; net 56; shards binary 30: pass. api: 66 of 66 one at a time; in parallel D233's report and D215's borrowed tests fail as at the base (below) ([records/windows-x86_64.txt](records/windows-x86_64.txt)) |
| scripts/lint, all seven targets (Mac Studio) | `b5883e8e0`, and `d5ccb7803` before the merge | format, then clippy `-D warnings` of the workspace for aarch64-apple-darwin, x86_64 and aarch64 Linux gnu and musl, x86_64 and aarch64 Windows: pass |

The same runs at `d5ccb7803`, before main was merged in, passed alike (init 198, the shards
binary 30 on Linux and macOS, 29 on Windows).

Linux arm64 has no host here for light tests; its lint passes, and CI's arm64 runners run
the suites on main.

## Guard-removal mutants (R10)

`s9-mutants.sh` (adacachy, `b5883e8e0`): each mutant removes one guard, the test that holds
it is run, and the source restored and touched.

| Mutant | Guard removed | Killed by |
|---|---|---|
| held-none | `agentfile::held` refuses nothing | `members_reaching_one_port_by_name_reach_it_alike` |
| boundary-ignored | a destination granted without the microVM's `EXPOSE` | `destinations_are_what_joined_networks_name_and_open` |
| let-in-destination | a destination on `--expose`/`--ingress` taken | `a_destination_is_a_tcp_host_reached_out` |
| localhost | `localhost` taken as a destination | shards-dns `a_destination_names_a_host_and_its_port` |
| host-any-port | the host itself at every port, not its granted one | shards-net `the_host_itself_is_its_one_granted_port` |
| host-asked | the host's name asked of a resolver | shards-net `the_host_itself_is_its_one_granted_port` |
| init-expose-members | init grants every member of the network, not those its `EXPOSE` names | init `destinations_reach_the_members_their_port_is_opened_for` |
| init-no-host-name | no hosts line for the host | init `destinations_reach_the_members_their_port_is_opened_for` |
| receivers-dropped | a published port names no receiver | shards `published_ports_name_whom_they_reach` |

Not covered by a light test: init's nf_tables rules that keep the run's processes
(`confine_eth0`) and its engine's members off the gateway, which only a guest kernel runs
(owed below).

## Found, not this change's

Reproduced at the base `a7dbf1446`, on files this change does not touch:

- **Windows, shards-api `physical::report` tests (D233)** fail when run in parallel
  (`ManagedIo { operation: "GPU report pipe", code: Some(231) }`, ERROR_PIPE_BUSY), in 2 of 3
  full runs at the base and 3 of 3 at the branch, each test passing alone: a named pipe
  shared between tests. At the merge `client::borrowed_tests::withheld_reply_expires_before_fixture_release`
  (D215's borrowed client, `UnexpectedEof`) failed beside them once; all 66 pass one at a time
  ([harness/s9-win-cmp.ps1](harness/s9-win-cmp.ps1)).
- **macOS, shards-api `client::tests::gpu_probe_protocol_requires_exact_completed_receipt`**
  (`GpuProbe(Pending)`, once even alone) and
  `client::borrowed_tests::contract::zero_patience_attempts_one_request_and_does_not_wait_for_accept_or_reply`:
  2 of 3 full runs at the base failed the first.

## R01–R16

| Rule | This change |
|---|---|
| R01 | No unwrap, expect, indexing or unchecked arithmetic in production code; clippy's panic lints pass on every target. Saturating port arithmetic in `receiving`; counts read by the wire's bounded `count`. |
| R02 | D236 records the alternatives (a directive of its own; names by SNI or Host; learned addresses per member in the guest, Cilium's toFQDNs, dnsmasq's nftset; the host at an address of its own, Docker Desktop's and Lima's) and why each was or was not taken, with kiln's MCP endpoint read from kiln's source. |
| R03 | Literals: `host.docker.internal` (Docker Desktop's documented name, under ICANN's reserved `.internal`), DNS's TCP port 53 (RFC 1035 §4.2.2), 63 and 253 (RFC 1035 §2.3.4), as D236 lists them. No new bound, wait or size. |
| R04 | No new lifecycle: grants are derived once from the normalized Agentfile; refusals are typed where they cross shards-api (`Cause::Widening`, `Cause::Image`). |
| R05 | No new interface but `Narrowing.destinations` and `Bound.to`, each D231's existing seam; no cache or wait added. |
| R06, R07, R13 | Focused functions (`destination_of`, `destinations_on`, `held`, `receiving`, `host_granted`); one parser shared by D192 and the Agentfile; one receiver rule shared by `read ports` and the run host; comments for the sources and the hold. |
| R08, R14 | No performance claim. Owed: the cost of a destination's flow and of `host.docker.internal`'s mapping (ROOT's). |
| R09 | H2, reproduced live by the bring-up; no live run here. |
| R10 | Unit and light tests above; nine guard-removal mutants, each killed. |
| R11 | Light tests on Linux x86_64, macOS arm64 and Windows x86_64; lint on all seven supported targets. Unverified: Linux arm64 tests (no host here), every real-VM run. |
| R12 | One grammar (`shards_dns`), shared by the build, the daemon (D192) and init; no platform-specific code. |
| R15 | Implementer scope; merged to main fast-forward under the 2026-11-12 rule; ROOT's E2E owed. |
| R16 | Work directories `~/g121-tmp/s9-*` on adacachy and the Mac Studio, `C:\g121\s9` on the Windows guest; nothing built on this Mac. |

## Owed to ROOT

- Real-VM E2E on KVM, HVF and WHP (D236's *Evidence owed*): a member granted provider names
  reaches them alone (not another host on their port, another port, nor an address no answer
  gave) and resolves them alone; a member granted `host.docker.internal:PORT` reaches a
  listener on the host's loopback at PORT and no other host port, while the run's own command
  and another member reach neither; an image `held` refuses fails its build, and its run
  when crafted. `api_image.rs`'s ingress test now checks `Bound.to`, and its refusal test a
  destination the image does not grant.
- The bring-up's examples (docs/examples/harness-codex, harness-claude) on destinations
  rather than `--dns --egress=443`, once run so.
- The two pre-existing flakes above.
