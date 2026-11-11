//! Freed heap back to the OS once requests stop. tcmalloc (the image's allocator) and glibc both
//! keep freed pages: after a burst a pod idled at its peak (4.4 of 6 GiB on atgcp1-dev) and the
//! next burst OOM-killed it.

use std::sync::atomic::{AtomicU64, Ordering};
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
