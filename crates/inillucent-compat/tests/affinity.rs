//! Both arms of a gate run on the same processors.
//!
//! Invariant: **a child started through the gates' launcher runs on exactly
//! the processors its parent was pinned to, and the launcher refuses a child
//! that is anywhere else.**
//!
//! task-2064 found `inillucent-fullgate` on the efficiency cores of a hybrid
//! processor and its `sqlite-bench` child on the performance cores, so every
//! paired round compared two kinds of hardware. The gates now pin themselves
//! and start the reference arm through
//! `inillucent_compat::affinity::spawn_on_same_cores`, which relies on a child
//! inheriting its parent's affinity mask. This file checks that reliance in
//! both directions: the child's mask read back from the operating system must
//! equal the parent's, and a child moved to other processors must be refused.
//!
//! **One test does all of it, in order**, because an affinity mask belongs to
//! the process and libtest runs the tests of one binary on threads of one
//! process. Two tests pinning at once would each measure the other.

use std::process::{Child, Command, Stdio};

use inillucent_compat::affinity::{self, CoreClass};

/// Set in the environment of the child this file starts, so the helper test
/// below knows it is the child rather than an ordinary run.
const CHILD: &str = "INILLUCENT_AFFINITY_CHILD";

/// The child: waits until its standard input closes, then exits.
///
/// A child that has already exited cannot be asked for its mask on every
/// platform, so the child waits for the parent to finish asking. In an
/// ordinary test run the variable is unset and this does nothing, which is
/// not a skip: it is not a test of anything unless it is the child.
#[test]
fn a_child_waits_for_its_input_to_close() {
    if std::env::var_os(CHILD).is_some() {
        let mut ignored = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut ignored);
    }
}

/// Returns a command that starts this test binary as a waiting child.
fn waiting_child() -> Command {
    let program = std::env::current_exe().expect("the test binary has a path");
    let mut command = Command::new(program);
    command
        .args([
            "a_child_waits_for_its_input_to_close",
            "--exact",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Lets a waiting child exit and reaps it.
///
/// @param child - a child started by [`waiting_child`]
fn release(mut child: Child) {
    drop(child.stdin.take());
    child
        .wait()
        .expect("the waiting child exits once its input closes");
}

#[test]
fn a_gate_child_runs_on_the_parents_processors_and_is_refused_anywhere_else() {
    let machine: Vec<usize> = affinity::pin(CoreClass::Any)
        .expect("asking for any class never pins")
        .machine
        .iter()
        .map(|each| each.index)
        .collect();
    match machine.len() {
        0 => {
            // macOS: no call confines a process to a set of cores. The gate
            // says so rather than claiming a mask, and that is what is checked.
            let placement = affinity::pin(CoreClass::Performance).expect("nothing to refuse");
            assert!(!placement.pinned);
            assert!(placement.reason.is_some(), "an unpinned run says why");
        }
        // One logical processor: a child has nowhere else to run, so the two
        // arms are on the same processor by construction and there is nothing
        // to refuse. Asking to pin must not claim otherwise.
        1 => assert!(!affinity::pin(CoreClass::Performance).expect("pins").pinned),
        _ => inherits_and_refuses(&machine, &pin_to_a_strict_subset(&machine)),
    }
}

/// Checks a launched child's mask against the parent's, then that a child on
/// other processors is refused.
///
/// @param machine - every logical processor the machine reported
/// @param ours - the processors this process is pinned to, fewer than `machine`
fn inherits_and_refuses(machine: &[usize], ours: &[usize]) {
    // The inheritance the gates rely on: started through the launcher, the
    // child is on exactly the parent's processors.
    let child = affinity::spawn_on_same_cores(&mut waiting_child(), "the waiting child")
        .expect("a child of a pinned process inherits its mask");
    let theirs = affinity::child_processors(&child).expect("the child's mask can be read");
    assert_eq!(
        affinity::mask_of(&theirs),
        affinity::mask_of(ours),
        "the child is not on the processors its parent was pinned to"
    );
    release(child);

    // The refusal: a child on other processors is what task-2064 measured,
    // and the check must name both masks rather than let it be timed.
    let elsewhere: Vec<usize> = machine
        .iter()
        .copied()
        .filter(|index| !ours.contains(index))
        .collect();
    let child = waiting_child().spawn().expect("the waiting child starts");
    affinity::move_child(&child, &elsewhere).expect("a child can be moved");
    let refused = affinity::confirm_child(&child, "the moved child");
    release(child);
    let reason = refused.expect_err("a child on other processors must be refused");
    assert!(
        reason.contains(&affinity::mask_of(&elsewhere))
            && reason.contains(&affinity::mask_of(ours)),
        "the refusal names both masks: {reason}"
    );
}

/// Pins this process to fewer processors than the machine has, and returns them.
///
/// On a hybrid processor that is the performance class, which is what a gate
/// does by default, and the mask must be exactly the processors of the highest
/// rank. On a machine with one class it is the first processor alone, so the
/// inheritance is still tested against a mask that differs from the machine's.
/// Called only on a machine with at least two logical processors.
///
/// @param machine - every logical processor the machine reported
fn pin_to_a_strict_subset(machine: &[usize]) -> Vec<usize> {
    let placement = affinity::pin(CoreClass::Performance).expect("pinning succeeds");
    if placement.pinned {
        let top = placement.machine.iter().map(|each| each.rank).max();
        let fastest: Vec<usize> = placement
            .machine
            .iter()
            .filter(|each| Some(each.rank) == top)
            .map(|each| each.index)
            .collect();
        assert_eq!(
            placement.processors, fastest,
            "the performance class is the processors with the highest rank"
        );
        assert!(placement.processors.len() < machine.len());
        placement.processors
    } else {
        let first = *machine.first().expect("the caller passed two or more");
        affinity::pin_processors(&[first]).expect("one processor can be pinned to");
        assert_eq!(affinity::current_processors(), vec![first]);
        vec![first]
    }
}
