//! The Python `ctypes` binding, run against the same conformance suite.
//!
//! Invariant: **`drivers/bindings/python/run_conformance.py` runs and every
//! case in `drivers/conformance/suite.json` passes through it.** The runner is
//! 228 lines and was called by nothing (task-1969, 5.7), while
//! `drivers/README.md` presents it as the proof that a second language can
//! implement the driver from the documents alone. A proof nobody runs is a
//! claim.
//!
//! **Why here rather than in the compat harness.** The binding loads the C ABI
//! `cdylib` this crate builds, so the thing under test is this crate's artifact,
//! and the row that selects this suite is the row a change to this crate
//! already selects.
//!
//! It needs one thing the workspace cannot build, a Python interpreter. That is
//! declared on the row as `python` and announced as a skip rather than a pass
//! when it is absent. The `cdylib` used to be a second prerequisite, `capi`,
//! because it was looked for where only a plain `cargo build` puts it. It is
//! now read from beside this test, where the `cargo test` build that produced
//! this test left it, so it is always there and its absence is a failure
//! (task-2101).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Returns the workspace root.
fn workspace_root() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path
}

/// Returns a Python interpreter, or nothing.
///
/// Both spellings, because `python3` is the name on most Linux distributions
/// and `python` is the name a Windows install and a virtual environment both
/// use.
fn interpreter() -> Option<String> {
    for name in ["python3", "python"] {
        let answered = Command::new(name).arg("--version").output();
        if answered.is_ok_and(|output| output.status.success()) {
            return Some(name.to_string());
        }
    }
    None
}

/// Returns the file name the C ABI shared library has on this platform.
fn library_name() -> &'static str {
    if cfg!(windows) {
        "inillucent_driver_capi.dll"
    } else if cfg!(target_os = "macos") {
        "libinillucent_driver_capi.dylib"
    } else {
        "libinillucent_driver_capi.so"
    }
}

/// Returns the C ABI shared library that the build of this test produced.
///
/// **Beside this test binary, in `deps`, rather than one directory up
/// (task-2101).** `cargo test` builds every crate type the manifest lists and
/// leaves them in `deps`; only a plain `cargo build` copies the `cdylib` up to
/// `target/debug`. This used to look there, so it found a library only when
/// something else had run `cargo build` first - usually `conformance`, which is
/// also what then tried to replace the file while Python had it loaded.
///
/// Beside the calling test also for the reason `inillucent_compat::cliproc`
/// gives: a release run and a coverage run each build into a directory of
/// their own, and a fixed path finds the wrong one or nothing.
fn library() -> PathBuf {
    let mut directory = std::env::current_exe().unwrap_or_default();
    directory.pop();
    let path = directory.join(library_name());
    assert!(
        path.is_file(),
        "{} is not there. `cargo test` builds it with this test, because the manifest \
         lists `cdylib`, so either that line was removed or this binary was not built by cargo",
        path.display()
    );
    path
}

/// Copies the library to a file only this run uses, and returns the copy.
///
/// **Python loads the copy, never the build output (task-2101).** Windows will
/// not remove or replace a DLL a process has loaded. While Python held the
/// build's own `inillucent_driver_capi.dll`, any cargo build that needed to
/// relink it failed with `Access is denied. (os error 5)`, and in the suite
/// beside this one that failure was reported as a missing C compiler. With a
/// copy there is no file cargo writes that this suite holds open, so the
/// conflict cannot happen whatever runs at the same time. The process id is in
/// the name so two runs of this suite do not hold each other's copy either.
///
/// @param library - the library the build produced
fn private_copy(library: &Path) -> PathBuf {
    let directory = workspace_root().join("_agent_output/capi-python");
    let _ = std::fs::create_dir_all(&directory);
    let copy = directory.join(format!("{}-{}", std::process::id(), library_name()));
    std::fs::copy(library, &copy).unwrap_or_else(|error| {
        panic!(
            "could not copy {} to {}: {error}",
            library.display(),
            copy.display()
        )
    });
    copy
}

/// Every case in the shared suite passes through the Python binding.
#[test]
fn the_python_binding_passes_the_conformance_suite() {
    let Some(python) = interpreter() else {
        inillucent_base::testing::skipping(
            "no python on PATH; install one, or see drivers/README.md",
        );
        return;
    };
    let library = private_copy(&library());

    let root = workspace_root();
    let runner = root.join("drivers/bindings/python/run_conformance.py");
    assert!(
        runner.is_file(),
        "{} is not there, and drivers/README.md points a reader at it",
        runner.display()
    );

    // The binding finds the library through this, so the suite runs against the
    // artifact this build produced rather than against one installed on the
    // machine - which would be a test of somebody else's release.
    let output = Command::new(&python)
        .arg(&runner)
        .env("INILLUCENT_DRIVER_LIB", &library)
        .current_dir(&root)
        .output()
        .unwrap_or_else(|error| panic!("{python} did not start: {error}"));
    // Python has exited, so nothing holds the copy.
    let _ = std::fs::remove_file(&library);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        output.status.success(),
        "the Python binding does not pass the suite the Rust driver passes:\n{said}"
    );
    // The runner prints how many steps it ran. A run that loaded the library,
    // found no cases and exited zero is the shape this whole review is about.
    assert!(
        said.contains("steps") || said.contains("cases"),
        "the runner exited zero without saying how much it ran:\n{said}"
    );
    assert!(
        !said.contains("FAIL"),
        "the runner exited zero and its report names a failure:\n{said}"
    );
}

/// The runner and the suite it reads are both where the documents say.
///
/// A cheap check that costs no interpreter, so a machine with no Python still
/// fails when the file `drivers/README.md` points at has been moved or
/// renamed - which is the way this pair goes stale.
#[test]
fn the_runner_and_the_suite_are_where_the_documents_say() {
    let root = workspace_root();
    for named in [
        "drivers/bindings/python/run_conformance.py",
        "drivers/bindings/python/inillucent.py",
        "drivers/conformance/suite.json",
    ] {
        assert!(
            Path::new(&root.join(named)).is_file(),
            "{named} is named by drivers/README.md and is not in this checkout"
        );
    }
}
