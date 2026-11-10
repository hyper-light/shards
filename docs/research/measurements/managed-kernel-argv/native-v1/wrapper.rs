mod arch { pub mod x86_64 { pub mod layout {
    pub const CMDLINE: u64 = 0x2_0000;
    pub const CMDLINE_MAX: usize = 2048;
} pub mod boot { #[derive(Debug)] pub enum BootError {
    DoesNotFit { what: &'static str, at: u64, len: u64 },
} } } }
#[path = "/Users/adalundhe/Projects/shards/.claude/worktrees/managed-kernel-argv/crates/vmm/src/vm/cmdline.rs"] mod cmdline;
