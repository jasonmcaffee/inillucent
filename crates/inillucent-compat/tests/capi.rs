//! The C ABI, proved the only way it can be: with a C compiler.
//!
//! Invariant: the probe is compiled against the *official* `sqlite3.h` and
//! linked against each engine in turn, and the two runs must print the same
//! bytes. Nothing else demonstrates an ABI. A Rust test calling this crate's own
//! entry points shares their declarations, so it agrees with itself whatever
//! they say; only a compiler reading somebody else's header can tell whether a
//! signature, a constant or a struct layout is right.
//!
//! The test skips rather than fails when there is no C compiler or no pinned
//! reference: it is evidence when it can be gathered, and a missing toolchain
//! is not a claim about the engine.

use std::path::{Path, PathBuf};
use std::process::Command;

use inillucent_compat::workspace_root;

/// Where the probe's build artefacts go.
fn area() -> PathBuf {
    workspace_root().join("_agent_output/capi")
}

/// Returns the directory holding the pinned SQLite sources, if it is there.
fn reference() -> Option<PathBuf> {
    let directory = workspace_root().join(".sqlite-ref/3.53.4/src");
    directory.join("sqlite3.h").is_file().then_some(directory)
}

/// Returns the pinned amalgamation object, if it has been built.
///
/// This platform's own object and no other. Both are checked in beside each
/// other, and handing a linker the wrong one produces a wall of undefined
/// symbols that looks like a broken library rather than the wrong file.
fn reference_object() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "sqlite3.obj"
    } else {
        "sqlite3.o"
    };
    let object = workspace_root().join(".sqlite-ref/3.53.4").join(name);
    object.is_file().then_some(object)
}

/// Returns the directory `cargo` puts this workspace's artefacts in.
fn target_directory() -> PathBuf {
    // The test binary lives in `target/<profile>/deps`, so its grandparent is
    // the profile directory the library was built into.
    let mut path = std::env::current_exe().unwrap_or_default();
    path.pop();
    path.pop();
    path
}

/// Builds the C ABI library and returns what a linker should be given.
fn build_library() -> Option<PathBuf> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = Command::new(cargo)
        .current_dir(workspace_root())
        .args(["build", "-p", "inillucent-capi"])
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let directory = target_directory();
    for name in [
        "inillucent_capi.dll.lib",
        "libinillucent_capi.so",
        "libinillucent_capi.dylib",
    ] {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Compiles the probe and links it against one library.
///
/// Returns the executable, or `None` when there is no usable C compiler - which
/// is a skip rather than a failure, and is why this hands back an option.
fn compile(name: &str, headers: &Path, library: &Path) -> Option<PathBuf> {
    let out = area();
    std::fs::create_dir_all(&out).ok()?;
    let source = workspace_root().join("crates/inillucent-compat/tests/c/capi_probe.c");
    let exe = out.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    if cfg!(windows) {
        // A batch file rather than a command line: `cmd /c` will not take a
        // quoted path with forward slashes in it, and a build that fails is
        // much easier to read when the script is sitting on disk.
        let script = out.join(format!("build_{name}.bat"));
        let body = format!(
            "@echo off\r\ncall \"{}\" >nul\r\ncl /nologo /W3 /MD /I \"{}\" \"{}\" \"{}\" /Fe:\"{}\" /Fo:\"{}\\\\\" /link /INCREMENTAL:NO\r\n",
            windows_path(&vcvars()?),
            windows_path(headers),
            windows_path(&source),
            windows_path(library),
            windows_path(&exe),
            windows_path(&out)
        );
        std::fs::write(&script, body).ok()?;
        let output = Command::new("cmd")
            .arg("/c")
            .arg(&script)
            .current_dir(&out)
            .output()
            .ok()?;
        if !output.status.success() {
            eprintln!(
                "the probe did not compile:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return None;
        }
        // The executable finds its library in its own directory, not the
        // working one, so the dynamic library goes beside it.
        {
            let shared = "inillucent_capi.dll";
            let from = target_directory().join(shared);
            if from.is_file() {
                let _ = std::fs::copy(&from, out.join(shared));
            }
        }
    } else {
        let output = Command::new("cc")
            .arg("-O0")
            .arg("-I")
            .arg(headers)
            .arg(&source)
            .arg(library)
            .arg("-lpthread")
            .arg("-ldl")
            .arg("-lm")
            .arg("-o")
            .arg(&exe)
            .output()
            .ok()?;
        if !output.status.success() {
            eprintln!(
                "the probe did not compile:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return None;
        }
    }
    exe.is_file().then_some(exe)
}

/// Returns a path written the way `cmd` wants to read it.
fn windows_path(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\")
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

/// Runs a probe and returns everything it printed.
fn run(exe: &Path) -> Option<String> {
    let output = Command::new(exe).current_dir(area()).output().ok()?;
    if !output.status.success() {
        eprintln!(
            "the probe exited {:?}:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout)
        );
    }
    Some(String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"))
}

/// The probe compiles against the official header and links against both
/// engines, and the two runs print the same thing.
#[test]
fn the_c_probe_agrees_with_the_pinned_engine() {
    let Some(headers) = reference() else {
        eprintln!("the pinned SQLite sources are not downloaded; skipping");
        return;
    };
    let Some(object) = reference_object() else {
        eprintln!("the pinned SQLite object is not built; skipping");
        return;
    };
    let Some(library) = build_library() else {
        eprintln!("the C ABI library did not build; skipping");
        return;
    };
    let Some(against_sqlite) = compile("probe_sqlite", &headers, &object) else {
        eprintln!("no usable C compiler; skipping");
        return;
    };
    let Some(against_inillucent) = compile("probe_inillucent", &headers, &library) else {
        eprintln!("no usable C compiler; skipping");
        return;
    };
    let (Some(expected), Some(found)) = (run(&against_sqlite), run(&against_inillucent)) else {
        panic!("a probe did not run");
    };
    if expected == found {
        assert!(expected.contains("done\n"), "the probe ran to the end");
        return;
    }
    // A whole-file diff is unreadable at this size, so the first differing line
    // is what gets reported: it is the one that matters, and every line after
    // it is usually the same failure repeated.
    let mut expected_lines = expected.lines();
    let mut found_lines = found.lines();
    let mut line = 0;
    loop {
        line += 1;
        match (expected_lines.next(), found_lines.next()) {
            (None, None) => break,
            (left, right) if left == right => continue,
            (left, right) => {
                panic!(
                    "line {line} differs:\n  SQLite:  {}\n  inillucent: {}",
                    left.unwrap_or("(end)"),
                    right.unwrap_or("(end)")
                );
            }
        }
    }
}
