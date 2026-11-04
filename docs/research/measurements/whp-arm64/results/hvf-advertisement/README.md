# HVF EL2 advertisement A/B: cause unresolved

This preserves the original, unmodified-advertisement run (A) and the locally corrected
PFR0 run (B). It does not establish why Windows' hypervisor failed under HVF, and does
not close Step 10. The later TCG/TF-A host is a separate configuration.

| Arm | Raw Windows boot time | First event from that boot | WHP present | CreatePartition |
|---|---|---|---|---|
| A: default advertisement, nested EL2 enabled | 2026-11-02 07:58:03 | 43: EL2 not present | 0 | 0xC0351000 |
| B: PFR0.EL2 set to 1 when nested EL2 is enabled | 2026-11-02 08:23:21 | 26: BalStatus 0x19, sub-status 0xC | 0 | 0xC0351000 |

Both boots have HypervisorPlatform, VirtualMachinePlatform and Microsoft-Hyper-V-All
enabled and hypervisorlaunchtype Auto. Both return WHP Features 0xad9. Both say
HyperVisorPresent=True through Get-ComputerInfo, which does not prove WHP availability.

The raw Windows transcripts append earlier boots. B therefore still contains older
event43 records; its current boot's event26 must not be replaced with those older
records. The guest's RTC was interpreted eight hours ahead at this stage. These raw
Windows times and the Mac launch UTC timestamps remain separate, without rewriting
either clock or transcript.

QEMU's correction base is v11.1.3, commit
`005c61c0ef6f6ae615283440b4bb7be51b2c6ddd`. The exact two-line correction is
`qemu-hvf-el2.patch`, byte-identical to the retained experiment patch and the fixture's
existing patch. `experiment.json` records its SHA-256 and verifies the old and corrected
HVF source Git blobs against the patch header. It changes PFR0.EL2 only when nested EL2
is enabled; it does not advertise VHE or EL3.

`A/launch.txt` records NESTED=1, CPUS=4 and MEM=8G. The retained original launch script
uses `-M virt,gic-version=3,highmem=on,virtualization=on -accel hvf -cpu host -smp 4 -m 8G`,
with code.fd, vars.fd and win.qcow2. `B/launch.txt` records those environment values plus
ACCEL=hvf, CPU=host, the local QEMU binary path, VM_DISK=win-el2-id.qcow2 and
VARS=vars-el2-id.fd; its binary reported v11.1.3-dirty. The complete launch environments
and script program arguments are retained verbatim.

The expanded QEMU process argument list and historical executable hashes were not
captured. B's historical start-script bytes were not retained separately. Its exact
recorded settings are preserved; a reconstruction from today's script must not be
presented as the observed full command line. Current binaries include later fixture
work and are not substitutes for historical binary hashes. These provenance gaps stay
explicit until a separately labelled rerun can capture them; no rerun interrupted the
active real-VM suite to produce this bundle.

M422's independent framework probe (`framework-config.txt`) measures default PFR0.EL2=0
with nested EL2 enabled, a successful set/readback to1, VH=0 and E2H reading back0. This
advertisement finding prevents assigning Windows' failure to absent VHE alone. The
corrected Windows boot still fails, with an internal loader status rather than a
causally isolated VHE test. The HVF host result and VHE cause remain unresolved.

`A/windows.txt` and `B/windows.txt` preserve the complete selected captures as display
views. `transcript-format.json` records original-capture and display hashes; original
bytes remain in the local kit. Only line endings, BOM and trailing display padding were
normalized. No Windows code or feature check was modified. Neither
arm ran a WHP partition, so neither proves real-VM feature parity or a general host
blocker.
