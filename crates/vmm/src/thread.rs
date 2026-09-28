//! Host scheduling policy for vCPU threads (macOS).
//!
//! With the default policy, HVF's in-kernel WFI wake-ups for guest timers are late by
//! ~25% of the interval (258 µs on a 1 ms timer) because of timer coalescing. Under
//! Mach's time-constraint policy they are late by 5-19 µs, and interrupt-delivery p99
//! to an idle vCPU drops from 61-315 µs to 11 µs
//! (docs/research/platform-measurements.md M8, M10).

/// CPU time a vCPU may use per scheduling period before XNU's real-time fail-safe
/// demotes it, and the latency window it asks for. These are the values measured in M10.
const COMPUTATION_NS: u64 = 500_000;
const CONSTRAINT_NS: u64 = 1_000_000;

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> libc::c_int;
}

/// Applies QoS user-interactive plus the Mach time-constraint policy to the calling
/// thread. Failure is reported, not fatal: the VM still runs, only with coarser timers.
pub fn make_current_realtime() -> Result<(), String> {
    // SAFETY: plain syscalls on the current thread with valid, initialized arguments.
    unsafe {
        let rc = libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
        if rc != 0 {
            return Err(format!("pthread_set_qos_class_self_np: {}", std::io::Error::from_raw_os_error(rc)));
        }
        let mut tb = MachTimebaseInfo { numer: 0, denom: 0 };
        mach_timebase_info(&mut tb);
        let to_abs = |ns: u64| (ns * tb.denom as u64 / tb.numer as u64) as u32;
        let mut policy = libc::thread_time_constraint_policy {
            period: 0,
            computation: to_abs(COMPUTATION_NS),
            constraint: to_abs(CONSTRAINT_NS),
            preemptible: 1,
        };
        let kr = libc::thread_policy_set(
            libc::pthread_mach_thread_np(libc::pthread_self()),
            libc::THREAD_TIME_CONSTRAINT_POLICY as libc::thread_policy_flavor_t,
            (&mut policy as *mut libc::thread_time_constraint_policy).cast(),
            libc::THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        );
        if kr != libc::KERN_SUCCESS {
            return Err(format!("thread_policy_set(THREAD_TIME_CONSTRAINT_POLICY) = {kr}"));
        }
    }
    Ok(())
}
