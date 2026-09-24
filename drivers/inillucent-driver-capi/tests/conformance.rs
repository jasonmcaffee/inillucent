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
//! What this harness does is take the static library `cargo test` built beside
//! it, compile
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
//! the toolchain has an address sanitizer, the C program is built with it
//! **and so is the Rust library it links**, and the run then fails on the
//! access rather than on the value it happened to read. Set
//! `INILLUCENT_CAPI_ASAN=1` to require it: without that the sanitized build is
//! attempted and the plain one is the fallback, because a sanitizer is not
//! installed everywhere and a suite that only runs where one is installed is a
//! suite that mostly does not run.
//!
//! **Until task-2103 only the C side was instrumented, and the sanitized case
//! could not see the defects it said it guarded.** A sanitizer checks the loads
//! of code that was compiled with it. The library was a plain `cargo build`, so
//! a read of freed memory made by Rust was never checked. That was measured,
//! not assumed. On main before task-2098 (`4c40750`), `lifecycle.c` linked the
//! old way ran 10 times with exit 0, `done` and no report, while the plain
//! program in task-2094's run had died with `0xC0000005` on the same read. The
//! same program linked against a library built with `-Zsanitizer=address`
//! reported `heap-use-after-free` in `Live::is`, called from
//! `inillucent_bind_int` on a statement `inillucent_stmt_free` had released, on
//! 3 of 3 runs.
//!
//! The allocator was never the gap. MSVC's sanitizer wraps `RtlAllocateHeap`
//! and `RtlFreeHeap`, which is what Rust's `System` allocator calls, so a box
//! Rust frees is poisoned without `windows_hook_rtl_allocators`, and a C read
//! of it was reported by the old build. Only the Rust loads were unchecked.
//!
//! `-Zsanitizer` is an unstable flag, and the sanitized build turns it on for
//! the pinned compiler with `RUSTC_BOOTSTRAP=1`, set on that one cargo child
//! and nowhere else. The alternative is a nightly compiler beside the pinned
//! one, and a nightly would bring its own lints and its own code generation to
//! a build that is meant to test this one. The instrumented library goes into
//! its own target directory, so the flag never touches the ordinary build's
//! artefacts.
//!
//! **A sanitized pass is only believed after the canary is caught.**
//! `tests/c/freed_read_canary.rs` frees a box and reads it from Rust, built the
//! same way as the library. If the sanitizer does not report that read, the
//! case fails before the real program runs, because a pass after that would
//! say nothing about Rust. The canary does not show that the library itself
//! was instrumented, so the case also looks for the sanitizer's
//! `__asan_report_load` calls in the library before it runs the program.
//!
//! This has been run on Windows with MSVC. The Linux branch builds with the
//! same flags and links with `cc -fsanitize=address`, and it has not been run.

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

/// Returns the plain static library that the build of this test produced.
///
/// The **static** library rather than the dynamic one, so the program and the
/// library are one image with one sanitizer runtime. This library is not
/// instrumented, so a sanitizer linked against it checks only the loads the C
/// program makes. The sanitized case uses [`build_instrumented_library`].
///
/// **No cargo is run for it (task-2101).** This used to run
/// `cargo build -p inillucent-driver-capi`, and that build also relinks the
/// `cdylib` and copies it up to `target/debug`, which is the file
/// `python_conformance` loads into a Python process. `cargo test` and a plain
/// `cargo build` build the library with different features, so each relinked
/// after the other, and when the two suites ran together Windows refused to
/// remove a DLL Python had loaded: `failed to remove file
/// ...\debug\inillucent_driver_capi.dll ... Access is denied. (os error 5)`.
/// Both suites ran concurrently in 5 of 5 runs of the pair under
/// `inillucent-testrun --strict`, and `conformance` failed every time.
///
/// `cargo test` already builds every crate type the manifest lists when it
/// builds the library this test links, and leaves the static library in
/// `deps`, beside this test binary. That file comes from the same build as the
/// test, so it cannot be older than the test, and reading it writes nothing.
fn plain_library() -> PathBuf {
    let directory = deps_directory();
    static_library_in(&directory).unwrap_or_else(|| {
        panic!(
            "{} has no static library for this crate. `cargo test` builds it with the \
             test, because the manifest lists `staticlib`, so either that line was removed \
             or this binary was not built by cargo",
            directory.display()
        )
    })
}

/// Returns the directory this test binary is in, which is where cargo put the
/// library crate types it built for it.
fn deps_directory() -> PathBuf {
    let mut path = std::env::current_exe().unwrap_or_default();
    path.pop();
    path
}

/// Returns the static library in a directory, under either platform's name.
///
/// @param directory - the profile directory cargo built into
fn static_library_in(directory: &Path) -> Option<PathBuf> {
    ["inillucent_driver_capi.lib", "libinillucent_driver_capi.a"]
        .into_iter()
        .map(|name| directory.join(name))
        .find(|candidate| candidate.is_file())
}

/// Returns the target triple to instrument, where Rust has an address
/// sanitizer for it.
///
/// `-Zsanitizer` needs an explicit `--target`, because without one the flag
/// would also reach build scripts and procedural macros, which run inside the
/// compiler and have no sanitizer runtime to call.
fn sanitizer_target() -> Option<&'static str> {
    if cfg!(all(windows, target_arch = "x86_64", target_env = "msvc")) {
        Some("x86_64-pc-windows-msvc")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("aarch64-unknown-linux-gnu")
    } else {
        None
    }
}

/// Builds the static library with the Rust side instrumented, and returns what
/// a linker should be given.
///
/// This is what lets the sanitized case see a read of freed memory that Rust
/// makes; see the module comment for the run that showed the plain library
/// hides one. `CARGO_ENCODED_RUSTFLAGS` rather than `RUSTFLAGS` because cargo
/// reads it first, so a value inherited from the shell cannot replace the flag.
///
/// **A build that fails is a failure of the test, with cargo's output
/// (task-2101).** It used to be a skip, and under `--strict` a skip is counted
/// as a missing prerequisite, so a failed link was reported as a machine
/// without a C compiler or a sanitizer. The canary has already built with the
/// same flag by the time this runs, so the toolchain is there and a failure
/// here is about this crate. This build writes only into its own target
/// directory, which nothing else loads from.
///
/// @param target - the triple to instrument, from [`sanitizer_target`]
fn build_instrumented_library(target: &str) -> PathBuf {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let directory = instrumented_target_directory();
    let produced = Command::new(&cargo)
        .current_dir(workspace_root())
        .env("RUSTC_BOOTSTRAP", "1")
        .env("CARGO_ENCODED_RUSTFLAGS", "-Zsanitizer=address")
        .env_remove("RUSTFLAGS")
        .args(["build", "-p", "inillucent-driver-capi", "--target", target])
        .arg("--target-dir")
        .arg(&directory)
        .output()
        .unwrap_or_else(|error| panic!("{cargo} did not start: {error}"));
    assert!(
        produced.status.success(),
        "the instrumented C ABI library did not build, and {}. This is a failure and not a \
         missing sanitizer: the canary built with the same flag. Cargo said:\n{}",
        ending(produced.status.code()),
        String::from_utf8_lossy(&produced.stderr)
    );
    let profile = directory.join(target).join("debug");
    static_library_in(&profile).unwrap_or_else(|| {
        panic!(
            "cargo built the instrumented library and {} has no static library in it",
            profile.display()
        )
    })
}

/// Says whether a static library was compiled with the address sanitizer.
///
/// The canary proves the toolchain can see a read Rust makes, and says nothing
/// about whether the library build kept its flag. Instrumented code calls the
/// sanitizer's `__asan_report_load*` functions, so their names appear in the
/// library's symbol references. Measured on this crate: 0 references in the
/// plain library and about 1,710 in the instrumented one.
///
/// @param library - the static library to read
fn is_instrumented(library: &Path) -> bool {
    let needle = b"__asan_report_load";
    std::fs::read(library)
        .map(|bytes| bytes.windows(needle.len()).any(|window| window == needle))
        .unwrap_or(false)
}

/// Returns the target directory the instrumented library is built into.
///
/// Beside the ordinary one rather than inside it. A different `RUSTFLAGS`
/// makes cargo rebuild everything it touches, and sharing a directory would
/// have the plain and the instrumented builds rebuilding each other's
/// dependencies on every run.
fn instrumented_target_directory() -> PathBuf {
    let mut path = target_directory();
    path.pop();
    path.join("capi-asan")
}

/// Builds `tests/c/freed_read_canary.rs` the way the instrumented library is
/// built, and returns the static library.
///
/// `rustc` directly rather than cargo, so the canary is not a crate in the
/// workspace and cannot end up in anything that ships. It runs in the workspace
/// root so rustup picks the pinned compiler.
fn build_canary() -> Option<PathBuf> {
    let target = sanitizer_target()?;
    let out = area("freed_read_canary");
    let library = out.join(match cfg!(windows) {
        true => "freed_read_canary.lib",
        false => "libfreed_read_canary.a",
    });
    let _ = std::fs::remove_file(&library);
    let produced = Command::new("rustc")
        .current_dir(workspace_root())
        .env("RUSTC_BOOTSTRAP", "1")
        .args(["--crate-type", "staticlib", "--edition", "2021", "-g"])
        .args(["-Zsanitizer=address", "--target", target])
        .arg(crate_root().join("tests/c/freed_read_canary.rs"))
        .arg("-o")
        .arg(&library)
        .output()
        .ok()?;
    if !produced.status.success() {
        eprintln!(
            "the freed read canary did not build:\n{}",
            String::from_utf8_lossy(&produced.stderr)
        );
        return None;
    }
    library.is_file().then_some(library)
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

/// Compiles a C program from `tests/c` against the header and links it to a
/// static library.
///
/// @param program - the C file's name without `.c`
/// @param name - what to call the build, which names its directory
/// @param library - the static library
/// @param sanitized - whether to ask for an address sanitizer
fn compile(program: &str, name: &str, library: &Path, sanitized: bool) -> Option<PathBuf> {
    let out = area(name);
    let source = crate_root().join(format!("tests/c/{program}.c"));
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

/// Describes how the program ended, for a failure message.
///
/// **The exit status is what says why a run stopped early (task-2098).** A run
/// that stopped after `ok close` was first read as a program that printed
/// nothing and failed to load, because the message had no exit status in it.
/// It was an access violation, `0xC0000005`, which Windows reports as a
/// negative `i32`, so the status is printed in hex as well.
///
/// @param code - the exit status, when there was one
fn ending(code: Option<i32>) -> String {
    match code {
        None => "it was ended by a signal and has no exit status".to_string(),
        Some(code) => {
            let hex = format!("{:#010X}", code as u32);
            let meaning = match code as u32 {
                0xC000_0005 => " (an access violation: a read or write of memory it did not own)",
                0xC000_0135 => " (a DLL it links was not found)",
                0xC000_0409 => " (a stack buffer overrun, or a Rust abort)",
                0xC000_00FD => " (a stack overflow)",
                _ => "",
            };
            format!("it exited {code}, {hex}{meaning}")
        }
    }
}

/// Reports what the program printed, failing on any refusal.
///
/// @param printed - everything the program wrote
/// @param code - the exit status, when it had one
/// @param how - which build produced it, for the message
fn judge(printed: &str, code: Option<i32>, how: &str) {
    assert!(
        !printed.contains("ERROR: AddressSanitizer"),
        "{how}: the address sanitizer reported an access, and {}. It printed:\n{printed}",
        ending(code)
    );
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
        "{how}: the C conformance program did not reach the end, and {}. It printed:\n{printed}",
        ending(code)
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
        "{how}: the program printed no failure and still {}, which is what an \
         address sanitizer does when it finds something after the last check",
        ending(code)
    );
}

/// Every exported call, from C, in every destruction order.
#[test]
fn the_c_conformance_program_passes() {
    let library = plain_library();
    let Some(exe) = compile("lifecycle", "lifecycle", &library, false) else {
        inillucent_base::testing::skipping("no usable C compiler");
        return;
    };
    let (printed, code) = run(&exe, "lifecycle");
    print!("{printed}");
    judge(&printed, code, "plain");
}

/// Skips the sanitized case, or fails it when `INILLUCENT_CAPI_ASAN` asks for
/// the sanitizer to be required.
///
/// @param required - whether `INILLUCENT_CAPI_ASAN` is set
/// @param why - what could not be built
fn sanitizer_unavailable(required: bool, why: &str) {
    assert!(!required, "INILLUCENT_CAPI_ASAN is set and {why}");
    inillucent_base::testing::skipping(why);
}

/// Runs the canary and fails unless the sanitizer reported its Rust side read
/// of freed memory.
///
/// @param printed - everything the canary program wrote
/// @param code - its exit status
fn judge_canary(printed: &str, code: Option<i32>) {
    assert!(
        printed.contains("ERROR: AddressSanitizer: heap-use-after-free")
            && printed.contains("canary_read"),
        "the sanitized build did not report the canary's read of freed memory in Rust, so \
         a sanitized pass would say nothing about the Rust side. The canary {}. It printed:\n{printed}",
        ending(code)
    );
    assert!(
        !printed.lines().any(|line| line == "done"),
        "the canary was reported and still ran to the end, so the sanitizer is not \
         stopping on a report. It printed:\n{printed}"
    );
}

/// The same program under an address sanitizer, with the C program and the
/// Rust library both instrumented, where the toolchain has one.
///
/// This is the case meant to catch a read of freed memory that the plain run
/// passes over because the bytes are still plausible. Before task-2098, a
/// misuse check read the liveness word of a handle `Box::from_raw` had already
/// released, and every check in the plain run passed while it did that. That
/// read is made by Rust, so only an instrumented library sees it: this case
/// reported it on 3 of 3 runs of the pre-task-2098 tree, and the C only build
/// this case used before task-2103 reported nothing on 10 of 10. The canary
/// runs first and must be caught, and the library must contain the sanitizer's
/// calls, so a build that has lost the instrumentation fails here rather than
/// passing.
#[test]
fn the_c_conformance_program_passes_under_a_sanitizer() {
    let required = std::env::var("INILLUCENT_CAPI_ASAN").is_ok_and(|value| value != "0");
    let Some(target) = sanitizer_target() else {
        return sanitizer_unavailable(required, "Rust has no address sanitizer for this platform");
    };
    let Some(canary) = build_canary() else {
        return sanitizer_unavailable(
            required,
            "Rust cannot be built with an address sanitizer here",
        );
    };
    let Some(canary_exe) = compile("freed_read_canary", "freed_read_canary", &canary, true) else {
        return sanitizer_unavailable(required, "no address sanitizer in this C toolchain");
    };
    let (printed, code) = run(&canary_exe, "freed_read_canary");
    judge_canary(&printed, code);

    let library = build_instrumented_library(target);
    assert!(
        is_instrumented(&library),
        "{} has no address sanitizer calls in it, so a sanitized pass would not have \
         checked a single load the Rust side makes",
        library.display()
    );
    let Some(exe) = compile("lifecycle", "lifecycle_asan", &library, true) else {
        return sanitizer_unavailable(required, "the sanitized program did not compile");
    };
    let (printed, code) = run(&exe, "lifecycle_asan");
    print!("{printed}");
    judge(&printed, code, "sanitized");
}
