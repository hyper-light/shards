//! Targets without a hypervisor backend: every start fails with an explanation. The
//! handle types hold a private uninhabited field, so no value of them can exist.

use super::{Config, ExitReason, RestoreConfig};

fn no_backend() -> String {
    format!(
        "shards cannot run VMs on {}/{} yet: no hypervisor backend for this host",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

pub fn start(_cfg: &Config) -> Result<(Handle, Running), String> {
    Err(no_backend())
}

pub fn restore(_cfg: &RestoreConfig) -> Result<(Handle, Running), String> {
    Err(no_backend())
}

pub fn check_host() -> Result<(), String> {
    Err(no_backend())
}

pub fn max_vcpus() -> Result<u32, String> {
    Err(no_backend())
}

#[derive(Debug, Clone, Copy)]
enum Never {}

#[derive(Debug, Clone)]
pub struct Handle(Never);

impl Handle {
    pub fn markers(&self) -> Vec<(u32, u128)> {
        match self.0 {}
    }

    pub fn entered_at_us(&self) -> Option<u128> {
        match self.0 {}
    }

    pub fn exited_at_us(&self) -> Option<u128> {
        match self.0 {}
    }

    pub fn stop(&self) {
        match self.0 {}
    }

    pub fn console_input(&self, _bytes: &[u8]) {
        match self.0 {}
    }

    pub fn release(&self) {
        match self.0 {}
    }

    pub fn released_at_us(&self) -> Option<u128> {
        match self.0 {}
    }

    pub fn recording(&self) -> bool {
        match self.0 {}
    }

    pub fn save_working_set(&self) -> Result<usize, String> {
        match self.0 {}
    }

    pub fn prefetched(&self) -> usize {
        match self.0 {}
    }
}

#[derive(Debug)]
pub struct Running(Never);

impl Running {
    pub fn wait(self, _handle: Handle) -> ExitReason {
        match self.0 {}
    }
}
