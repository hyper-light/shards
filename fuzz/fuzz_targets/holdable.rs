//! What an evaluation in the rego fuzz targets can hold (shards_rego's
//! `Context::holdable`, D98's number-text bound): the memory libFuzzer lets the process
//! have, less what it has had, less what AddressSanitizer keeps beside what is live.

/// libFuzzer's `-rss_limit_mb` (2048 by default, as CI passes it), in bytes: libFuzzer
/// ends the run once the process's peak RSS passes it (FuzzerLoop.cpp,
/// RssLimitCallback); 0 is no limit. A single allocation past `-malloc_limit_mb`, the
/// same by default, ends it too, and no allocation exceeds the bound it is held to.
fn rss_limit() -> Option<u64> {
    let mb = std::env::args()
        .filter_map(|a| {
            a.strip_prefix("-rss_limit_mb=")
                .and_then(|v| v.parse::<u64>().ok())
        })
        .last()
        .unwrap_or(2048);
    (mb > 0).then(|| mb.saturating_mul(1 << 20))
}

/// AddressSanitizer's quarantine, which keeps what is freed resident:
/// `quarantine_size_mb` of ASAN_OPTIONS, else its default, 256 MiB on 64-bit hosts
/// (compiler-rt asan_allocator.cpp, kDefaultQuarantineSizeMb).
fn quarantine() -> u64 {
    let mb = std::env::var("ASAN_OPTIONS").ok().and_then(|o| {
        o.split([':', ','])
            .filter_map(|kv| kv.strip_prefix("quarantine_size_mb="))
            .last()
            .and_then(|v| v.parse::<u64>().ok())
    });
    mb.unwrap_or(256).saturating_mul(1 << 20)
}

/// The process's peak RSS, which libFuzzer holds to its limit: getrusage(2)'s
/// ru_maxrss, KiB on Linux and bytes on macOS. Unknown, the limit itself: no room.
fn peak(limit: u64) -> u64 {
    // SAFETY: an rusage of integers, which getrusage fills.
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: getrusage of this process into the rusage it is given.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) } != 0 {
        return limit;
    }
    let max = u64::try_from(u.ru_maxrss).unwrap_or(0);
    if cfg!(target_os = "macos") { max } else { max.saturating_mul(1024) }
}

/// What an evaluation can hold now: the room left under the limit, less the quarantine
/// its frees may fill, of which ASan's shadow takes a byte for every 8 the evaluation
/// touches (asan_mapping.h, ASAN_SHADOW_SCALE 3).
pub fn holdable() -> u64 {
    let Some(limit) = rss_limit() else {
        return u64::MAX;
    };
    (limit.saturating_sub(peak(limit)) / 9 * 8).saturating_sub(quarantine())
}
