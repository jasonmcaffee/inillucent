//! What a process costs besides time: resident memory and processor time.
//!
//! Invariant: every number here comes from the operating system's own
//! accounting for a *process*, and none of it is inferred from anything this
//! workspace allocated. A gate that counted its own allocations would be
//! measuring one arm's bookkeeping against the other arm's kernel, and the
//! reference arm is a separate program this code cannot instrument at all.
//!
//! ## Why the two arms are measured differently, and why that is fair
//!
//! The reference is a child process - `sqlite-bench` - so its peak resident set
//! and its user and kernel time are exactly what the operating system reports
//! for that process when it exits. Nothing else runs in it.
//!
//! This engine's arm runs *inside the gate*, alongside the plan, the fixture
//! paths and the harness's own buffers, so the process peak is not the arm's
//! peak. What is comparable is the **delta**: the resident set before a
//! workload's rounds and after them, and the processor time the same interval
//! consumed. Both are reported as deltas and the process peak is reported
//! beside them, so a reader can see what the number is a number about rather
//! than being handed one figure that quietly means two things.
//!
//! ## What "peak" means on each platform
//!
//! Windows keeps a per-process high-water mark that only rises
//! (`PeakWorkingSetSize`), and so does Linux (`VmHWM`). Neither can be reset
//! from inside the process without releasing pages the allocator still owns, so
//! a *peak delta* over a workload is the rise in that mark and is zero for a
//! workload that never exceeded an earlier one. That is a true statement and a
//! misleading one on its own, which is why the current working set is reported
//! too: it goes down as well as up.

/// One process's memory and processor accounting at a moment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcessCost {
    /// Bytes of resident memory right now.
    pub working_set: u64,
    /// The largest resident set the process has ever had, in bytes.
    pub peak_working_set: u64,
    /// Processor time spent in user code, in nanoseconds.
    pub user_nanos: u64,
    /// Processor time spent in the kernel on this process's behalf.
    pub kernel_nanos: u64,
    /// Page faults the process has taken, soft and hard together.
    ///
    /// **Recorded because a fault is paid inside whichever clock is running when
    /// memory is first touched (task-2095).** The reference arm is a fresh child
    /// every round, so every page its cache and its sorter use is faulted in
    /// while a workload is being timed; this engine's arm is one long lived
    /// process whose pool is warmed before the round. The count says how many
    /// faults a round paid, so a pass whose time moved can be checked for
    /// whether the count moved with it or only the cost of each fault did.
    pub page_faults: u64,
}

impl ProcessCost {
    /// Returns what this process has cost so far.
    ///
    /// Zeroes when the platform will not say, which is a reading of "unknown"
    /// rather than of "nothing": the caller reports it as a blank rather than
    /// as a measurement.
    pub fn now() -> ProcessCost {
        platform::now()
    }

    /// Returns the change from an earlier reading.
    ///
    /// The working set is signed in principle - a workload may release more
    /// than it takes - and is clamped at zero here, because a negative resident
    /// set is not a thing a table of costs can say. The peak cannot fall.
    ///
    /// @param earlier - the reading taken before the interval
    pub fn since(&self, earlier: &ProcessCost) -> ProcessCost {
        ProcessCost {
            working_set: self.working_set.saturating_sub(earlier.working_set),
            peak_working_set: self
                .peak_working_set
                .saturating_sub(earlier.peak_working_set),
            user_nanos: self.user_nanos.saturating_sub(earlier.user_nanos),
            kernel_nanos: self.kernel_nanos.saturating_sub(earlier.kernel_nanos),
            page_faults: self.page_faults.saturating_sub(earlier.page_faults),
        }
    }

    /// Returns the processor time, user and kernel together.
    pub fn cpu_nanos(&self) -> u64 {
        self.user_nanos.saturating_add(self.kernel_nanos)
    }
}

/// Returns the peak resident set a finished child process reached, in bytes.
///
/// **Read while the process is still a handle, not after it is reaped.** A
/// process that has exited still has its accounting available through its
/// handle until the last one is closed, which is what makes this readable at
/// all - and is why the caller passes the `Child` rather than a pid.
///
/// Zero when the platform will not say.
///
/// @param child - the child, already waited on
pub fn child_cost(child: &std::process::Child) -> ProcessCost {
    platform::child_cost(child)
}

#[cfg(windows)]
mod platform {
    use super::ProcessCost;

    /// Windows counts processor time in 100-nanosecond units.
    const TICK_NANOS: u64 = 100;

    /// Returns a `FILETIME` as a count of 100-nanosecond ticks.
    ///
    /// @param time - the file time the kernel filled in
    fn ticks(time: &windows_sys::Win32::Foundation::FILETIME) -> u64 {
        (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
    }

    /// Returns what one process handle has cost.
    ///
    /// @param handle - a handle to the process to ask about
    fn of_handle(handle: windows_sys::Win32::Foundation::HANDLE) -> ProcessCost {
        use windows_sys::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows_sys::Win32::System::Threading::GetProcessTimes;
        let mut cost = ProcessCost::default();
        // SAFETY: both calls are given a handle this process owns and pointers
        // to stack structures whose `cb` field is set to their own size, which
        // is the contract `GetProcessMemoryInfo` documents. Neither writes past
        // the structure it is given, and a failure leaves the zeroed value in
        // place - which the caller reads as "unknown".
        unsafe {
            let mut counters: PROCESS_MEMORY_COUNTERS = core::mem::zeroed();
            counters.cb = core::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            if GetProcessMemoryInfo(handle, &mut counters, counters.cb) != 0 {
                cost.working_set = counters.WorkingSetSize as u64;
                cost.peak_working_set = counters.PeakWorkingSetSize as u64;
                cost.page_faults = u64::from(counters.PageFaultCount);
            }
            let mut created = core::mem::zeroed();
            let mut exited = core::mem::zeroed();
            let mut kernel = core::mem::zeroed();
            let mut user = core::mem::zeroed();
            if GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) != 0 {
                cost.kernel_nanos = ticks(&kernel).saturating_mul(TICK_NANOS);
                cost.user_nanos = ticks(&user).saturating_mul(TICK_NANOS);
            }
        }
        cost
    }

    /// Returns what this process has cost so far.
    pub fn now() -> ProcessCost {
        // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no
        // closing and is valid for the life of the process.
        let handle = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() };
        of_handle(handle)
    }

    /// Returns what a child process cost.
    ///
    /// @param child - the child, already waited on
    pub fn child_cost(child: &std::process::Child) -> ProcessCost {
        use std::os::windows::io::AsRawHandle;
        of_handle(child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE)
    }
}

#[cfg(not(windows))]
mod platform {
    use super::ProcessCost;

    /// Returns the value of one `/proc/self/status` field, in bytes.
    ///
    /// @param status - the file's contents
    /// @param field - the field's name, without its colon
    fn kilobytes(status: &str, field: &str) -> u64 {
        status
            .lines()
            .filter_map(|line| line.strip_prefix(field))
            .filter_map(|rest| rest.trim_start_matches(':').trim().split(' ').next())
            .filter_map(|number| number.trim().parse::<u64>().ok())
            .map(|value| value.saturating_mul(1024))
            .next()
            .unwrap_or(0)
    }

    /// Returns a `timeval` as nanoseconds.
    fn nanos(time: &libc::timeval) -> u64 {
        (time.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add((time.tv_usec as u64).saturating_mul(1_000))
    }

    /// Returns what this process has cost so far.
    ///
    /// The memory comes from `/proc/self/status`, which is the kernel's own
    /// accounting and needs no unsafe at all; the processor time comes from
    /// `getrusage`, which does.
    pub fn now() -> ProcessCost {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let mut cost = ProcessCost {
            working_set: kilobytes(&status, "VmRSS"),
            peak_working_set: kilobytes(&status, "VmHWM"),
            ..ProcessCost::default()
        };
        // SAFETY: `getrusage` fills a structure this frame owns and writes
        // nothing else. A failure leaves the zeroed value, read as "unknown".
        unsafe {
            let mut usage: libc::rusage = core::mem::zeroed();
            if libc::getrusage(libc::RUSAGE_SELF, &mut usage) == 0 {
                cost.user_nanos = nanos(&usage.ru_utime);
                cost.kernel_nanos = nanos(&usage.ru_stime);
                cost.page_faults = (usage.ru_minflt as u64).saturating_add(usage.ru_majflt as u64);
                // `ru_maxrss` is kilobytes on Linux and bytes on macOS; the
                // status file above is the Linux answer and is preferred when
                // it is there.
                if cost.peak_working_set == 0 {
                    cost.peak_working_set = (usage.ru_maxrss as u64).saturating_mul(1024);
                }
            }
        }
        cost
    }

    /// Returns what a child process cost.
    ///
    /// `RUSAGE_CHILDREN` is the sum over every reaped child, so it is read as a
    /// difference by the caller taking it before and after. The gate runs one
    /// child at a time, which is what makes that difference this child's.
    ///
    /// @param _child - the child, already waited on
    pub fn child_cost(_child: &std::process::Child) -> ProcessCost {
        let mut cost = ProcessCost::default();
        // SAFETY: as above.
        unsafe {
            let mut usage: libc::rusage = core::mem::zeroed();
            if libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) == 0 {
                cost.user_nanos = nanos(&usage.ru_utime);
                cost.kernel_nanos = nanos(&usage.ru_stime);
                cost.page_faults = (usage.ru_minflt as u64).saturating_add(usage.ru_majflt as u64);
                cost.peak_working_set = (usage.ru_maxrss as u64).saturating_mul(1024);
                cost.working_set = cost.peak_working_set;
            }
        }
        cost
    }
}

/// Renders a byte count the way a report reads it.
///
/// @param bytes - the count
pub fn mebibytes(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Renders a nanosecond count as milliseconds.
///
/// @param nanos - the count
pub fn millis(nanos: u64) -> f64 {
    nanos as f64 / 1_000_000.0
}
