//! The host monotonic clock all capture timestamps are expressed in. It is the
//! clock each OS's native capture APIs stamp buffers with: mach absolute time
//! on macOS (ScreenCaptureKit, CoreMedia), QPC on Windows (Graphics Capture),
//! and `CLOCK_MONOTONIC` on Linux (PipeWire).

use visor_core::Micros;

#[cfg(target_os = "macos")]
pub fn host_now() -> Micros {
    use std::sync::OnceLock;

    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_absolute_time() -> u64;
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    }
    static TIMEBASE: OnceLock<(u128, u128)> = OnceLock::new();
    let &(numer, denom) = TIMEBASE.get_or_init(|| {
        let mut info = MachTimebaseInfo { numer: 1, denom: 1 };
        // SAFETY: plain out-parameter call into libSystem.
        unsafe { mach_timebase_info(&mut info) };
        (u128::from(info.numer), u128::from(info.denom))
    });
    // SAFETY: no preconditions.
    let ticks = u128::from(unsafe { mach_absolute_time() });
    (ticks * numer / denom / 1_000) as Micros
}

#[cfg(target_os = "windows")]
pub fn host_now() -> Micros {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let (mut counter, mut freq) = (0i64, 0i64);
    // SAFETY: plain out-parameter calls; both always succeed on XP and later.
    unsafe {
        let _ = QueryPerformanceCounter(&mut counter);
        let _ = QueryPerformanceFrequency(&mut freq);
    }
    (counter as u128 * 1_000_000 / freq.max(1) as u128) as Micros
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn host_now() -> Micros {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid out-pointer; CLOCK_MONOTONIC is always available.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as Micros * 1_000_000 + ts.tv_nsec as Micros / 1_000
}

#[cfg(test)]
mod tests {
    #[test]
    fn is_monotonic_and_in_microseconds() {
        let a = super::host_now();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = super::host_now();
        assert!((15_000..200_000).contains(&(b - a)), "elapsed {}", b - a);
    }
}
