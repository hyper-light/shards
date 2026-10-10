//! Whether this process is in App Sandbox: signed with its entitlement, which has macOS
//! confine it from its launch ("App Sandbox Entitlement", Apple). The VM process (D30) and
//! the network process (D31) each refuse to run outside it.

use crate::frameworks::{Owned, UTF8, api};

/// Whether this process's signature carries `com.apple.security.app-sandbox` as true, as
/// SecTask.h's `SecTaskCopyValueForEntitlement` reads it; an error where Security cannot be
/// asked.
pub fn sandboxed() -> Result<bool, String> {
    const KEY: &[u8] = b"com.apple.security.app-sandbox";
    let api = api()?;
    let len = isize::try_from(KEY.len()).map_err(|_| "an entitlement's name too long".to_string())?;
    // SAFETY: each object made here is owned, released once as it drops; the key's bytes
    // are a valid UTF-8 buffer of `len`.
    unsafe {
        let task = Owned::new(api, (api.SecTaskCreateFromSelf)(std::ptr::null()))
            .ok_or("this process's task: SecTaskCreateFromSelf made none")?;
        let key = Owned::new(
            api,
            (api.CFStringCreateWithBytes)(std::ptr::null(), KEY.as_ptr(), len, UTF8, 0),
        )
        .ok_or("an entitlement's name: CFStringCreateWithBytes made none")?;
        let value = Owned::new(
            api,
            (api.SecTaskCopyValueForEntitlement)(task.ptr, key.ptr, std::ptr::null_mut()),
        );
        Ok(value.is_some_and(|v| (api.CFEqual)(v.ptr, api.kCFBooleanTrue) != 0))
    }
}

#[cfg(test)]
mod tests {
    /// A test binary is signed with no App Sandbox entitlement (scripts/hvf-run gives it
    /// the hypervisor's alone): Security answers, and says no.
    #[test]
    fn a_test_binary_is_not_sandboxed() {
        assert_eq!(super::sandboxed(), Ok(false));
    }
}
