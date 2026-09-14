//! What a command cost the machine: peak resident set and processor time.
//!
//! Invariant: the numbers are the **operating system's**, read off the child
//! before it is reaped. A process cannot measure its own peak reliably from
//! inside - the peak is often reached in a teardown it does not outlive - and a
//! shell's `time` reports the clock and nothing else. This spawns, drains,
//! waits, and asks.
//!
//! It exists because the questions it answers are about programs this workspace
//! did not write to be measured: `inillucent-bench open` on a three-gigabyte
//! retrieval index, a shell, a build. Adding accounting to each of them would be
//! the same code in several places, and in the places where the peak is reached
//! after `main` returns it would not work at all.
//!
//! Usage:
//!   inillucent-childcost `<program>` [arguments...]

use std::process::{Command, ExitCode, Stdio};

use inillucent_compat::procstat::{child_cost, mebibytes, millis};

fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(1);
    let Some(program) = arguments.next() else {
        eprintln!("usage: inillucent-childcost <program> [arguments...]");
        return ExitCode::from(2);
    };
    let rest: Vec<String> = arguments.collect();
    let started = std::time::Instant::now();
    let spawned = Command::new(&program)
        .args(&rest)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            eprintln!("childcost: {program} did not start: {error}");
            return ExitCode::FAILURE;
        }
    };
    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => {
            eprintln!("childcost: {program} did not finish: {error}");
            return ExitCode::FAILURE;
        }
    };
    // Read while the handle is still open: a reaped process has no handle left
    // to read its accounting from.
    let cost = child_cost(&child);
    let elapsed = started.elapsed().as_secs_f64();
    println!();
    println!("## what {program} cost");
    println!("  wall clock : {elapsed:.2} s");
    println!("  peak RSS   : {:.1} MiB", mebibytes(cost.peak_working_set));
    println!("  user CPU   : {:.0} ms", millis(cost.user_nanos));
    println!("  kernel CPU : {:.0} ms", millis(cost.kernel_nanos));
    if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
