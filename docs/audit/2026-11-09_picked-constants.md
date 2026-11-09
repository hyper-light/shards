# Picked constants: audit of 2026-11-09

The user's rule: "why the arbitrary constants? We don't allow those. Learned and data-derived methods only."

Method: every `const` in `crates/*/src` (tests excluded) whose name marks a tunable (TIMEOUT, WAIT, PATIENCE, RETR, BACKOFF, INTERVAL, GRACE, SLACK, THRESHOLD, BURST, CAPACITY, BUDGET, LIMIT, MAX, MOST, LEAST, DEADLINE, PERIOD, POLL, STALL, TICK) or whose type is `Duration`: 350 found (`rg`). Of those, the 158 below carry no citation in the eight lines above them (no RFC, section, PM or D entry, source file, spec, kernel, upstream project, "measured", "derived" or "learned"). Each is either a spec's or an upstream's value whose citation must be written where it is, or a picked value to replace with one derived from the system's data or learned as shards runs.

Already replaced on 2026-11-09: vsock HANDSHAKE_TIMEOUT, MAX_STRAY_RSTS, MAX_HANDSHAKE, serial RX_CAPACITY (p/vmm-derived, 66652aaf); SAID, MAX_CONNECTIONS, MAX_SAVED by Hex's D. Also picked, found beside them: vsock QUEUE_SIZE (256), vm_run.rs forward_stdin's 256-byte buffer.

## crates/shards (49)

- `crates/shards/src/agentfile.rs:22: const AGENTFILE_MOST: u64 = 1 << 20;`
- `crates/shards/src/build/builder.rs:1369: pub(crate) const AGENT_MAX: usize = 256 * 1024;`
- `crates/shards/src/build/give.rs:41: const PATIENCE: Duration = Duration::from_secs(10);`
- `crates/shards/src/build/give.rs:44: const SAY_EVERY: Duration = Duration::from_secs(60);`
- `crates/shards/src/build/mod.rs:2063: const POLICY_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);`
- `crates/shards/src/build/mod.rs:3020: const MOST: u64 = 2 * 1024 * 1024;`
- `crates/shards/src/build/policy/provenance.rs:30: const MAX_MATERIAL_DEPTH: i64 = 24;`
- `crates/shards/src/build/policy/provenance.rs:32: const MAX_NESTING: usize = 10_000;`
- `crates/shards/src/build/policy_command.rs:444: const ASK_MOST: u64 = 1 << 20;`
- `crates/shards/src/build/proxy/ca.rs:22: const HOUR: Duration = Duration::from_secs(3600);`
- `crates/shards/src/build/proxy/ca.rs:23: const CA_LIFE: Duration = Duration::from_secs(10 * 365 * 24 * 3600);`
- `crates/shards/src/build/proxy/ca.rs:24: const LEAF_LIFE: Duration = Duration::from_secs(24 * 3600);`
- `crates/shards/src/build/proxy/head.rs:11: pub const MOST: usize = (1 << 20) + 4096;`
- `crates/shards/src/build/ssh.rs:88: const PACKET_MAX: usize = 256 * 1024;`
- `crates/shards/src/build/ssh.rs:92: const MAX_PACKET: u32 = 32 * 1024;`
- `crates/shards/src/cli/client.rs:57: const START_TIMEOUT: Duration = Duration::from_secs(30);`
- `crates/shards/src/cli/client.rs:1115: const STOP_TIMEOUT: Duration = Duration::from_secs(60);`
- `crates/shards/src/cli/show.rs:29: pub const TICK: Duration = Duration::from_millis(83);`
- `crates/shards/src/cli/terminal.rs:136: const PATIENCE: std::time::Duration = std::time::Duration::from_millis(500);`
- `crates/shards/src/cli/terminal.rs:371: const PATIENCE: std::time::Duration = std::time::Duration::from_millis(500);`
- `crates/shards/src/containers.rs:221: const MAX_RECORD: u64 = 8 << 20;`
- `crates/shards/src/daemon.rs:247: const STOP_GRACE: Duration = Duration::from_secs(10);`
- `crates/shards/src/daemon.rs:251: const RETRY: Duration = Duration::from_millis(250);`
- `crates/shards/src/daemon.rs:254: const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);`
- `crates/shards/src/daemon.rs:9470: const PATIENCE: Duration = Duration::from_secs(20);`
- `crates/shards/src/daemon/commands.rs:3606: const TOP_LIMIT: usize = 16 << 20;`
- `crates/shards/src/daemon/demand.rs:149: const KEEP: Duration = Duration::from_secs(600);`
- `crates/shards/src/daemon/engine.rs:58: const BODY_MOST: u64 = 8 * shards_ipc::MAX_PAYLOAD as u64;`
- `crates/shards/src/daemon/moves.rs:44: const MAX_PATH: usize = 4096;`
- `crates/shards/src/daemon/moves.rs:45: const MAX_PLAN: usize = 16 << 20;`
- `crates/shards/src/daemon/moves.rs:46: const MAX_XATTRS: usize = 1 << 16;`
- `crates/shards/src/daemon/moves.rs:47: const MAX_XATTR: usize = 64 << 10;`
- `crates/shards/src/daemon/parents.rs:284: pub(super) const EMPTIED: Duration = Duration::from_secs(10);`
- `crates/shards/src/daemon/reclaim.rs:52: const SAY_EVERY: Duration = Duration::from_secs(60);`
- `crates/shards/src/daemon/ship/awslogs.rs:29: const RENEW: Duration = Duration::from_secs(5 * 60);`
- `crates/shards/src/daemon/ship/fluentd.rs:48: const MOST_WAIT: Duration = Duration::from_secs(60);`
- `crates/shards/src/daemon/ship/fluentd.rs:50: const ACK_MOST: usize = 1024;`
- `crates/shards/src/daemon/ship/gcplogs.rs:31: const ENTRY_MOST: usize = 256 * 1024;`
- `crates/shards/src/daemon/ship/gcplogs.rs:33: const RENEW: Duration = Duration::from_secs(5 * 60);`
- `crates/shards/src/daemon/snapshots.rs:350: const MAX_RESUME: u64 = 4 << 10;`
- `crates/shards/src/daemon/top/specs.rs:10: pub(super) const UNLIMITED: u32 = 4;`
- `crates/shards/src/daemon/top/specs.rs:13: pub(super) const PIDMAX: u32 = 0x10;`
- `crates/shards/src/daemon/visit.rs:22: const START_LIMIT: Duration = Duration::from_secs(30);`
- `crates/shards/src/logging/gelf.rs:63: pub const TCP_MOST: usize = 2 * 1024 * 1024;`
- `crates/shards/src/oidc.rs:18: const BROWSER_WAIT: Duration = Duration::from_secs(120);`
- `crates/shards/src/pull.rs:74: const PROGRESS_EVERY: Duration = Duration::from_millis(100);`
- `crates/shards/src/run.rs:1156: const MOST: u64 = 64 << 10;`
- `crates/shards/src/synced.rs:165: const FRAME_MOST: usize = 64 << 10;`
- `crates/shards/src/synced.rs:297: const SECRET_MAX: u64 = shards_abi::run::MAX_PAYLOAD as u64;`

## crates/vmm (35)

- `crates/vmm/src/arch/aarch64/boot.rs:18: pub const FDT_MAX: u64 = SZ_2M;`
- `crates/vmm/src/arch/aarch64/state.rs:57: const MAX_VCPUS: usize = 1024;`
- `crates/vmm/src/arch/aarch64/state.rs:58: const MAX_SYS: usize = 512;`
- `crates/vmm/src/arch/aarch64/state.rs:59: const MAX_ICC: usize = 32;`
- `crates/vmm/src/arch/aarch64/state.rs:61: const MAX_GIC: usize = 16 << 20;`
- `crates/vmm/src/arch/aarch64/state.rs:62: const MAX_ID: usize = 64;`
- `crates/vmm/src/arch/x86_64/boot.rs:69: const MAX_PHDRS: u16 = 64;`
- `crates/vmm/src/arch/x86_64/cpuid.rs:31: const TSC_DEADLINE: u32 = 1 << 24;`
- `crates/vmm/src/devices/virtio/fs/host.rs:322: pub const PATH_MAX: c_int = 4096;`
- `crates/vmm/src/devices/virtio/fs/mod.rs:91: pub const MAX_FRAME: usize = proto::MAX_WRITE as usize + 4096;`
- `crates/vmm/src/devices/virtio/fs/proto.rs:51: pub const MAX_WRITE: u32 = MAX_PAGES as u32 * 4096;`
- `crates/vmm/src/devices/virtio/gpu/virgl.rs:494: const MAX_DEVICES: u32 = 64;`
- `crates/vmm/src/devices/virtio/iommu.rs:79: const READABLE_MAX: usize = 72;`
- `crates/vmm/src/devices/virtio/mmio.rs:22: pub(super) const QUEUE_NUM_MAX: u64 = 0x034;`
- `crates/vmm/src/devices/virtio/queue.rs:23: pub const MAX_QUEUE_SIZE: u16 = 32768;`
- `crates/vmm/src/hv/kvm/aarch64/gic.rs:56: const MAX_WRITES: usize = 1 << 16;`
- `crates/vmm/src/hv/kvm/aarch64/state.rs:27: const MAX_REGS: usize = 4096;`
- `crates/vmm/src/hv/kvm/state.rs:42: const MAX_BLOB: usize = 64 << 10;`
- `crates/vmm/src/hv/kvm/state.rs:43: const MAX_MSRS: usize = 4096;`
- `crates/vmm/src/hv/kvm/sys.rs:109: pub const CAP_MAX_VCPUS: u64 = 66;`
- `crates/vmm/src/hv/kvm/sys/aarch64.rs:437: const MAX_REGS: usize = 4096;`
- `crates/vmm/src/hv/kvm/sys/x86_64.rs:144: pub const CAP_TSC_DEADLINE_TIMER: u64 = 72;`
- `crates/vmm/src/hv/kvm/sys/x86_64.rs:262: pub const MAX_CPUID_ENTRIES: usize = 256;`
- `crates/vmm/src/hv/kvm/sys/x86_64.rs:334: const MAX: usize = 1024;`
- `crates/vmm/src/hv/whp/state.rs:153: const MAX_BLOB: usize = 64 << 10;`
- `crates/vmm/src/hv/whp/state.rs:154: const MAX_REGISTERS: usize = 256;`
- `crates/vmm/src/hv/whp/state.rs:155: const MAX_LEAVES: usize = 64;`
- `crates/vmm/src/hv/whp/state.rs:156: const MAX_VCPUS: usize = 254;`
- `crates/vmm/src/snapshot/mod.rs:121: const MAX_PLUGGED_RUNS: usize = 1 << 18;`
- `crates/vmm/src/snapshot/mod.rs:124: const MAX_DATA_RUNS: usize = 1 << 18;`
- `crates/vmm/src/snapshot/mod.rs:125: const MAX_PATH: usize = 4096;`
- `crates/vmm/src/snapshot/mod.rs:126: const MAX_NAME: usize = 64;`
- `crates/vmm/src/vm/hold.rs:21: pub(super) const PERIOD: Duration = Duration::from_millis(100);`
- `crates/vmm/src/vm/hold.rs:24: const BUSY: Duration = Duration::from_millis(10);`
- `crates/vmm/src/vm/live.rs:212: pub const MAX_RECORD: usize = 1 << 20;`

## crates/rego (9)

- `crates/rego/src/funcs/encoding/gojson.rs:63: const MAX_NESTING_DEPTH: usize = 10000;`
- `crates/rego/src/funcs/encoding/yaml/scanner.rs:135: const MAX_FLOW_LEVEL: usize = 10000;`
- `crates/rego/src/funcs/encoding/yaml/scanner.rs:136: const MAX_INDENTS: usize = 10000;`
- `crates/rego/src/funcs/encoding/yaml/scanner.rs:137: const MAX_NUMBER_LENGTH: i8 = 2;`
- `crates/rego/src/funcs/regex.rs:63: const CACHE_MAX: usize = 100;`
- `crates/rego/src/funcs/regex/syntax.rs:26: const MAX_HEIGHT: usize = 1000;`
- `crates/rego/src/funcs/regex/syntax.rs:27: const MAX_SIZE: i64 = (128 << 20) / 40;`
- `crates/rego/src/funcs/regex/syntax.rs:28: const MAX_RUNE: u32 = 0x10FFFF;`
- `crates/rego/src/parser.rs:127: const MAX_DEPTH: usize = 100_000;`

## crates/flate (6)

- `crates/flate/src/bits.rs:17: pub const MAX_STORE_BLOCK_SIZE: usize = 65535;`
- `crates/flate/src/deflate.rs:16: const MAX_MATCH_LENGTH: i64 = 258;`
- `crates/flate/src/deflate.rs:18: const MAX_FLATE_BLOCK_TOKENS: usize = 1 << 14;`
- `crates/flate/src/deflate.rs:22: const MAX_HASH_OFFSET: i64 = 1 << 24;`
- `crates/flate/src/fast.rs:14: const MAX_MATCH_OFFSET: i32 = 1 << 15;`
- `crates/flate/src/fast.rs:15: const MAX_MATCH_LENGTH: i32 = 258;`

## crates/tuf (6)

- `crates/tuf/src/updater.rs:46: pub const MAX_ROOT_ROTATIONS: i64 = 256;`
- `crates/tuf/src/updater.rs:47: pub const MAX_DELEGATIONS: usize = 32;`
- `crates/tuf/src/updater.rs:48: pub const ROOT_MAX: u64 = 512_000;`
- `crates/tuf/src/updater.rs:49: pub const TIMESTAMP_MAX: u64 = 16_384;`
- `crates/tuf/src/updater.rs:50: pub const SNAPSHOT_MAX: u64 = 2_000_000;`
- `crates/tuf/src/updater.rs:51: pub const TARGETS_MAX: u64 = 5_000_000;`

## crates/build (5)

- `crates/build/src/copy.rs:24: const MAX_ROOT_LINKS: u32 = 255;`
- `crates/build/src/copy.rs:36: pub const STICKY: u32 = 1 << 20;`
- `crates/build/src/skill.rs:27: const MAX_NAME: usize = 64;`
- `crates/build/src/skill.rs:28: const MAX_DESCRIPTION: usize = 1024;`
- `crates/build/src/skill.rs:29: const MAX_COMPATIBILITY: usize = 500;`

## crates/ext4 (5)

- `crates/ext4/src/lib.rs:62: const LEAST_BLOCKS: u64 = 2048;`
- `crates/ext4/src/lib.rs:64: const MOST_BLOCKS: u64 = 1 << 32;`
- `crates/ext4/src/lib.rs:821: const MAX_LEN: u64 = 32768;`
- `crates/ext4/src/read.rs:166: const MAX_DEPTH: u16 = 5;`
- `crates/ext4/src/read.rs:168: const INIT_MAX_LEN: u16 = 32768;`

## crates/registry (5)

- `crates/registry/src/auth.rs:46: const MAX_TOKEN_RESPONSE: u64 = 1 << 20;`
- `crates/registry/src/http.rs:66: const MAX_TRAILER: usize = 4096;`
- `crates/registry/src/http.rs:67: const MAX_REDIRECTS: usize = 10;`
- `crates/registry/src/registry.rs:38: const THROTTLED_MOST: Duration = Duration::from_millis(6400);`
- `crates/registry/src/registry.rs:3183: const SLACK: Duration = Duration::from_millis(1500);`

## crates/sigstore (5)

- `crates/sigstore/src/bundle.rs:41: pub const MAX_TLOG_ENTRIES: usize = 32;`
- `crates/sigstore/src/proto.rs:778: const RECURSION_LIMIT: usize = 10_000;`
- `crates/sigstore/src/tlog/gojson.rs:153: const MAX_DEPTH: usize = 10_000;`
- `crates/sigstore/src/tlog/note.rs:180: const MAX: usize = 64 * 1024;`
- `crates/sigstore/src/x509.rs:1856: const MAX_SIGNATURE_CHECKS: usize = 100;`

## crates/zstd (5)

- `crates/zstd/src/enc.rs:12: pub const MAX_COMPRESSED_BLOCK_SIZE: usize = 128 << 10;`
- `crates/zstd/src/enc.rs:15: const MAX_MATCH_LENGTH: i32 = 131_074;`
- `crates/zstd/src/huff.rs:16: const MAX_SYMBOL_VALUE: usize = 255;`
- `crates/zstd/src/huff.rs:17: const TABLE_LOG_MAX: u8 = 11;`
- `crates/zstd/src/huff.rs:136: const FSE_MAX_TABLE_LOG: u8 = 12;`

## crates/init (4)

- `crates/init/src/devices.rs:978: const FUNCTION_WITHIN: std::time::Duration = std::time::Duration::from_secs(10);`
- `crates/init/src/hotplug.rs:39: const FOUND_WITHIN: Duration = Duration::from_secs(10);`
- `crates/init/src/inroot.rs:200: const MAX_LINKS: usize = 255;`
- `crates/init/src/join.rs:1210: const MAX_FDS: usize = 2;`

## crates/ipc (4)

- `crates/ipc/src/lib.rs:2019: pub const MAX_PAYLOAD: usize = 1 << 20;`
- `crates/ipc/src/lib.rs:2066: pub const MAX_FDS: usize = 9;`
- `crates/ipc/src/slots.rs:84: pub const MAX: usize = 8;`
- `crates/ipc/src/slots/named.rs:43: const LOOK_AGAIN: Duration = Duration::from_millis(250);`

## crates/git (3)

- `crates/git/src/checkout.rs:37: const MAX_DEPTH: usize = 4096;`
- `crates/git/src/pktline.rs:8: pub const MAX: usize = 65520;`
- `crates/git/src/pktline.rs:32: pub const DELIMITER: &[u8] = b"0001";`

## crates/server (3)

- `crates/server/src/sandbox.rs:44: pub const FRAME_MAX: usize = 1 << 20;`
- `crates/server/src/server.rs:59: const HEAD_MAX: usize = 16 * 1024;`
- `crates/server/src/server.rs:60: const BODY_MAX: usize = 1 << 20;`

## crates/remote (2)

- `crates/remote/src/invite.rs:31: pub const JOIN_MAX: usize = NAME_MAX + 1 + 32;`
- `crates/remote/src/server.rs:42: const MADE_MOST: u64 = 4 << 10;`

## crates/testguest (2)

- `crates/testguest/src/linux.rs:1229: const STOOD_STILL: Duration = Duration::from_secs(15);`
- `crates/testguest/src/linux.rs:1428: const STREAM_QUIET: Duration = Duration::from_secs(60);`

## crates/abi (1)

- `crates/abi/src/run.rs:31: pub const MAX_PAYLOAD: u32 = 1 << 20;`

## crates/agentfile (1)

- `crates/agentfile/src/run_options.rs:73: const MAX: i64 = 8192;`

## crates/api (1)

- `crates/api/src/client.rs:42: const STOP_GRACE: Duration = Duration::from_secs(10);`

## crates/archive (1)

- `crates/archive/src/root.rs:141: const MAX_SYMLINKS: usize = 8;`

## crates/erofs (1)

- `crates/erofs/src/lib.rs:60: const NAME_MAX: usize = 255;`

## crates/gateway (1)

- `crates/gateway/src/grpc.rs:12: pub const MOST: usize = 16 << 20;`

## crates/net (1)

- `crates/net/src/bridge.rs:588: const RTAX_MAX: u32 = 8;`

## crates/seccomp (1)

- `crates/seccomp/src/compiler.rs:52: const MAX_ERRNO: u32 = 4095;`

## crates/testdir (1)

- `crates/testdir/src/bin/shards-testhelper.rs:23: const AWAIT_MOST: Duration = Duration::from_secs(300);`

## crates/tui (1)

- `crates/tui/src/mark.rs:40: const TICK: u8 = 2;`
