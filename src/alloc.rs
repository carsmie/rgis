//! Freed heap back to the OS once requests stop. tcmalloc (the image's allocator) and glibc both
//! keep freed pages: after a burst a pod idled at its peak (4.4 of 6 GiB) and the
//! next burst OOM-killed it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Requests finished so far; a release only follows new work.
static DONE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn request_done() {
    DONE.fetch_add(1, Ordering::Relaxed);
}

/// Every RGIS_RELEASE_IDLE_SECS (default 5, 0 = off): with no request in flight and some finished
/// since the last check, return free memory to the OS.
pub fn start_idle_release() {
    let secs = std::env::var("RGIS_RELEASE_IDLE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(5);
    if secs == 0 {
        return;
    }
    std::thread::Builder::new()
        .name("idle-release".into())
        .spawn(move || {
            let mut seen = 0;
            loop {
                std::thread::sleep(Duration::from_secs(secs));
                let done = DONE.load(Ordering::Relaxed);
                if done != seen && crate::metrics::in_flight_now() == 0 {
                    seen = done;
                    release();
                }
            }
        })
        .expect("spawn idle-release thread");
}

/// Over the warn line: new requests get 503, in-flight ones continue.
static BUSY: AtomicBool = AtomicBool::new(false);
/// Over the shed line: in-flight requests abort; server::handle answers 503.
static PRESSURE: AtomicBool = AtomicBool::new(false);
/// Shed episode count; a request overlapping one gets 503, never a partial image.
static EPISODE: AtomicU64 = AtomicU64::new(0);

/// Shedding? One relaxed load: cheap enough for per-feature loops.
#[inline]
pub(crate) fn under_pressure() -> bool {
    PRESSURE.load(Ordering::Relaxed)
}

/// Turning new requests away (over the warn line or shedding).
pub(crate) fn busy() -> bool {
    BUSY.load(Ordering::Relaxed) || under_pressure()
}

pub(crate) fn episode() -> u64 {
    EPISODE.load(Ordering::Relaxed)
}

/// Shedding now, or an episode began since `start` (episode() at request start; None, e.g.
/// warm-up: current pressure only).
pub(crate) fn shed_since(start: Option<u64>) -> bool {
    under_pressure() || start.is_some_and(|s| episode() != s)
}

/// Memory guard: pods were cgroup-OOM-killed by +3-5 GiB spikes shorter than the 60 s
/// scrape, losing every in-flight request. Polls the cgroup working set (RSS without a cgroup)
/// every RGIS_MEMORY_POLL_MS (50). Over RGIS_MEMORY_WARN_PCT (60) of the limit: release free heap;
/// still over, 503 new requests and log in-flight ones with their layer (names the culprit). Over
/// RGIS_MEMORY_SHED_PCT (85): fail in-flight requests too, until back under warn.
/// Limit: RGIS_MEMORY_LIMIT_MB, else cgroup memory.max; neither → off.
pub fn start_memory_guard() {
    let env = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u64>().ok());
    let Some(limit) = env("RGIS_MEMORY_LIMIT_MB")
        .map(|mb| mb << 20)
        .or_else(cgroup_limit)
        .filter(|l| *l > 0)
    else {
        tracing::info!("memory guard off: no cgroup limit or RGIS_MEMORY_LIMIT_MB");
        return;
    };
    let warn_at = limit / 100 * env("RGIS_MEMORY_WARN_PCT").unwrap_or(60).min(100);
    let shed_at = limit / 100 * env("RGIS_MEMORY_SHED_PCT").unwrap_or(85).min(100);
    let poll = Duration::from_millis(env("RGIS_MEMORY_POLL_MS").unwrap_or(50).max(10));
    tracing::info!(
        "memory guard: limit {} MB, warn {} MB, shed {} MB",
        limit >> 20,
        warn_at >> 20,
        shed_at >> 20
    );
    std::thread::Builder::new()
        .name("memory-guard".into())
        .spawn(move || {
            let mut last_release = std::time::Instant::now();
            loop {
                std::thread::sleep(poll);
                let mut ws = working_set();
                // Return tcmalloc's freed pages before turning anyone away (idle release never
                // runs under steady load). Below shed: at most 1/s (reuse costs page faults).
                // Above it or shedding: every poll (aborted requests free to tcmalloc, not the OS).
                if ws >= warn_at
                    && (ws >= shed_at
                        || under_pressure()
                        || last_release.elapsed() >= Duration::from_secs(1))
                {
                    release();
                    last_release = std::time::Instant::now();
                    ws = working_set();
                }
                if ws >= shed_at && !under_pressure() {
                    EPISODE.fetch_add(1, Ordering::Relaxed);
                    PRESSURE.store(true, Ordering::Relaxed);
                    crate::metrics::memory_shed();
                    tracing::warn!(
                        "memory {} MB ≥ shed line {} MB of {} MB: failing in-flight requests; in flight: {}",
                        ws >> 20,
                        shed_at >> 20,
                        limit >> 20,
                        crate::server::in_flight_summary()
                    );
                }
                if ws >= warn_at && !BUSY.load(Ordering::Relaxed) {
                    BUSY.store(true, Ordering::Relaxed);
                    tracing::warn!(
                        "memory {} MB ≥ warn line {} MB of {} MB: turning new requests away; in flight: {}",
                        ws >> 20,
                        warn_at >> 20,
                        limit >> 20,
                        crate::server::in_flight_summary()
                    );
                } else if ws < warn_at && BUSY.load(Ordering::Relaxed) {
                    BUSY.store(false, Ordering::Relaxed);
                    PRESSURE.store(false, Ordering::Relaxed);
                    tracing::info!("memory {} MB: back under the warn line", ws >> 20);
                }
            }
        })
        .expect("spawn memory-guard thread");
}

/// Kubelet's cgroup v2 working set (memory.current − inactive_file); RSS without cgroup v2.
fn working_set() -> u64 {
    let cg = || -> Option<u64> {
        let current = parse_limit(&std::fs::read_to_string("/sys/fs/cgroup/memory.current").ok()?)?;
        let stat = std::fs::read_to_string("/sys/fs/cgroup/memory.stat").ok()?;
        Some(current.saturating_sub(stat_value(&stat, "inactive_file").unwrap_or(0)))
    };
    cg().unwrap_or_else(rss_bytes)
}

/// A `key value` line of memory.stat.
fn stat_value(stat: &str, key: &str) -> Option<u64> {
    stat.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(' ')?.trim().parse().ok())
}

/// Container memory limit: cgroup v2 memory.max, else v1 memory.limit_in_bytes.
fn cgroup_limit() -> Option<u64> {
    parse_limit(&std::fs::read_to_string("/sys/fs/cgroup/memory.max").ok()?)
        .or_else(|| {
            parse_limit(
                &std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes").ok()?,
            )
        })
}

/// "max" and v1's "no limit" sentinel (≈ i64::MAX rounded to pages) mean unlimited.
fn parse_limit(s: &str) -> Option<u64> {
    s.trim().parse::<u64>().ok().filter(|v| *v < 1 << 50)
}

/// Return free heap pages to the OS; logs what it freed when it was a lot.
pub fn release() {
    let before = rss_bytes();
    let how = sys::release();
    let after = rss_bytes();
    let freed = before.saturating_sub(after) >> 20;
    if freed >= 100 {
        tracing::info!(
            "released {freed} MB of free heap ({how}), RSS now {} MB",
            after >> 20
        );
    } else {
        tracing::debug!("released {freed} MB of free heap ({how})");
    }
}

/// Resident set size from /proc/self/statm (0 where unavailable).
fn rss_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map_or(0, |pages| pages * 4096)
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod sys {
    use std::ffi::{c_char, c_int, c_void};
    use std::sync::LazyLock;

    unsafe extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        fn malloc_trim(pad: usize) -> c_int;
    }

    /// gperftools' C API, present when tcmalloc is preloaded (LD_PRELOAD in the image).
    static TCMALLOC_RELEASE: LazyLock<Option<unsafe extern "C" fn()>> = LazyLock::new(|| {
        // RTLD_DEFAULT is null on glibc.
        let f = unsafe {
            dlsym(
                std::ptr::null_mut(),
                c"MallocExtension_ReleaseFreeMemory".as_ptr(),
            )
        };
        (!f.is_null())
            .then(|| unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn()>(f) })
    });

    pub fn release() -> &'static str {
        match *TCMALLOC_RELEASE {
            Some(f) => {
                unsafe { f() };
                "tcmalloc"
            }
            None => {
                unsafe { malloc_trim(0) };
                "malloc_trim"
            }
        }
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
mod sys {
    pub fn release() -> &'static str {
        "unsupported"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_limits() {
        assert_eq!(parse_limit("6442450944\n"), Some(6 << 30));
        assert_eq!(parse_limit("max\n"), None);
        // cgroup v1 "unlimited"
        assert_eq!(parse_limit("9223372036854771712"), None);
    }

    #[test]
    fn memory_stat_lines() {
        let stat = "anon 100\ninactive_anon 5\ninactive_file 4096\nactive_file 7\n";
        assert_eq!(stat_value(stat, "inactive_file"), Some(4096));
        assert_eq!(stat_value(stat, "anon"), Some(100));
        assert_eq!(stat_value(stat, "file"), None);
    }

    #[test]
    fn shed_since_compares_episodes() {
        // Read-only: bumping EPISODE would shed concurrent tests.
        let now = episode();
        assert!(!shed_since(None));
        assert!(!shed_since(Some(now)));
        assert!(shed_since(Some(now.wrapping_sub(1))));
    }
}
