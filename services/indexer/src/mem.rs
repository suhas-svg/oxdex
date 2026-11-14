//! Process memory as the OS sees it (Task Manager's "working set").

/// `(current, peak)` resident bytes, if the platform reports them.
#[cfg(windows)]
pub fn rss() -> Option<(u64, u64)> {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut c: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: valid out-pointer and size; GetCurrentProcess is a pseudo-handle.
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c, size) };
    (ok != 0).then_some((c.WorkingSetSize as u64, c.PeakWorkingSetSize as u64))
}

#[cfg(target_os = "linux")]
pub fn rss() -> Option<(u64, u64)> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb = |key: &str| -> Option<u64> {
        let line = status.lines().find(|l| l.starts_with(key))?;
        line.split_whitespace().nth(1)?.parse::<u64>().ok().map(|k| k * 1024)
    };
    Some((kb("VmRSS:")?, kb("VmHWM:")?))
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn rss() -> Option<(u64, u64)> {
    None
}

pub fn mb(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}
