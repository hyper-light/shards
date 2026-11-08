# Arm64 device-assignment probe: native result pending

The arm64 WHP module exports `device_assignment_refused() -> Option<String>` for the
passthrough branch's shared caller. It reads the host's Features capability and asks
`AllowDeviceAssignment` on a temporary, unconfigured partition. It creates no vCPU,
memory mapping or device. The partition's existing RAII wrapper deletes it afterward.
The property payload is a four-byte Windows BOOL, as on the x86_64 backend.

`Some` carries the capability or API refusal actually observed. `None` means WHP
accepted the property. Acceptance must expose the missing DDA implementation rather
than become an invented OS limit. The selected native test therefore fails on `None`.
Windows Server DDA implementation, hardware E2E and parity audit row 9 remain open.

The arm64 Windows VMM all-target Clippy check and static-CRT unit-test executable
build passed. `checked-source.patch`, `validation.json`, the command harness and raw
build/lint logs identify exactly those checks. The native selected test has not run:
the fixture's SSH observation channel is unavailable. These checks establish no
native refusal, assigned-device behavior, complete-suite result or real-VM pass.

Once the fixture is observable, copy the recorded executable to its own probe kit,
verify its hash there, and run:

```text
shards_vmm-2babb289ce1ece2e.exe --exact hv::whp::tests::device_assignment_requires_an_actual_host_refusal --nocapture --test-threads=1
```

Capture the Windows account, native stdout, stderr and exit. Keep this API-only probe
separate from the full suite's immutable executables. The coordinator's combined
validation and the full real-WHP suite remain required before Step 10 can close.
