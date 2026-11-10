# Corrected shared release caption: current-source after

Corrected tested source: fabd3b33886eba33adbf58f18a5d0ec208029e27 on the
assigned28e base. Against the previous tested0691 source, the ONLY code delta is
tenant_exit.rs shared release state string:
OwnerReleasedNeighborAfterBothMember1Eofs → OwnerReleasedNeighborAfterGateEof.
All caller-specific phase/End/metadata assertions, owned native body, production,
observer/parser, queues and wrappers remain unchanged.

The known old defect was mistakenly left in the current adapter. ROOT caught it
again in0691/396d: actual A phase records remain Open68/62 while release.json
claims both captured channels ended. Current-before raw-v1 and raw-minimum-before,
all original release/phase/End records and frozen0691 binaries remain intact.
No raw record was corrected retroactively. This is the same reproduced caption
defect, not a new inferred production failure.

After source-freeze-v3, actual r43 built the exact corrected host/native source0
and froze all seven artifacts. Actual r44 ran host Clippy0 and native musl guest
Clippy0 with -Dwarnings, then NEW normal0+A0; each exactly1passed/0failed/0ignored/
23filtered. Both current release records read OwnerReleasedNeighborAfterGateEof
and gate_peer_eof:true. Both strict tests, protocol completion, stopped PID0,
private removal, owner/observer joins and FD11→11 passed. No timeout occurred.

Normal final actor metadata is ended/complete102/99. A retains actor Open68/62
through causally new sibling fd1/fd2 and main progress, then fresh child reply,
exact suffix118/109 and canonical End/ACK/transport EOF at wire306/297. Its final
actor metadata is ended/complete306/297; continuing sibling in both cases stays
aborted/unknown177/171 after stop. Gate EOF now describes only the observed gate
connection. It makes no claim that both captured streams have already ended.

Private Fedora8fadc1ec213332e16cb0d915a3fb913f scope and admitted2CPU/4GiB/24GiB
are unchanged; Cargo jobs4, each original work240s+cleanup15s and outer255s.
This authorized correction/revalidation did not rebase onto new maina920 or
touch ROOT sources/targets/caches. Independent parent/child status, full population
reaping and kernel/parent resource return remain Unknown. Existing R01–R16 limits
and coordinator integration/other-platform/profile/audit obligations in RESULTS.md
remain applicable. No fixture-validity result becomes an ownership-guard proof.

[After transfer](raw-caption-after/TRANSFER.json) verifies27,989-byte archive,
SHA2569375df887529faa023d5839246df0cee3545bb8708e9dd4e266b08e1275af453,
three <=12,000-byte chunks,105 safe regular members and all104 original sizes/
hashes locally. Logs and commit transcripts are losslessly compressed with
STORAGE.json original/stored mappings, no whitespace waiver/normalization.
The full compile JSONL remains remotely with its retained size/hash. All before,
after, source and frozen data are preserved. Current-before archives remain
57,415bytes/230 originals plus13,624bytes/65 originals, linked in RESULTS.md.

Full managed-runtime P0–P13/G25 and this continuing lane remain open for ROOT.

After corrected checkpoint629060747, actual r49 continued the permitted minimum
positive challenge1/report239 input on the SAME frozen fabd source: exactly1
passed/0failed/0ignored/23filtered. It retained the exact6-byte child request/
reply, corrected gate-EOF caption, strict suffix/End/final metadata and all
cleanup/FD11→11. No source rebuild or rebase occurred. Its separate
[transfer](raw-minimum-caption/TRANSFER.json) verifies13,657-byte archive
SHAf9841b1d34dffe18802aa8078e9143883bb535167e932056cb935827e8fcd9a8,
66 safe regular members/all65 original sizes and hashes, two bounded chunks.
