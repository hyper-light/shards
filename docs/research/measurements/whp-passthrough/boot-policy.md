# M401: IOMMU boot-policy experiment on jupiter

Status: approved and completed on 2026-11-03. Both reboots finished, the original
loader and boot manager match the saved baseline, and the temporary entry is gone.

M401 found that jupiter's default boot configuration refuses device assignment.
Microsoft documents `hypervisoriommupolicy enable` as selecting whether the
hypervisor uses an IOMMU. The approved experiment found that it does not change
this host's WHP answer: features remain `0x2FF`, `IommuSupport=0`, and setting
`AllowDeviceAssignment` returns `0x80070032` (`ERROR_NOT_SUPPORTED`). The reader
stayed attached throughout. Closing audit row 9 still requires
implementing Windows Server DDA and testing it on assignable Windows Server
hardware. Audit row 13 covers the separate Windows GPU confinement item.

## Recorded result

One experiment, Windows 11 Pro 26200.9711, with one WHP probe in each boot policy:

| Boot | LastBootUpTime (UTC) | IOMMU policy | WHP IommuSupport | AllowDeviceAssignment |
| --- | --- | --- | --- | --- |
| Original before experiment | 2026-11-01 07:20:46.3699320 | absent | 0 (M401's earlier probe) | 0x80070032 (earlier probe) |
| Temporary entry | 2026-11-03 21:39:40.5000000 | Enable | 0 | 0x80070032 |
| Restored original | 2026-11-03 21:45:34.5000000 | absent | 0 | 0x80070032 |

`boot-policy.ps1` records state before changing BCD and runs `Prepare`, `BootProbe
-Reboot`, `Inspect`, `Return -Reboot`, and `Verify` separately. All five actions
exited 0. Its native PowerShell parse and uploaded SHA-256 checks passed before
execution. The temporary entry was `{5db7ed20-c375-11f1-a54d-502e911a8b83}`;
the original default was never changed. After the second reboot, verification
deleted only that temporary entry and compared the complete original loader and
boot-manager listings with the baseline. A further `bcdedit /enum all /v` check
confirmed the temporary GUID absent. BitLocker protection and Secure Boot stayed
on, and the reader remained healthy.

Raw output and harness hashes: [jupiter-boot-policy.txt](jupiter-boot-policy.txt).
Machine-readable state: [jupiter-boot-policy.json](jupiter-boot-policy.json).
This measures an OS refusal; it is not a successful passthrough E2E.

The read-only 2026-11-02 preflight is in `jupiter-boot-preflight.txt`:

- Windows 11 Pro 26200, with no explicit `hypervisoriommupolicy` value.
- One normal boot entry; no one-time boot sequence is set.
- BitLocker protection is on, Secure Boot is enabled, and no FVE policy key exists.
- The SD reader reports Status OK. No Hyper-V VM was running; two build processes
  were active. These counts must be checked again before any reboot.
- The `kiln-agent` scheduled task is ready with boot and logon triggers.

## Scope and execution

Approval covers a temporary boot entry and two reboots of this shared runner.
Coordinate an idle window with the plan coordinator first. Have a console recovery
route available before changing boot configuration. Keep BitLocker and Secure Boot
enabled; if Windows rejects the setting, abort and remove the temporary entry.

1. Save verbose listings of the current entry and boot manager. Confirm the
   original default is still `{dffce7cc-f00e-11f0-9721-831b2d8db458}` and there is
   no existing one-time boot sequence. Record the state in the experiment's log.
2. Copy the current entry with a unique description (`shards DDA probe`), capture
   the new GUID, and verify it differs from the original entry. Set the IOMMU
   policy only on the copy. Remove the copy from the normal display order and
   select it through the one-time boot sequence. Check every command's exit code.

   ```powershell
   bcdedit.exe /copy "{current}" /d "shards DDA probe"
   # $probeId is the new GUID returned by /copy, validated and saved in the log.
   bcdedit.exe /set "$probeId" hypervisoriommupolicy enable
   bcdedit.exe /displayorder "$probeId" /remove
   bcdedit.exe /bootsequence "$probeId"
   Restart-Computer
   ```

3. After the runner reconnects, verify its boot time changed, the current entry is
   the copy, and the explicit IOMMU policy is enabled. Verify BitLocker protection
   and Secure Boot remain enabled. Run `probe.ps1` and capture its WHP features,
   AllowDeviceAssignment result, and Hyper-V I/O virtualization reasons.
4. If WHP accepts assignment, verify the reader is still empty and unused, then
   use `assign.ps1` with `PCIROOT(0)#PCI(0202)#PCI(0000)`. Its cleanup must remount
   and enable the reader. A permitting result provides a development host for the
   WHP implementation during the approved window. Windows Server hardware
   validation remains required for row 9. A refusal remains evidence
   of this configuration; the probe is not a successful shards hardware E2E.
5. Finish the window with a second reboot. The one-time sequence is consumed, so
   the original default entry is selected. Verify that entry and its original
   absent IOMMU policy, then remove only the recorded temporary entry:

   ```powershell
   Restart-Computer
   # After reconnecting and confirming the original entry is current:
   bcdedit.exe /bootsequence "$probeId" /remove
   bcdedit.exe /delete "$probeId" /cleanup
   ```

   Verify the boot manager matches the saved normal entry and display order, the
   reader is healthy, and BitLocker protection and Secure Boot remain enabled.
   If setup is aborted before the first reboot, remove the temporary GUID from
   the one-time sequence and delete that GUID without rebooting.

The original boot entry and boot manager's `default` value were preserved throughout.
The execution above supplies the result; the procedure remains here for reproduction
in a separately approved host window.

## Primary references

- [Microsoft: BCDEdit /set](https://learn.microsoft.com/en-us/windows-hardware/drivers/devtest/bcdedit--set)
  documents `hypervisoriommupolicy`.
- [Microsoft: adding boot entries](https://learn.microsoft.com/en-us/windows-hardware/drivers/devtest/adding-boot-entries)
  documents copying the current entry and the generated identifier.
- [Microsoft: BCDEdit /bootsequence](https://learn.microsoft.com/en-us/windows-hardware/drivers/devtest/bcdedit--bootsequence)
  documents one-time selection and removal of an identifier from that sequence.
- [Microsoft: BCD settings and BitLocker](https://learn.microsoft.com/en-us/windows/security/operating-system-security/data-protection/bitlocker/bcd-settings-and-bitlocker)
  lists `hypervisoriommupolicy` among settings ignored by default, rather than in
  the default validation profile. This does not establish this host's behavior
  under a changed boot configuration; verify its protection after each reboot.
