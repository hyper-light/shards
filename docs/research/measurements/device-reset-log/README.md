# A guest failing a device after every reset (audit V07)

Evidence for D8's "said once" (docs/design/architecture.md); results are
platform-measurements.md M131.

`src/main.rs`, built against a checkout's `crates/vmm`:

    cargo build --release --manifest-path docs/research/measurements/device-reset-log/Cargo.toml
    target/release/device-reset-log --seconds 5

A virtio-blk device behind its MMIO transport, driven through its registers as a driver
drives them, its queue's available index 100 ahead of a ring of 8 (AvailIndexJump). Each
cycle writes STATUS 0, sets the device up, writes DRIVER_OK and waits for
DEVICE_NEEDS_RESET. It prints the cycles, the seconds, and the bytes and lines the
process's log (sent to a file) gained. No vCPU runs: a guest's register writes would each
add an exit.
