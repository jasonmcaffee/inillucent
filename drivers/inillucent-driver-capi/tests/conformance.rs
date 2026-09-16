//! The C ABI, run from C.
//!
//! Invariant: **every exported call is reached by a C caller compiled against
//! the public header**, and every destruction order the header permits is
//! performed. `abi.rs` beside this file compares the header, `drivers/abi.toml`
//! and the exported symbols; that is a check on three lists agreeing, and it
//! cannot call anything. Before this suite, 53 exported functions had five
//! structural tests and no behaviour at all - a driver could have returned the
//! wrong status from every one of them and stayed green.
//!
//! What this harness does is build the static library, compile
//! `tests/c/lifecycle.c` against `include/inillucent_driver.h`, run it, and
//! fail on any `FAIL` line or on a run that did not reach `done`. The C program
//! is where the argument is; this file is a compiler and a reader.
//!
//! **A missing C compiler is a skip, not a pass.** The suite says so on
//! standard error and returns, the same way `capi.rs` does for the pinned
//! SQLite oracle, and `inillucent-testrun --strict` is what counts a suite that
//! could not run so that a green with no toolchain cannot be mistaken for a
//! green with one.
//!
//! ## The address sanitizer
//!
//! A lifetime defect in this crate is a read of freed memory, and a read of
//! freed memory usually returns plausible bytes rather than crashing. So when
//! the toolchain has an address sanitizer, the C program is built with it and
//! the Rust side is asked for one too, and the run then fails on the access
//! rather than on the value it happened to read. Set `INILLUCENT_CAPI_ASAN=1`
//! to require it: without that the sanitized build is attempted and the plain
//! one is the fallback, because a sanitizer is not installed everywhere and a
//! suite that only runs where one is installed is a suite that mostly does not
//! run.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Returns the crate's own directory.
fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Returns the workspace root.
fn workspace_root() -> PathBuf {
    let mut path = crate_root();
    path.pop();
    path.pop();
    path
}

/// Returns the directory one variant builds and runs in.
///
/// **One per variant, because the C program makes scratch databases with
/// relative names.** The two cases below are two `#[test]`s in one binary and
/// cargo runs them at the same time; sharing a working directory had them
/// opening each other's files, which failed in a way that read as a defect in
/// the ABI.
///
/// @param variant - which build this is
fn area(variant: &str) -> PathBuf {
    let path = workspace_root()
        .join("_agent_output/capi-conformance")
        .join(variant);
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Returns the directory cargo puts this workspace's artefacts in.
fn target_directory() -> PathBuf {
    // The test binary lives in `target/<profile>/deps`, so its grandparent is
    // the profile directory the library was built into.
    let mut path = std::env::current_exe().unwrap_or_default();
    path.pop();
    path.pop();
    path
}

/// Builds the static library and returns what a linker should be given.
///
/// The **static** library rather than the dynamic one, because linking
/// statically is what makes a sanitizer see both sides of the boundary: with a
/// separately built DLL the allocator the C side is instrumented against and
/// the allocator the Rust side uses are different ones, and nothing is
/// reported.
fn build_library() -> Option<PathBuf> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = Command::new(cargo)
        .current_dir(workspace_root())
        .args(["build", "-p", "inillucent-driver-capi"])
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let directory = target_directory();
    for name in ["inillucent_driver_capi.lib", "libinillucent_driver_capi.a"] {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Returns a path written the way `cmd` wants to read it.
///
/// @param path - the path to rewrite
fn windows_path(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\")
}

/// Copies the sanitizer's runtime library beside the program that needs it.
///
/// Returns false when there is none to copy, which makes the sanitized case a
/// skip rather than a run that exits 127 with an empty standard output.
///
/// @param out - the directory the program was built into
fn place_asan_runtime(out: &Path) -> bool {
    let Some(vcvars) = vcvars() else {
        return false;
    };
    // `.../VC/Auxiliary/Build/vcvars64.bat` -> `.../VC/Tools/MSVC/<version>`
    let mut root = vcvars;
    for _ in 0..3 {
        root.pop();
    }
    let versions = root.join("Tools/MSVC");
    let Ok(entries) = std::fs::read_dir(&versions) else {
        return false;
    };
    let name = "clang_rt.asan_dynamic-x86_64.dll";
    for entry in entries.flatten() {
        let candidate = entry.path().join("bin/Hostx64/x64").join(name);
        if candidate.is_file() {
            return std::fs::copy(&candidate, out.join(name)).is_ok();
        }
    }
    false
}

/// Returns the batch file that puts a C compiler on the path, on Windows.
fn vcvars() -> Option<PathBuf> {
    for root in [
        "C:/Program Files/Microsoft Visual Studio/2022/Community",
        "C:/Program Files/Microsoft Visual Studio/2022/Professional",
        "C:/Program Files/Microsoft Visual Studio/2022/Enterprise",
        "C:/Program Files/Microsoft Visual Studio/2022/BuildTools",
    ] {
        let candidate = PathBuf::from(root).join("VC/Auxiliary/Build/vcvars64.bat");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Compiles the C program against the header and links it to the library.
///
/// @param library - the static library
/// @param sanitized - whether to ask for an address sanitizer
fn compile(library: &Path, sanitized: bool) -> Option<PathBuf> {
    let name = match sanitized {
        true => "lifecycle_asan",
        false => "lifecycle",
    };
    let out = area(name);
    let source = crate_root().join("tests/c/lifecycle.c");
    let headers = crate_root().join("include");
    let exe = out.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let _ = std::fs::remove_file(&exe);

    if cfg!(windows) {
        // A batch file rather than a command line, for the reason `capi.rs`
        // gives: `cmd /c` will not take a quoted path with forward slashes in
        // it, and a build that fails is much easier to read when the script is
        // on disk.
        let script = out.join(format!("build_{name}.bat"));
        // `/Zi` because the sanitizer says so itself: without debug info it
        // reports an address rather than a line, which is most of what makes a
        // sanitizer report worth having.
        let sanitize = match sanitized {
            true => "/fsanitize=address /Zi ",
            false => "",
        };
        let body = format!(
            "@echo off\r\ncall \"{}\" >nul\r\ncl /nologo /W3 /MD {}/I \"{}\" \"{}\" \"{}\" \
             ws2_32.lib advapi32.lib userenv.lib ntdll.lib bcrypt.lib \
             /Fe:\"{}\" /Fo:\"{}\\\\\" /link /INCREMENTAL:NO /DEBUG\r\n",
            windows_path(&vcvars()?),
            sanitize,
            windows_path(&headers),
            windows_path(&source),
            windows_path(library),
            windows_path(&exe),
            windows_path(&out)
        );
        std::fs::write(&script, body).ok()?;
        let produced = Command::new("cmd")
            .arg("/c")
            .arg(&script)
            .current_dir(&out)
            .output()
            .ok()?;
        if !produced.status.success() {
            eprintln!(
                "the C conformance program did not compile ({name}):\n{}\n{}",
                String::from_utf8_lossy(&produced.stdout),
                String::from_utf8_lossy(&produced.stderr)
            );
            return None;
        }
        if sanitized && !place_asan_runtime(&out) {
            // The program links against the sanitizer's runtime as a DLL and
            // that DLL is only on the path inside a developer command prompt.
            // A run without it exits 127 and prints nothing at all, which
            // reads exactly like a program that produced no output.
            eprintln!("the address sanitizer runtime was not found beside the compiler");
            return None;
        }
    } else {
        let mut command = Command::new("cc");
        command.arg("-O0").arg("-g").arg("-Wall");
        if sanitized {
            command
                .arg("-fsanitize=address")
                .arg("-fno-omit-frame-pointer");
        }
        command
            .arg("-I")
            .arg(&headers)
            .arg(&source)
            .arg(library)
            .arg("-lpthread")
            .arg("-ldl")
            .arg("-lm")
            .arg("-o")
            .arg(&exe);
        let produced = command.output().ok()?;
        if !produced.status.success() {
            eprintln!(
                "the C conformance program did not compile ({name}):\n{}",
                String::from_utf8_lossy(&produced.stderr)
            );
            return None;
        }
    }
    exe.is_file().then_some(exe)
}

/// Runs the compiled program in its own directory and returns what it printed.
///
/// Its own directory because the program makes scratch databases with relative
/// names, and a run that wrote them into the repository root would be a run
/// that left something behind.
///
/// @param exe - the program
/// @param variant - which build this is, which names its directory
fn run(exe: &Path, variant: &str) -> (String, Option<i32>) {
    let produced = match Command::new(exe).current_dir(area(variant)).output() {
        Ok(produced) => produced,
        Err(error) => return (format!("could not run: {error}"), None),
    };
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&produced.stdout),
        String::from_utf8_lossy(&produced.stderr)
    )
    .replace("\r\n", "\n");
    (printed, produced.status.code())
}

/// Reports what the program printed, failing on any refusal.
///
/// @param printed - everything the program wrote
/// @param code - the exit status, when it had one
/// @param how - which build produced it, for the message
fn judge(printed: &str, code: Option<i32>, how: &str) {
    let failed: Vec<&str> = printed
        .lines()
        .filter(|line| line.starts_with("FAIL"))
        .collect();
    assert!(
        failed.is_empty(),
        "{how}: the C conformance program reported {} failure(s):\n{}",
        failed.len(),
        failed.join("\n")
    );
    assert!(
        printed.contains("\ndone\n")
            || printed.starts_with("done\n")
            || printed.ends_with("done\n"),
        "{how}: the C conformance program did not reach the end. It printed:\n{printed}"
    );
    let checks = printed
        .lines()
        .filter(|line| line.starts_with("ok "))
        .count();
    assert!(
        checks > 60,
        "{how}: only {checks} checks ran, which is fewer than this program contains - \
         it stopped early without saying so"
    );
    assert_eq!(
        code,
        Some(0),
        "{how}: the program printed no failure and still exited {code:?}, which is what an \
         address sanitizer does when it finds something after the last check"
    );
}

/// Every exported call, from C, in every destruction order.
#[test]
fn the_c_conformance_program_passes() {
    let Some(library) = build_library() else {
        inillucent_base::testing::skipping("the C ABI static library did not build");
        return;
    };
    let Some(exe) = compile(&library, false) else {
        inillucent_base::testing::skipping("no usable C compiler");
        return;
    };
    let (printed, code) = run(&exe, "lifecycle");
    print!("{printed}");
    judge(&printed, code, "plain");
}

/// The same program under an address sanitizer, where the toolchain has one.
///
/// This is the case that would have caught the defect it guards: a statement
/// stepped after its connection was freed read a `*const inillucent_conn` that
/// `Box::from_raw` had already released, and the bytes at that address were
/// still plausible. Every check in the plain run passed while it did that.
#[test]
fn the_c_conformance_program_passes_under_a_sanitizer() {
    let required = std::env::var("INILLUCENT_CAPI_ASAN").is_ok_and(|value| value != "0");
    let Some(library) = build_library() else {
        inillucent_base::testing::skipping("the C ABI static library did not build");
        return;
    };
    let Some(exe) = compile(&library, true) else {
        assert!(
            !required,
            "INILLUCENT_CAPI_ASAN is set and the sanitized build did not compile"
        );
        inillucent_base::testing::skipping("no address sanitizer in this toolchain");
        return;
    };
    let (printed, code) = run(&exe, "lifecycle_asan");
    print!("{printed}");
    judge(&printed, code, "sanitized");
}
