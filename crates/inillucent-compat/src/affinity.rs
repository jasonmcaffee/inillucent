//! Which processors a measurement runs on, and making both arms run on the same ones.
//!
//! Invariant: **a program that times this engine against SQLite runs both arms
//! on one class of core, says which processors those were, and refuses to
//! measure when the reference child is on different processors from itself.**
//!
//! ## Why this exists (task-2085)
//!
//! The machine the published numbers come from is an Intel Core Ultra 9 285:
//! 8 performance cores and 16 efficiency cores. `inillucent-fullgate` runs this
//! engine inside its own process and SQLite in a `sqlite-bench` child, and with
//! no affinity set Windows put the gate on the efficiency cores and the child on
//! the performance cores. Both arms of every paired round were then measured on
//! different hardware, and nothing in the output said so. task-2064 measured the
//! read families three ways, this engine's time against SQLite's in ms:
//!
//! | | performance cores | efficiency cores | not pinned |
//! |---|---|---|---|
//! | `scan.aggregate` | 1.76 / 92.9 | 2.97 / 115.9 | 2.95 / 92.9 |
//! | `scan.group` | 2.77 / 76.5 | 6.48 / 90.9 | 6.31 / 77.9 |
//!
//! Not pinned, this engine's column matched the efficiency cores and SQLite's
//! matched the performance cores, and on another day both matched the
//! performance cores. So a gate figure depended on what the scheduler did that
//! day. The fix has two parts, and the second matters as much as the first:
//! the process pins itself before anything is measured, and the report prints
//! the mask it ran on, so a run on the wrong processors can be seen in the
//! output rather than worked out afterwards.
//!
//! ## How a core class is found
//!
//! Never from a hard coded mask. On Windows `GetSystemCpuSetInformation`
//! reports an `EfficiencyClass` per logical processor, and it is **highest on
//! the performance cores**. On Linux the kernel reports `cpu_capacity` per CPU
//! on a hybrid part, and `cpufreq/cpuinfo_max_freq` everywhere else that has
//! cpufreq; the performance cores are the CPUs with the highest value. A
//! machine where every processor reports the same value has one class and
//! nothing is pinned.
//!
//! ## Why the child needs nothing done to it
//!
//! A process's affinity is inherited by every process it creates, on Windows
//! through `CreateProcess` and on Linux through `fork`. So pinning the gate
//! before it starts `sqlite-bench` pins `sqlite-bench` too. That is an
//! assumption about the operating system, so [`spawn_on_same_cores`] reads the
//! child's mask back and refuses when it differs, and `tests/affinity.rs`
//! fails when it does.
//!
//! ## What pinning costs one workload
//!
//! `read.correlated` is the one place the answer went the other way:
//! `correlated.exists` took 59.69 ms on the 8 performance cores, 46.35 ms on
//! the 16 efficiency cores and 38.74 ms unpinned with all 24. A path that uses
//! more than one thread is limited to the processors in the mask, so a pinned
//! run reports it slower than an unpinned one. `--cores any` takes the
//! unpinned figure on purpose.
//!
//! ## Limits
//!
//! Windows affinity masks are per processor group, and a group holds at most
//! 64 logical processors. This pins within group 0 and says so when the class
//! it wanted has processors in another group. macOS has no call that confines
//! a process to a set of cores, so there nothing is pinned and the report says
//! that too.

use std::process::{Child, Command};

/// Which class of core a measurement is confined to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreClass {
    /// The processors with the highest efficiency class or capacity. The default.
    Performance,
    /// The processors with the lowest efficiency class or capacity.
    Efficiency,
    /// Not pinned: whatever mask the process was started with.
    Any,
}

impl CoreClass {
    /// Reads a class from its command line spelling.
    ///
    /// @param text - `performance`, `efficiency` or `any`
    pub fn parse(text: &str) -> Result<CoreClass, String> {
        match text {
            "performance" | "p" => Ok(CoreClass::Performance),
            "efficiency" | "e" => Ok(CoreClass::Efficiency),
            "any" => Ok(CoreClass::Any),
            other => Err(format!(
                "`--cores` wants performance, efficiency or any, not `{other}`"
            )),
        }
    }

    /// Returns the class's command line spelling.
    pub fn name(&self) -> &'static str {
        match self {
            CoreClass::Performance => "performance",
            CoreClass::Efficiency => "efficiency",
            CoreClass::Any => "any",
        }
    }
}

/// One logical processor and how the operating system ranks it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Processor {
    /// The logical processor number the affinity calls use.
    pub index: usize,
    /// The efficiency class, capacity or maximum frequency. Higher is faster.
    pub rank: u64,
}

/// Where a program ended up running after it asked to be pinned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    /// The class that was asked for.
    pub asked: CoreClass,
    /// Whether this program changed its own affinity.
    pub pinned: bool,
    /// The processors the process may run on, read back after pinning.
    pub processors: Vec<usize>,
    /// Every logical processor the machine reported.
    pub machine: Vec<Processor>,
    /// Why the process was not pinned, when it was not.
    pub reason: Option<String>,
}

impl Placement {
    /// Returns the processors as a hexadecimal mask, `0xC03C03` for this
    /// machine's performance cores.
    pub fn mask(&self) -> String {
        mask_of(&self.processors)
    }

    /// Returns how many core classes the machine has.
    pub fn class_count(&self) -> usize {
        let mut ranks: Vec<u64> = self.machine.iter().map(|each| each.rank).collect();
        ranks.sort_unstable();
        ranks.dedup();
        ranks.len()
    }

    /// Returns the value a results row records: the class and the mask,
    /// `performance:0xC03C03`, or `any:0xFFFFFF` for a run nobody pinned.
    pub fn row_field(&self) -> String {
        let class = if self.pinned || self.asked == CoreClass::Any {
            self.asked.name()
        } else {
            "unpinned"
        };
        format!("{class}:{}", self.mask())
    }

    /// Returns the lines a report's `## configuration` block prints.
    pub fn configuration_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "  cores       : {} - {} of {} logical processors, mask {}",
            self.asked.name(),
            self.processors.len(),
            self.machine.len().max(self.processors.len()),
            self.mask()
        )];
        lines.push(format!("                {}", self.classes_sentence()));
        match (&self.reason, self.pinned) {
            (Some(reason), _) => lines.push(format!("                not pinned: {reason}")),
            (None, true) => lines.push(
                "                pinned by this program before anything was timed; every child it"
                    .to_string(),
            ),
            (None, false) => {}
        }
        if self.pinned {
            lines.push(
                "                starts inherits the mask and is checked against it when started"
                    .to_string(),
            );
            lines.push(
                "                a workload that uses more than one thread is limited to these processors"
                    .to_string(),
            );
        }
        lines.push(
            "                --cores performance|efficiency|any chooses; any leaves the mask alone"
                .to_string(),
        );
        lines
    }

    /// Prints the configuration lines to standard output.
    pub fn print_configuration(&self) {
        for line in self.configuration_lines() {
            println!("{line}");
        }
    }

    /// Returns a sentence naming the machine's core classes and their sizes.
    fn classes_sentence(&self) -> String {
        let Some(top) = self.machine.iter().map(|each| each.rank).max() else {
            return "the machine did not report its processors".to_string();
        };
        let bottom = self
            .machine
            .iter()
            .map(|each| each.rank)
            .min()
            .unwrap_or(top);
        if top == bottom {
            return format!(
                "this machine has one core class of {} logical processors",
                self.machine.len()
            );
        }
        let fast = self.machine.iter().filter(|each| each.rank == top).count();
        let slow = self
            .machine
            .iter()
            .filter(|each| each.rank == bottom)
            .count();
        format!(
            "this machine has {} core classes: {fast} performance, {slow} efficiency",
            self.class_count()
        )
    }
}

/// Removes `--cores <class>` from a command line and returns the class.
///
/// Removed rather than read, so a program whose parser refuses an option it
/// does not know, or reads its first free word as a path, sees the command
/// line it always saw. Performance when the flag is absent.
///
/// @param arguments - the command line, without the program name
pub fn take_cores_flag(arguments: &mut Vec<String>) -> Result<CoreClass, String> {
    let Some(at) = arguments.iter().position(|value| value == "--cores") else {
        return Ok(CoreClass::Performance);
    };
    let value = arguments
        .get(at.saturating_add(1))
        .cloned()
        .ok_or("`--cores` needs performance, efficiency or any")?;
    let class = CoreClass::parse(&value)?;
    arguments.drain(at..at.saturating_add(2));
    Ok(class)
}

/// Reads `--cores` off a command line, pins this process and prints the result.
///
/// The one call a gate makes at the start of `main`. It removes the flag from
/// `arguments`, pins, prints the lines under a `## cores` heading so the
/// placement is in the output even of a program with no configuration block,
/// and returns the placement for a program that records it in its rows.
///
/// @param arguments - the command line, without the program name
pub fn pin_from_arguments(arguments: &mut Vec<String>) -> Result<Placement, String> {
    let class = take_cores_flag(arguments)?;
    let placement = pin(class)?;
    println!("## cores");
    placement.print_configuration();
    Ok(placement)
}

/// Confines this process, and every process it starts from now on, to one core class.
///
/// A machine with one class is left alone. `Any` is left alone. An error only
/// when the operating system refused a mask it was given, because a gate that
/// asked to be pinned and was not must not go on to publish a number.
///
/// @param class - the class to confine the process to
pub fn pin(class: CoreClass) -> Result<Placement, String> {
    let machine = platform::processors();
    let top = machine.iter().map(|each| each.rank).max();
    let bottom = machine.iter().map(|each| each.rank).min();
    let wanted = match (class, top, bottom) {
        (CoreClass::Any, _, _) => None,
        (_, Some(high), Some(low)) if high == low => None,
        (CoreClass::Performance, Some(high), _) => Some(high),
        (CoreClass::Efficiency, _, Some(low)) => Some(low),
        _ => None,
    };
    let reason = match (class, wanted, machine.is_empty()) {
        (CoreClass::Any, _, _) => Some("--cores any was asked for".to_string()),
        (_, _, true) => Some(platform::UNSUPPORTED.to_string()),
        (_, None, false) => Some("this machine has one core class".to_string()),
        (_, Some(_), false) => None,
    };
    if let Some(rank) = wanted {
        let chosen: Vec<usize> = machine
            .iter()
            .filter(|each| each.rank == rank)
            .map(|each| each.index)
            .collect();
        pin_processors(&chosen)?;
    }
    Ok(Placement {
        asked: class,
        pinned: wanted.is_some(),
        processors: current_processors(),
        machine,
        reason,
    })
}

/// Confines this process to exactly the processors named.
///
/// @param processors - logical processor numbers
pub fn pin_processors(processors: &[usize]) -> Result<(), String> {
    platform::set_current(processors)
}

/// Returns the logical processors this process may run on.
pub fn current_processors() -> Vec<usize> {
    platform::current()
}

/// Returns the logical processors a child process may run on.
///
/// None when the platform cannot say, which on macOS is always.
///
/// @param child - a child that has not yet been waited on
pub fn child_processors(child: &Child) -> Option<Vec<usize>> {
    platform::of_child(child)
}

/// Moves a running child to other processors.
///
/// A gate never calls this. It exists so `tests/affinity.rs` can put a child on
/// processors its parent is not on and prove [`confirm_child`] refuses it.
///
/// @param child - a child that has not yet been waited on
/// @param processors - logical processor numbers
pub fn move_child(child: &Child, processors: &[usize]) -> Result<(), String> {
    platform::set_child(child, processors)
}

/// Checks that a child runs on the same processors as this process.
///
/// Ok when they match, and Ok when the platform cannot read a child's mask.
/// The error names both masks, so the refusal says what went wrong.
///
/// @param child - a child that has not yet been waited on
/// @param name - what the child is, for the message
pub fn confirm_child(child: &Child, name: &str) -> Result<(), String> {
    let Some(theirs) = child_processors(child) else {
        return Ok(());
    };
    let ours = current_processors();
    if theirs == ours {
        return Ok(());
    }
    Err(format!(
        "{name} is running on processors {} and this program on {}, so the two arms \
         would be measured on different cores",
        mask_of(&theirs),
        mask_of(&ours)
    ))
}

/// Starts a child and checks that it runs on the same processors as this process.
///
/// The launcher every gate uses for the reference arm. On a mismatch the child
/// is killed and reaped before the error is returned, so a refusal leaves no
/// process behind.
///
/// @param command - the command to start
/// @param name - what the child is, for the message
pub fn spawn_on_same_cores(command: &mut Command, name: &str) -> Result<Child, String> {
    let mut child = command
        .spawn()
        .map_err(|error| format!("{name} did not start: {error}"))?;
    if let Err(reason) = confirm_child(&child, name) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(reason);
    }
    Ok(child)
}

/// Renders a set of logical processors as a hexadecimal mask.
///
/// Built from 64 bit words so a machine past 64 processors still renders.
///
/// @param processors - logical processor numbers
pub fn mask_of(processors: &[usize]) -> String {
    let Some(highest) = processors.iter().max() else {
        return "0x0".to_string();
    };
    let mut words = vec![0u64; highest / 64 + 1];
    for index in processors {
        if let Some(word) = words.get_mut(index / 64) {
            *word |= 1u64 << (index % 64);
        }
    }
    let mut text = String::from("0x");
    for (at, word) in words.iter().rev().enumerate() {
        if at == 0 {
            text.push_str(&format!("{word:X}"));
        } else {
            text.push_str(&format!("{word:016X}"));
        }
    }
    text
}

/// Returns the processors whose bit is set in a mask word.
///
/// @param mask - the word
/// @param base - the processor number of bit 0
#[cfg(any(windows, target_os = "linux"))]
fn bits_of(mask: u64, base: usize) -> Vec<usize> {
    (0..64usize)
        .filter(|bit| mask & (1u64 << bit) != 0)
        .map(|bit| base + bit)
        .collect()
}

#[cfg(windows)]
mod platform {
    use super::{bits_of, Processor};
    use windows_sys::Win32::Foundation::HANDLE;

    /// Never read on Windows, where every machine reports its processors.
    pub const UNSUPPORTED: &str = "the operating system reported no processors";

    /// `CpuSetInformation`, the one entry type that describes a processor.
    const CPU_SET_INFORMATION: u32 = 0;

    /// Returns every logical processor with its efficiency class.
    ///
    /// **Parsed from bytes rather than through the structure**, because the
    /// entries are variable sized: each one starts with its own `Size`, and a
    /// later Windows may make them longer. The offsets are the documented
    /// layout of `SYSTEM_CPU_SET_INFORMATION`: `Size` at 0, `Type` at 4, `Id`
    /// at 8, `Group` at 12, `LogicalProcessorIndex` at 14, `EfficiencyClass`
    /// at 18.
    pub fn processors() -> Vec<Processor> {
        let bytes = cpu_set_bytes();
        let mut found = Vec::new();
        let mut at = 0usize;
        while let Some(size) = read_u32(&bytes, at) {
            let entry = bytes.get(at..at.saturating_add(size as usize));
            if let (Some(entry), Some(CPU_SET_INFORMATION)) = (entry, read_u32(&bytes, at + 4)) {
                let group = entry
                    .get(12..14)
                    .and_then(|pair| <[u8; 2]>::try_from(pair).ok())
                    .map(u16::from_le_bytes);
                let index = entry.get(14).copied();
                let class = entry.get(18).copied();
                if let (Some(group), Some(index), Some(class)) = (group, index, class) {
                    found.push(Processor {
                        index: usize::from(group) * 64 + usize::from(index),
                        rank: u64::from(class),
                    });
                }
            }
            if size == 0 {
                break;
            }
            at = at.saturating_add(size as usize);
        }
        found.sort_by_key(|each| each.index);
        found
    }

    /// Reads a little endian `u32` out of a byte buffer.
    ///
    /// @param bytes - the buffer
    /// @param at - the offset
    fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
        let slice = bytes.get(at..at.checked_add(4)?)?;
        <[u8; 4]>::try_from(slice).ok().map(u32::from_le_bytes)
    }

    /// Returns what `GetSystemCpuSetInformation` wrote, as bytes.
    fn cpu_set_bytes() -> Vec<u8> {
        use windows_sys::Win32::System::SystemInformation::GetSystemCpuSetInformation;
        let mut length = 0u32;
        // SAFETY: a null buffer with a length of zero is the documented way to
        // ask for the size needed; the call writes only `length`. It fails with
        // ERROR_INSUFFICIENT_BUFFER, which is expected and not checked.
        unsafe {
            GetSystemCpuSetInformation(
                core::ptr::null_mut(),
                0,
                &mut length,
                core::ptr::null_mut(),
                0,
            );
        }
        // Words rather than bytes so the buffer has the structure's 8 byte alignment.
        let mut words = vec![0u64; (length as usize).div_ceil(8)];
        let capacity = (words.len() * 8) as u32;
        // SAFETY: the buffer is `capacity` bytes long and 8 byte aligned, which
        // is the structure's alignment, and the call writes at most `capacity`
        // bytes into it. A null process handle asks for every processor.
        let written = unsafe {
            GetSystemCpuSetInformation(
                words.as_mut_ptr().cast(),
                capacity,
                &mut length,
                core::ptr::null_mut(),
                0,
            )
        };
        if written == 0 {
            return Vec::new();
        }
        let mut bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        bytes.truncate(length as usize);
        bytes
    }

    /// Returns this process's pseudo handle.
    fn this_process() -> HANDLE {
        // SAFETY: `GetCurrentProcess` returns a pseudo handle that needs no
        // closing and is valid for the life of the process.
        unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() }
    }

    /// Returns a child's process handle.
    ///
    /// @param child - the child
    fn child_handle(child: &std::process::Child) -> HANDLE {
        use std::os::windows::io::AsRawHandle;
        child.as_raw_handle() as HANDLE
    }

    /// Returns the processors a process may run on, in group 0.
    ///
    /// @param handle - the process
    fn of_handle(handle: HANDLE) -> Option<Vec<usize>> {
        let mut process = 0usize;
        let mut system = 0usize;
        // SAFETY: both pointers are to locals this frame owns, and the call
        // writes one `usize` through each. The handle is either the pseudo
        // handle or a child handle the `Child` still holds open.
        let ok = unsafe {
            windows_sys::Win32::System::Threading::GetProcessAffinityMask(
                handle,
                &mut process,
                &mut system,
            )
        };
        (ok != 0).then(|| bits_of(process as u64, 0))
    }

    /// Sets the processors a process may run on.
    ///
    /// @param handle - the process
    /// @param processors - logical processor numbers
    fn set_handle(handle: HANDLE, processors: &[usize]) -> Result<(), String> {
        let beyond: Vec<&usize> = processors.iter().filter(|each| **each >= 64).collect();
        if !beyond.is_empty() {
            return Err(format!(
                "processors {beyond:?} are outside processor group 0, which is the only group \
                 an affinity mask can name"
            ));
        }
        let mask = processors
            .iter()
            .fold(0usize, |mask, index| mask | (1usize << index));
        // SAFETY: the handle is the pseudo handle or a child handle opened with
        // every access right, and the mask is a plain integer. A mask naming a
        // processor the system does not have is refused, not undefined.
        let ok =
            unsafe { windows_sys::Win32::System::Threading::SetProcessAffinityMask(handle, mask) };
        if ok == 0 {
            return Err(format!(
                "SetProcessAffinityMask refused {}: {}",
                super::mask_of(processors),
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    /// Returns the processors this process may run on.
    pub fn current() -> Vec<usize> {
        of_handle(this_process()).unwrap_or_default()
    }

    /// Returns the processors a child may run on.
    ///
    /// @param child - the child
    pub fn of_child(child: &std::process::Child) -> Option<Vec<usize>> {
        of_handle(child_handle(child))
    }

    /// Confines this process to the processors named.
    ///
    /// @param processors - logical processor numbers
    pub fn set_current(processors: &[usize]) -> Result<(), String> {
        set_handle(this_process(), processors)
    }

    /// Confines a child to the processors named.
    ///
    /// @param child - the child
    /// @param processors - logical processor numbers
    pub fn set_child(child: &std::process::Child, processors: &[usize]) -> Result<(), String> {
        set_handle(child_handle(child), processors)
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{bits_of, Processor};

    /// Never read on Linux, where every machine reports its processors.
    pub const UNSUPPORTED: &str = "the kernel reported no online processors";

    /// Words in the affinity buffer: 16 of 64 bits is 1,024 CPUs, glibc's
    /// `CPU_SETSIZE`.
    const WORDS: usize = 16;

    /// Returns every online CPU ranked by capacity, or by maximum frequency.
    ///
    /// `cpu_capacity` is what the scheduler itself uses to tell a big core
    /// from a little one, and it is present on hybrid parts. Where it is
    /// absent `cpuinfo_max_freq` separates the classes on a hybrid Intel part
    /// too. A CPU with neither ranks 0, and a machine where every CPU ranks
    /// the same has one class.
    ///
    /// **Only the CPUs this process may already run on.** A container or a
    /// cpuset cgroup can list a CPU as online and still refuse to schedule on
    /// it, and `sched_setaffinity` answers EINVAL to a mask naming one.
    pub fn processors() -> Vec<Processor> {
        let online = std::fs::read_to_string("/sys/devices/system/cpu/online").unwrap_or_default();
        let allowed = current();
        let mut ranked = ranks(&online, "cpu_capacity");
        if !ranked.iter().any(|each| each.rank > 0) {
            ranked = ranks(&online, "cpufreq/cpuinfo_max_freq");
        }
        if !allowed.is_empty() {
            ranked.retain(|each| allowed.contains(&each.index));
        }
        ranked
    }

    /// Reads one sysfs value per online CPU.
    ///
    /// @param online - the kernel's list of online CPUs, `0-7,16-23`
    /// @param file - the file under `/sys/devices/system/cpu/cpuN/`
    fn ranks(online: &str, file: &str) -> Vec<Processor> {
        cpu_list(online)
            .into_iter()
            .map(|index| Processor {
                index,
                rank: std::fs::read_to_string(format!("/sys/devices/system/cpu/cpu{index}/{file}"))
                    .ok()
                    .and_then(|text| text.trim().parse().ok())
                    .unwrap_or(0),
            })
            .collect()
    }

    /// Parses the kernel's CPU list format.
    ///
    /// @param text - ranges and single numbers separated by commas
    fn cpu_list(text: &str) -> Vec<usize> {
        let mut found = Vec::new();
        for part in text.trim().split(',').filter(|part| !part.is_empty()) {
            let mut ends = part.splitn(2, '-');
            let first = ends
                .next()
                .and_then(|value| value.trim().parse::<usize>().ok());
            let last = ends
                .next()
                .and_then(|value| value.trim().parse::<usize>().ok());
            match (first, last) {
                (Some(first), Some(last)) => found.extend(first..=last),
                (Some(first), None) => found.push(first),
                _ => {}
            }
        }
        found
    }

    /// Returns the processors a process may run on.
    ///
    /// @param pid - the process, 0 for this one
    fn of_pid(pid: libc::pid_t) -> Option<Vec<usize>> {
        let mut words = [0u64; WORDS];
        // SAFETY: the buffer is `size_of_val(&words)` bytes and the kernel
        // writes at most that many. `cpu_set_t` is a bit array of the same
        // layout, so the cast names the same bytes.
        let ok = unsafe {
            libc::sched_getaffinity(
                pid,
                core::mem::size_of_val(&words),
                words.as_mut_ptr().cast::<libc::cpu_set_t>(),
            )
        };
        (ok == 0).then(|| {
            words
                .iter()
                .enumerate()
                .flat_map(|(at, word)| bits_of(*word, at * 64))
                .collect()
        })
    }

    /// Sets the processors a process may run on.
    ///
    /// @param pid - the process, 0 for this one
    /// @param processors - CPU numbers
    fn set_pid(pid: libc::pid_t, processors: &[usize]) -> Result<(), String> {
        let mut words = [0u64; WORDS];
        for index in processors {
            let word = words.get_mut(index / 64).ok_or_else(|| {
                format!("CPU {index} is past the 1,024 an affinity call can name")
            })?;
            *word |= 1u64 << (index % 64);
        }
        // SAFETY: as in `of_pid`; the kernel reads at most the buffer's size.
        let ok = unsafe {
            libc::sched_setaffinity(
                pid,
                core::mem::size_of_val(&words),
                words.as_ptr().cast::<libc::cpu_set_t>(),
            )
        };
        if ok != 0 {
            return Err(format!(
                "sched_setaffinity refused {}: {}",
                super::mask_of(processors),
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    /// Returns the processors this process may run on.
    pub fn current() -> Vec<usize> {
        of_pid(0).unwrap_or_default()
    }

    /// Returns the processors a child may run on.
    ///
    /// @param child - the child
    pub fn of_child(child: &std::process::Child) -> Option<Vec<usize>> {
        of_pid(child.id() as libc::pid_t)
    }

    /// Confines this process to the processors named.
    ///
    /// @param processors - CPU numbers
    pub fn set_current(processors: &[usize]) -> Result<(), String> {
        set_pid(0, processors)
    }

    /// Confines a child to the processors named.
    ///
    /// @param child - the child
    /// @param processors - CPU numbers
    pub fn set_child(child: &std::process::Child, processors: &[usize]) -> Result<(), String> {
        set_pid(child.id() as libc::pid_t, processors)
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use super::Processor;

    /// Why nothing is pinned here.
    pub const UNSUPPORTED: &str =
        "this operating system has no call that confines a process to a set of cores";

    /// Returns nothing: the platform does not report processor classes.
    pub fn processors() -> Vec<Processor> {
        Vec::new()
    }

    /// Returns nothing: the platform has no affinity mask to read.
    pub fn current() -> Vec<usize> {
        Vec::new()
    }

    /// Returns None: the platform has no affinity mask to read.
    ///
    /// @param _child - the child
    pub fn of_child(_child: &std::process::Child) -> Option<Vec<usize>> {
        None
    }

    /// Refuses: the platform has no affinity call.
    ///
    /// @param _processors - CPU numbers
    pub fn set_current(_processors: &[usize]) -> Result<(), String> {
        Err(UNSUPPORTED.to_string())
    }

    /// Refuses: the platform has no affinity call.
    ///
    /// @param _child - the child
    /// @param _processors - CPU numbers
    pub fn set_child(_child: &std::process::Child, _processors: &[usize]) -> Result<(), String> {
        Err(UNSUPPORTED.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_performance_cores_of_this_box_render_as_the_mask_task_2064_used() {
        assert_eq!(mask_of(&[0, 1, 10, 11, 12, 13, 22, 23]), "0xC03C03");
        assert_eq!(mask_of(&[]), "0x0");
        assert_eq!(mask_of(&[64]), "0x10000000000000000");
    }

    #[test]
    fn the_cores_flag_is_removed_and_its_absence_means_performance() {
        let mut given = vec![
            "fixture.db".to_string(),
            "--cores".into(),
            "any".into(),
            "--rounds".into(),
            "3".into(),
        ];
        assert_eq!(take_cores_flag(&mut given), Ok(CoreClass::Any));
        assert_eq!(given, vec!["fixture.db", "--rounds", "3"]);
        let mut plain = vec!["fixture.db".to_string()];
        assert_eq!(take_cores_flag(&mut plain), Ok(CoreClass::Performance));
        let mut wrong = vec!["--cores".to_string(), "fast".into()];
        assert!(take_cores_flag(&mut wrong).is_err());
    }
}
