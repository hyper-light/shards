//! Power events that devices raise: on x86 the guest powers off and resets through
//! device writes (ACPI sleep control, the i8042 reset command), not through a vCPU exit
//! (docs/research/kvm-x86_64-ground-truth.md §4).

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerEvent {
    Off,
    Reset,
}

pub type PowerHook = Box<dyn Fn(PowerEvent) + Send + Sync>;

/// Where devices send power events; the VM runtime installs the hook.
#[derive(Default)]
pub struct Power {
    hook: OnceLock<PowerHook>,
}

impl std::fmt::Debug for Power {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Power").finish_non_exhaustive()
    }
}

impl Power {
    pub fn on_event(&self, hook: PowerHook) {
        let _ = self.hook.set(hook);
    }

    pub fn request(&self, event: PowerEvent) {
        match self.hook.get() {
            Some(hook) => hook(event),
            None => crate::warn!("guest power event {event:?} before the VM started"),
        }
    }
}
