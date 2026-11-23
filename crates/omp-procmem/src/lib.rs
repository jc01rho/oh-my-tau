//! Process memory readings for the `memory_report` RPC.
//!
//! Linux reads `/proc/<pid>/status` (`VmRSS`, `VmHWM`) and `/proc/self/smaps_rollup`; macOS asks the
//! kernel for the task's `phys_footprint` (`task_info(TASK_VM_INFO)`) and for another process's
//! resident size (`proc_pid_rusage`). Any other platform answers [`MemoryError::Unsupported`]:
//! there is no estimate to fall back on, so no number is invented.

use std::fmt;

/// What the kernel reports about the calling process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessMemory {
    /// The number the gateway tracks as the process footprint.
    pub footprint_bytes: u64,
    /// Which kernel counter `footprint_bytes` is (`VmRSS` on Linux, `phys_footprint` on macOS).
    pub footprint_source: &'static str,
    /// Resident set size.
    pub rss_bytes: u64,
    /// Peak resident set size, when the kernel keeps one.
    pub peak_rss_bytes: Option<u64>,
    /// `smaps_rollup` counters (`Rss`, `Pss`, `Private_Dirty`, ...) in bytes, in file order.
    /// `None` when the platform has no such file or the kernel (before 4.14) lacks it.
    pub smaps_rollup: Option<Vec<(String, u64)>>,
}

/// Why a reading is not available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryError {
    /// This platform has no supported source of process memory.
    Unsupported { os: &'static str },
    /// The platform is supported but the kernel did not answer.
    Unreadable(String),
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported { os } => {
                write!(f, "process memory is not available on {os}")
            }
            Self::Unreadable(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for MemoryError {}

/// The calling process's memory.
///
/// # Errors
/// [`MemoryError::Unsupported`] off Linux and macOS; [`MemoryError::Unreadable`] when the kernel
/// does not answer.
pub fn current_process() -> Result<ProcessMemory, MemoryError> {
    imp::current_process()
}

/// Resident set size of process `pid`, in bytes.
///
/// # Errors
/// [`MemoryError::Unsupported`] off Linux and macOS; [`MemoryError::Unreadable`] when the process
/// is gone or belongs to someone else.
pub fn process_rss_bytes(pid: u32) -> Result<u64, MemoryError> {
    imp::process_rss_bytes(pid)
}

/// The `kB` counter `key` of a `/proc` status-style text, in bytes.
#[cfg(any(target_os = "linux", test))]
fn kb_field(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))
        .and_then(|rest| rest.trim().strip_suffix("kB"))
        .and_then(|n| n.trim().parse::<u64>().ok())
        .map(|kb| kb * 1024)
}

/// Every `Name: N kB` line of a `smaps_rollup` text, in bytes.
#[cfg(any(target_os = "linux", test))]
fn smaps_rollup_counters(text: &str) -> Vec<(String, u64)> {
    text.lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once(':')?;
            let kb = rest.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()?;
            Some((name.trim().to_owned(), kb * 1024))
        })
        .collect()
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{kb_field, smaps_rollup_counters, MemoryError, ProcessMemory};

    fn unreadable(path: &str, e: &std::io::Error) -> MemoryError {
        MemoryError::Unreadable(format!("{path}: {e}"))
    }

    pub(super) fn current_process() -> Result<ProcessMemory, MemoryError> {
        let status = std::fs::read_to_string("/proc/self/status")
            .map_err(|e| unreadable("/proc/self/status", &e))?;
        let rss_bytes = kb_field(&status, "VmRSS").ok_or_else(|| {
            MemoryError::Unreadable("/proc/self/status has no VmRSS line".to_owned())
        })?;
        let smaps_rollup = std::fs::read_to_string("/proc/self/smaps_rollup")
            .ok()
            .map(|text| smaps_rollup_counters(&text))
            .filter(|counters| !counters.is_empty());
        Ok(ProcessMemory {
            footprint_bytes: rss_bytes,
            footprint_source: "VmRSS",
            rss_bytes,
            peak_rss_bytes: kb_field(&status, "VmHWM"),
            smaps_rollup,
        })
    }

    pub(super) fn process_rss_bytes(pid: u32) -> Result<u64, MemoryError> {
        let path = format!("/proc/{pid}/status");
        let status = std::fs::read_to_string(&path).map_err(|e| unreadable(&path, &e))?;
        kb_field(&status, "VmRSS")
            .ok_or_else(|| MemoryError::Unreadable(format!("{path} has no VmRSS line")))
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{MemoryError, ProcessMemory};

    /// `TASK_VM_INFO` flavor of `task_info`.
    const TASK_VM_INFO: libc::task_flavor_t = 22;

    /// `struct task_vm_info` of `<mach/task_info.h>` up to `phys_footprint` (revision 1).
    #[repr(C)]
    #[derive(Default)]
    struct TaskVmInfo {
        virtual_size: u64,
        region_count: i32,
        page_size: i32,
        resident_size: u64,
        resident_size_peak: u64,
        device: u64,
        device_peak: u64,
        internal: u64,
        internal_peak: u64,
        external: u64,
        external_peak: u64,
        reusable: u64,
        reusable_peak: u64,
        purgeable_volatile_pmap: u64,
        purgeable_volatile_resident: u64,
        purgeable_volatile_virtual: u64,
        compressed: u64,
        compressed_peak: u64,
        compressed_lifetime: u64,
        phys_footprint: u64,
    }

    pub(super) fn current_process() -> Result<ProcessMemory, MemoryError> {
        let mut info = TaskVmInfo::default();
        let words = std::mem::size_of::<TaskVmInfo>() / std::mem::size_of::<libc::integer_t>();
        let mut count = libc::mach_msg_type_number_t::try_from(words)
            .map_err(|e| MemoryError::Unreadable(e.to_string()))?;
        // SAFETY: `info` is a live `repr(C)` buffer of exactly `count` 32-bit words, `count` is
        // in/out, and the kernel writes no more than `count` words into it. The current task's
        // port is always valid.
        #[allow(deprecated)]
        let rc = unsafe {
            libc::task_info(
                libc::mach_task_self(),
                TASK_VM_INFO,
                std::ptr::from_mut(&mut info).cast::<libc::integer_t>(),
                &mut count,
            )
        };
        if rc != libc::KERN_SUCCESS {
            return Err(MemoryError::Unreadable(format!(
                "task_info(TASK_VM_INFO) failed with kern_return_t {rc}"
            )));
        }
        // A kernel that stops before `phys_footprint` leaves it zero: that is no reading.
        if info.phys_footprint == 0 || usize::try_from(count).unwrap_or(0) < words {
            return Err(MemoryError::Unreadable(
                "task_info(TASK_VM_INFO) did not report phys_footprint".to_owned(),
            ));
        }
        Ok(ProcessMemory {
            footprint_bytes: info.phys_footprint,
            footprint_source: "phys_footprint",
            rss_bytes: info.resident_size,
            peak_rss_bytes: Some(info.resident_size_peak),
            smaps_rollup: None,
        })
    }

    pub(super) fn process_rss_bytes(pid: u32) -> Result<u64, MemoryError> {
        let pid = libc::c_int::try_from(pid)
            .map_err(|_| MemoryError::Unreadable(format!("pid {pid} is out of range")))?;
        // SAFETY: an all-zero `rusage_info_v2` is a valid value of that plain-integer struct.
        let mut usage: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        // SAFETY: `proc_pid_rusage` fills the `rusage_info_v2` the buffer points at for flavor
        // `RUSAGE_INFO_V2` and touches nothing else; the C signature takes it as a `void *`.
        let rc = unsafe {
            libc::proc_pid_rusage(
                pid,
                libc::RUSAGE_INFO_V2,
                std::ptr::from_mut(&mut usage).cast::<libc::rusage_info_t>(),
            )
        };
        if rc != 0 {
            return Err(MemoryError::Unreadable(format!(
                "proc_pid_rusage({pid}): {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(usage.ri_resident_size)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use super::{MemoryError, ProcessMemory};

    pub(super) fn current_process() -> Result<ProcessMemory, MemoryError> {
        Err(MemoryError::Unsupported {
            os: std::env::consts::OS,
        })
    }

    pub(super) fn process_rss_bytes(_pid: u32) -> Result<u64, MemoryError> {
        Err(MemoryError::Unsupported {
            os: std::env::consts::OS,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = "Name:\tomt\nVmPeak:\t  200000 kB\nVmHWM:\t   54321 kB\nVmRSS:\t   43210 kB\nRssAnon:\t   40000 kB\nThreads:\t4\n";

    #[test]
    fn status_counters_are_read_by_exact_name_in_bytes() {
        assert_eq!(kb_field(STATUS, "VmRSS"), Some(43_210 * 1024));
        assert_eq!(kb_field(STATUS, "VmHWM"), Some(54_321 * 1024));
        assert_eq!(kb_field(STATUS, "Rss"), None, "RssAnon is not Rss");
        assert_eq!(kb_field(STATUS, "Threads"), None, "no kB unit, no reading");
        assert_eq!(kb_field("VmRSS:\tlots kB\n", "VmRSS"), None);
    }

    #[test]
    fn smaps_rollup_keeps_every_counter_and_skips_the_range_header() {
        let text = "55a8c3a00000-7ffd5b9f1000 ---p 00000000 00:00 0   [rollup]\nRss:               6560 kB\nPss:               2230 kB\nPrivate_Dirty:      512 kB\nSwap:                 0 kB\nProtection: all\n";
        assert_eq!(
            smaps_rollup_counters(text),
            vec![
                ("Rss".to_owned(), 6560 * 1024),
                ("Pss".to_owned(), 2230 * 1024),
                ("Private_Dirty".to_owned(), 512 * 1024),
                ("Swap".to_owned(), 0),
            ]
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn the_running_process_has_a_footprint() {
        let memory = current_process().expect("this platform reports process memory");
        assert!(memory.footprint_bytes > 0, "{memory:?}");
        assert!(memory.rss_bytes > 0, "{memory:?}");
        assert!(memory.peak_rss_bytes.is_some_and(|peak| peak > 0));
        if cfg!(target_os = "linux") {
            assert_eq!(memory.footprint_source, "VmRSS");
            assert_eq!(memory.footprint_bytes, memory.rss_bytes);
            assert!(memory
                .smaps_rollup
                .is_some_and(|c| c.iter().any(|(name, _)| name == "Rss")));
        } else {
            assert_eq!(memory.footprint_source, "phys_footprint");
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn another_process_is_measured_by_pid_and_a_missing_one_is_an_error() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let rss = process_rss_bytes(child.id());
        child.kill().expect("kill sleep");
        child.wait().expect("reap sleep");
        assert!(rss.expect("a live child has a resident size") > 0);
        assert!(matches!(
            process_rss_bytes(child.id()),
            Err(MemoryError::Unreadable(_))
        ));
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn other_platforms_say_unsupported_instead_of_guessing() {
        assert!(matches!(
            current_process(),
            Err(MemoryError::Unsupported { .. })
        ));
        assert!(matches!(
            process_rss_bytes(1),
            Err(MemoryError::Unsupported { .. })
        ));
    }
}
