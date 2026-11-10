# Same frozen source: D3 and legacy one-child continuation

These finite cases follow tested checkpoint487e8d0a2380b8536aa0a7a9cfd9e0e23524738d.
All Rust source remains EXACT cba6d1c32c5d27ef34b5cd9b49f95834253d6448 from b57;
there was no source rebuild/rebase or new runtime/helper layer. Policy/roles,
R01–R16 accounting and owed ROOT gates are [RESULTS.md](RESULTS.md) and README.

All seven mutable/frozen artifact hashes were independently verified AGAIN before
each pair (r66/D3 readback6858; r69/legacy readback6863), equal to the original
seven-artifact record6845. Native SHA256
a9266f916668f0c960e69aef3e32b6acd2ce55540a3cfdc178de80e1e01d0f15;
product kernel48a3824b7467dee7d82f36401fed99626e91c5dc5fefc432bc354a69ec616aa7.
Same private Fedora source/target/cache/frozen and /tmp/td-01a1-b57-v1, admitted
2CPU/4GiB/24GiB/j4. Work240000ms, cleanup15000ms/hard255s each; no enlargement.

| Pair | Caller inputs | Actual selected results |
| --- | --- | --- |
| r67 normal+D3 | D3/TASKS7=D+4, actor46643/sibling39821, challenge0f5bcc02372e4b2b8a5bf0aae6eab917, report269 | normal0(1.66s),D30(1.90s); each1passed,0failed,0ignored,35filtered |
| r70 normal+legacyA | depthNone/TASKS5, actor50915/sibling42623, challenge8abade50590343d7ae7d6651e69ca0ea, report269 | normal0(1.13s),A0(0.96s); each1passed,0failed,0ignored,35filtered |

These are functional invocation durations, not benchmarks. Together with r58,
six actual selected cases pass. No ignored/unrun test is credited as passing.

D3 ready top records the physical chain921->1706->1709->1712, all actor UID200001.
Asynchronous exec history retains remaining2/1/0 with the identical serialized
guest expiry240484749128ns and cleanup15000ms. Observed exit events are1712,
1709,1706,921 code0 leaf-up. This is correlation/observed history, not semantic
subagents, full population accounting or independent direct Child.wait receipts.
The legacy testcase supplies no depth argument and exercises the unchanged
one-child branch; no depth-mode absolute-expiry claim is inferred for that mode.

Every positive inherited case preserves actor Open68/62, causally subsequent new
sibling67/61 and main progress, exact21B challenge echo+actual peerEOF, then exact
suffix payload118/109 and canonical End/ACK/transportEOF wire306/297. Strict
final metadata remains actor ended/complete306/297, sibling aborted/unknown177/171.
Normal retains its separate102/99 actor endpoints. All GateEof captions and
caller-specific assertions stay unchanged. All four new selected cases complete
owner stop, observer cancel/join, inspectfalse0/private metadata removal and host
FD11->11. Per-edge exported wait/full-population/parent/kernel return stays Unknown.

All182 new original files are in the unchanged37,124B raw-v2.tar.gz, SHA256
4297e875d44c2ed1fa13345cad377e66b0a68c4a1ad5d037988beafc826d7d9e. All183 regular
members and182 originals were safely verified/extracted locally; nine lossless
gzip representations retain original whitespace/blank EOF, with every decoded
original reverified. The D2 archive and remote originals/frozen artifacts remain
unchanged. Exact commands, inputs, history, channel bytes and all cleanup remain
in raw-v2 and its manifest/receipt/verification files.

No active VM/build job remains at this checkpoint. The next guest-expiry case is
[proposal-only](NEXT-EXPIRY-PROPOSAL.md), receipt6862: ROOT must select finite T
and approve its shape before implementation/actors. While that dependency was
pending the independent legacy compatibility path above actually ran. No source
implements the proposal and no stronger ownership/guard/management closure is
claimed. ROOT retains review, integration/platform gates and lane-release authority.
