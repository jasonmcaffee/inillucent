//! The checks that keep the engine's own rules true.
//!
//! Invariant: every rule `docs/dependency-policy.md` states about unsafe code,
//! documentation, formatting, and provenance is checked here. A rule that is
//! only written down has already been broken somewhere.
//!
//! These cover the phase 0-1 crates only. `inillucent-core` and `inillucent-bench`
//! predate the policy and are deliberately left alone, so a change that
//! touches nothing else in the retrieval engine stays verifiably minimal;
//! reformatting it would make that harder to verify rather than easier.

use std::path::{Path, PathBuf};
use std::process::Command;

use inillucent_compat::workspace_root;

/// The crates the policy applies to.
///
/// **Six crates were added in task-1932 (H9), and they were the ones where the
/// rules matter most.** The list held twenty and omitted, among others: the one
/// crate in the workspace allowed to write `unsafe`
/// (`inillucent-alloc`); the crate that parses bytes off a network socket and
/// holds 72 `unsafe` occurrences in its TLS files (`inillucent-remote`, whose
/// own module comment claimed `policy.rs` checked its `SAFETY` notes - it did
/// not, because the crate was not here); and the crate that decodes the
/// retrieval index straight off the database file, with no lint attributes at
/// all (`inillucent-core`). The two driver crates are the surface every
/// language binding reaches the engine through.
///
/// The argument for each is the one already written below for the engine
/// crates: a crate that is exempt is a crate that is exempt, and the exemption
/// is invisible from inside it.
const GOVERNED: [&str; 26] = [
    "inillucent-base",
    "inillucent-vfs",
    "inillucent-sim",
    "inillucent-value",
    // The rearchitected engine. It is held to the same standards as the
    // engine it replaces from its first commit rather than from its last: a
    // crate that is exempt while it is being written is a crate that is
    // exempt.
    "inillucent-pool",
    "inillucent-wal",
    "inillucent-tree",
    "inillucent-txn",
    "inillucent-scalar",
    "inillucent-exec",
    // The engine as a database, lifted out of `inillucent-compat`. It is
    // governed for the reason the comment above gives - a crate that is
    // exempt while it is being written is a crate that is exempt - and because
    // the code was already held to this standard while it lived inside a
    // governed crate. Leaving it out would have been a relaxation performed by
    // moving a file.
    "inillucent-engine",
    "inillucent-sqlite-reader",
    "inillucent-storage",
    "inillucent-sql",
    "inillucent-catalog",
    "inillucent-ext",
    "inillucent-search",
    "inillucent",
    "inillucent-cli",
    "inillucent-compat",
    // The six task-1932 added. See the doc comment above for why each one
    // matters more than the twenty that were already here, not less.
    "inillucent-alloc",
    "inillucent-core",
    "inillucent-remote",
    "inillucent-migrate",
    "inillucent-driver",
    "inillucent-driver-capi",
];

/// The crates whose whole point is an unsafe boundary.
///
/// `inillucent-capi` is the C ABI: every entry point takes raw pointers a C caller
/// owns, so `unsafe` is not an exception in it - it is the medium. Requiring a
/// `SAFETY:` note on each of two hundred entry points would produce two hundred
/// copies of one sentence, which is worse than useless: a reviewer would learn
/// to skip them. What is required instead is checked by
/// `every_exported_c_function_documents_itself` below.
///
/// `inillucent-capi` held this role until it was deleted with the old engine;
/// `inillucent-driver-capi`, under `drivers/` rather than `crates/`, replaced
/// it. It was never in [`GOVERNED`] - the `crates/` tree that
/// `unsafe_code_is_confined_and_justified` walks - so this list's other
/// consumer, that test's `UNSAFE_CRATES.contains` skip, has nothing to do for
/// it either; `every_exported_c_function_documents_itself` below is the one
/// that actually reads it, and reads it from `drivers/`.
const UNSAFE_CRATES: [&str; 1] = ["inillucent-driver-capi"];

/// The only files allowed to contain `unsafe`.
///
/// The first two are the operating-system boundary, which cannot be crossed in
/// safe Rust. Everything else in the engine is safe code, and the crate-level
/// `forbid(unsafe_code)` in `inillucent-base` says so to the compiler as well.
///
/// Most of the rest are measurement binaries, not the engine: a global
/// allocator is the only way to count heap allocations, and `GlobalAlloc` is an
/// unsafe trait. They are admitted here rather than quietly because the charter
/// is about what the engine is made of, and a baseline tool that never ships is
/// not part of it - but a file with `unsafe` in it should still have to say
/// why, in writing, in a list somebody reads.
///
/// The last entry is the first one that is a `cargo test` rather than a
/// binary, and it carries its own argument for why that is the same ground.
// The one file in the shell that says `unsafe`, added in task-1932 (H11).
// Ctrl+C has no representation in the standard library, so being told about
// it is `SetConsoleCtrlHandler` on Windows and `signal` on Unix, and both
// are FFI. Each call installs a handler and reads nothing back; each handler
// stores `true` into an already-allocated `AtomicBool` and returns, which is
// the whole of what a handler is allowed to do.
const UNSAFE_ALLOWED: [&str; 18] = [
    // **The AVX2 dot product, added by task-2000's design 9.** It is the one place
    // in the engine where safe Rust cannot express the thing that has to happen: a
    // 256-bit fused multiply-add is an intrinsic, every intrinsic in
    // `std::arch::x86_64` is `unsafe` because calling one on a processor that does
    // not have the feature is undefined, and there is no safe wrapper for them in
    // the standard library. The alternative is not a safe version of this kernel -
    // it is not having one, and relying on the optimiser to vectorise a scalar loop
    // it compiles to 128-bit lanes without `target-cpu`, which a published binary
    // cannot set because it has to run on the processors people have.
    //
    // The whole of the unsafety is confined to one function: `dot_wide` carries a
    // `# Safety` section naming `avx2` and `fma` as its requirement, `dot` is the
    // only caller and checks for both immediately above the call, and every block
    // inside it carries its own `SAFETY:` note about the one load or the one
    // arithmetic instruction it contains. Nothing in it allocates, frees, or holds a
    // reference past the statement it was made in.
    "crates/inillucent-core/src/distance.rs",
    "crates/inillucent-cli/src/interrupt.rs",
    // The allocator's own concurrency suite, added in task-1932 (H9). It
    // allocates on one thread and frees on another through `GlobalAlloc`, which
    // is an unsafe trait - the boundary is what the suite exists to cross, and
    // every call carries its own SAFETY note saying which thread owns the block
    // at that point.
    "crates/inillucent-alloc/tests/concurrency.rs",
    // **The allocator and the two TLS files, admitted in task-1932 (H9).**
    // None of them was here because none of their crates was in `GOVERNED`, so
    // `unsafe` in them was not permitted - it was unexamined, which is a
    // different thing and the worse one. `inillucent-remote`'s own module
    // comment said `policy.rs` checked its `SAFETY` notes; it did not, because
    // the crate was not governed.
    //
    // `inillucent-alloc` is a `GlobalAlloc`, which is an unsafe trait: it is
    // the one production crate in the workspace allowed to write the word, and
    // its whole surface is the boundary. The two TLS files are the operating
    // system's certificate stores - Windows's SChannel and the platform trust
    // roots on Unix - reached through FFI, which is the same ground
    // `inillucent-vfs`'s two files stand on.
    "crates/inillucent-alloc/src/lib.rs",
    "crates/inillucent-remote/src/tls/unix.rs",
    "crates/inillucent-remote/src/tls/windows.rs",
    "crates/inillucent-vfs/src/os/windows.rs",
    "crates/inillucent-vfs/src/os/unix.rs",
    // The local time zone, added in task-1981 and admitted here by task-1987,
    // which found it refused. It is the same operating-system boundary the two
    // files above stand on and it is reached the same way: `localtime_r` on
    // Unix and `SystemTimeToTzSpecificLocalTime` on Windows, with a zeroed
    // output structure per call. The offset between local time and UTC is not
    // something this workspace can compute - it changes at a daylight saving
    // boundary and it has changed by legislation - so there is no safe route
    // to the answer to prefer. Every call site carries its own SAFETY note.
    "crates/inillucent-vfs/src/zone.rs",
    "crates/inillucent-compat/src/bin/sqlperf.rs",
    "crates/inillucent-compat/src/bin/planperf.rs",
    // The same counting global allocator as the two profiling binaries above:
    // every method forwards to the system allocator and only adds a counter.
    "crates/inillucent-compat/src/bin/hotprofile.rs",
    // The gate's memory and processor accounting. What a
    // *process* costs is something only the operating system can say, and the
    // reference arm is a separate program this workspace cannot instrument at
    // all - so the numbers come from `GetProcessMemoryInfo`/`GetProcessTimes`
    // and `getrusage`, each of which is an FFI call and nothing else. It is
    // measurement rather than engine, which is the same ground the three above
    // stand on, and every call site carries its own SAFETY note.
    "crates/inillucent-compat/src/procstat.rs",
    // **Which processors a gate runs on (task-2085).** Both arms of a gate were
    // measured on different core classes of a hybrid processor because nothing
    // pinned them, and pinning a process is an operating system call with no
    // standard library form: `GetSystemCpuSetInformation`,
    // `GetProcessAffinityMask` and `SetProcessAffinityMask` on Windows,
    // `sched_getaffinity` and `sched_setaffinity` on Linux. Each is an FFI call
    // into a buffer the calling frame owns, and each carries its own SAFETY note.
    "crates/inillucent-compat/src/affinity.rs",
    // The allocator arm. A `GlobalAlloc` is the only way to
    // ask what the system allocator costs, and the question had to be asked:
    // the TDD expected the Linux gap to be the heap. Every path either forwards
    // to the system allocator unchanged or hands back a block obtained from it
    // for the same size class, and each one carries its own SAFETY note.
    "crates/inillucent-compat/src/bin/allocarm.rs",
    // The same counting allocator again, in the profiler that says which stage
    // of a compile allocates.
    "crates/inillucent-compat/src/bin/prepareprofile.rs",
    // The same counting allocator once more, in the profiler that says what an
    // already-prepared statement costs to *execute*. It adds one
    // thing the four above do not: the allocator captures a backtrace and
    // attributes the allocation to the frame that made it, behind a thread-local
    // reentrancy flag - a capture allocates, so an unguarded one recurses until
    // the stack runs out. Every method still forwards to the system allocator
    // unchanged and each one carries its own SAFETY note.
    "crates/inillucent-compat/src/bin/execprofile.rs",
    // **The first counting allocator in a `cargo test`, rather than in a
    // measurement binary (task-2026).** `budget.rs` guards what a compile
    // costs in allocations - the number the binder's scratch, the operator
    // listing and the result-column clone all move - and counting an
    // allocation needs a `GlobalAlloc`, which is an unsafe trait. Every method
    // forwards to the system allocator unchanged and carries its own SAFETY
    // note; the only addition is a counter.
    //
    // It differs from the five above in one way: the counter is
    // a `thread_local!`, not a global, because a `#[global_allocator]` in a
    // test binary sees every test in that binary and this file would otherwise
    // read whatever a parallel `cargo test` happened to be doing. The local is
    // `const`-initialised and holds a type with no destructor, so reading it
    // from inside the allocator cannot itself allocate and cannot recurse.
    //
    // This is a test rather than a binary, so the "a baseline tool that never
    // ships is not part of the engine" argument above does not quite cover it.
    // The narrower one does: a `tests/` file is not linked into anything an
    // application receives, and what it is measuring is precisely a cost that
    // has no safe instrument.
    "crates/inillucent/tests/budget.rs",
];

/// Returns every `.rs` file under a directory.
fn rust_files(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return found;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            found.extend(rust_files(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
    found
}

/// Returns the path relative to the workspace root, with forward slashes.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Every `unsafe` block must sit in one of the two operating-system files, and
/// each must carry a safety argument. An unsafe block with no written reason is
/// one nobody can review.
#[test]
fn unsafe_code_is_confined_and_justified() {
    let root = workspace_root();
    let mut offenders = Vec::new();
    let mut justified = 0usize;
    for crate_name in GOVERNED {
        if UNSAFE_CRATES.contains(&crate_name) {
            continue;
        }
        for file in rust_files(&crate_directory(&root, crate_name)) {
            let name = relative(&root, &file);
            // This file names the word in every check it makes.
            if name.ends_with("tests/policy.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&file).expect("the source reads");
            let lines: Vec<&str> = text.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                if !line.contains("unsafe ") && !line.contains("unsafe{") {
                    continue;
                }
                // The policy statements themselves mention the word.
                if line.trim_start().starts_with("//") || line.contains("unsafe_code") {
                    continue;
                }
                // **A function-pointer *type* is not an unsafe operation, and
                // requiring a safety argument on one asks for a sentence that
                // cannot be written (task-1932, H9).** `inillucent-remote`'s
                // TLS files resolve OpenSSL and SChannel at run time, so their
                // entry points are struct fields typed
                // `unsafe extern "C" fn(...)` - nineteen of them in
                // `tls/unix.rs` alone. The field declares what the pointer is;
                // the *call* through it is the unsafe operation, and each of
                // those does carry a note. A definition is told apart from a
                // type by having a name between `fn` and its arguments.
                if line.contains("unsafe extern \"C\" fn(") {
                    continue;
                }
                if !UNSAFE_ALLOWED.contains(&name.as_str()) {
                    offenders.push(format!("{name}:{}: {}", index + 1, line.trim()));
                    continue;
                }
                let start = index.saturating_sub(8);
                let preceding = lines.get(start..index).unwrap_or(&[]);
                // **`# Safety` counts, and it is the right form for a
                // declaration (task-1932, H9).** An unsafe *operation* carries
                // a `SAFETY:` comment saying why this call is sound; an unsafe
                // *function* carries a `# Safety` doc section saying what its
                // caller must guarantee. They are different sentences with
                // different subjects, and rustc's own
                // `clippy::missing_safety_doc` asks for the second. A check
                // that accepted only the first would push a declaration into
                // writing the wrong one.
                if preceding
                    .iter()
                    .any(|line| line.contains("SAFETY:") || line.contains("# Safety"))
                {
                    justified += 1;
                } else {
                    offenders.push(format!("{name}:{}: no SAFETY comment", index + 1));
                }
            }
        }
    }
    assert!(offenders.is_empty(), "{offenders:#?}");
    assert!(
        justified >= 5,
        "the operating-system boundary should have several justified unsafe blocks, found {justified}"
    );
}

/// Every symbol the C ABI exports must document what it does with its
/// pointers.
///
/// This is what stands in for the `SAFETY:` rule in `inillucent-capi`. An entry
/// point that takes raw pointers and says nothing about them is one a caller
/// cannot use correctly except by reading its body, which is the situation a C
/// ABI exists to avoid.
#[test]
fn every_exported_c_function_documents_itself() {
    let root = workspace_root();
    let mut offenders = Vec::new();
    let mut checked = 0usize;
    for crate_name in UNSAFE_CRATES {
        // `inillucent-driver-capi` lives under `drivers/`, not `crates/` - the
        // only entry this list has ever held that does.
        for file in rust_files(&root.join("drivers").join(crate_name)) {
            let name = relative(&root, &file);
            let text = std::fs::read_to_string(&file).expect("the source reads");
            let lines: Vec<&str> = text.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                if !line.starts_with("pub unsafe extern \"C\" fn")
                    && !line.starts_with("pub extern \"C\" fn")
                {
                    continue;
                }
                checked += 1;
                let start = index.saturating_sub(30);
                let preceding = lines.get(start..index).unwrap_or(&[]);
                let documented = preceding
                    .iter()
                    .rev()
                    .take_while(|line| {
                        line.trim_start().starts_with("///") || line.trim_start().starts_with("#[")
                    })
                    .any(|line| line.trim_start().starts_with("///"));
                let safety = !line.starts_with("pub unsafe")
                    || preceding.iter().any(|line| line.contains("# Safety"));
                if !documented {
                    offenders.push(format!("{name}:{}: no doc comment", index + 1));
                } else if !safety {
                    offenders.push(format!("{name}:{}: no `# Safety` section", index + 1));
                }
            }
        }
    }
    assert!(offenders.is_empty(), "{offenders:#?}");
    // **A floor on the scan, not a target for the ABI.** It exists so that a
    // glob which silently matched nothing cannot pass this test by finding no
    // functions to fault. Sixty was calibrated against `inillucent-capi`, which
    // exported the whole `sqlite3_*` surface; task-1911 deleted that crate and
    // the ABI that ships is `drivers/inillucent-driver-capi`, a deliberately
    // smaller surface of 53. Forty is below what the driver exports and far
    // above what a broken scan would find.
    assert!(
        checked >= 40,
        "the C ABI should export many symbols, found {checked}"
    );
}

/// Every governed crate must deny undocumented public items, so a public
/// function without a doc comment is a build error rather than a review note.
///
/// **The match is on the whole attribute, and it used to be on the lint's name
/// (task-1932, H9).** `text.contains("clippy::unwrap_used")` is true of a crate
/// that *denies* the lint and equally true of one that *allows* it - and
/// `inillucent-scalar` had a `#![cfg_attr(test, allow(clippy::expect_used,
/// clippy::indexing_slicing, clippy::panic, clippy::unwrap_used))]` block and
/// no `deny` for any of the four. It named all four lint paths, so it passed
/// this test while denying none of them, with 71 `expect`s and 22 direct index
/// expressions in `geopoly.rs`. A check that a crate can satisfy by allowing
/// the thing it is supposed to deny is the shape
/// `tests/inillucent-testing-tdd.md` rule 1.5 is about.
#[test]
fn every_governed_crate_denies_undocumented_items() {
    let root = workspace_root();
    for crate_name in GOVERNED {
        // A binary crate's root is `main.rs`; the rule is about the root, not
        // about which kind of crate it is.
        let directory = crate_directory(&root, crate_name).join("src");
        let lib = if directory.join("lib.rs").is_file() {
            directory.join("lib.rs")
        } else {
            directory.join("main.rs")
        };
        let text = std::fs::read_to_string(&lib).expect("the crate root reads");
        assert!(
            text.contains("#![deny(missing_docs)]"),
            "{crate_name} does not deny missing docs"
        );
        for lint in [
            "clippy::indexing_slicing",
            "clippy::unwrap_used",
            "clippy::expect_used",
            "clippy::panic",
        ] {
            assert!(
                text.contains(&format!("#![deny({lint})]")),
                "{crate_name} does not carry `#![deny({lint})]`. Naming the lint in a                  `cfg_attr(test, allow(...))` block is not denying it."
            );
        }
    }
}

/// Every `pub fn` in a crate whose root is `main.rs` carries a doc comment.
///
/// **`#![deny(missing_docs)]` compiles in a binary crate and reaches nothing
/// in it (task-1973).** The lint fires on items that are publicly reachable
/// from the crate root, and a binary's modules are declared `mod arm;` rather
/// than `pub mod arm;` - so nothing inside them is reachable from outside and
/// the lint has no surface to check. Measured: with the attribute on
/// `crates/inillucent-bench/src/main.rs` and twenty-seven undocumented
/// `pub fn` in the crate, the compiler reported **zero** missing-docs errors.
/// Changing one `mod metrics;` to `pub mod metrics;` made it report two
/// immediately, which is the proof that the attribute is live and its reach is
/// the problem.
///
/// So `docs/repository.md` says 29 of the 29 crates deny `missing_docs`, and in
/// the twenty-ninth this test is what the sentence is true because of. The
/// attribute stays on `main.rs`: it holds the day a module becomes `pub`, and
/// `every_governed_crate_denies_undocumented_items` above reads it.
///
/// A doc comment is `///` on the line above the signature, past any attribute
/// lines, which is the same rule the task-1969 review counted by. Anything
/// inside a `#[cfg(test)]` module is skipped, for the reason the other checks
/// here skip it: a test function's name is its description.
#[test]
fn every_public_function_in_a_binary_crate_is_documented() {
    let root = workspace_root();
    let mut undocumented: Vec<String> = Vec::new();
    let mut read = 0usize;
    let mut crates = 0usize;
    for group in ["crates", "drivers"] {
        let Ok(entries) = std::fs::read_dir(root.join(group)) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        paths.sort();
        for path in paths {
            // A crate with a library is one the lint already covers.
            if path.join("src/lib.rs").is_file() || !path.join("src/main.rs").is_file() {
                continue;
            }
            crates = crates.saturating_add(1);
            for file in rust_files(&path.join("src")) {
                let Ok(text) = std::fs::read_to_string(&file) else {
                    continue;
                };
                read = read.saturating_add(1);
                let named = relative(&root, &file);
                undocumented.extend(
                    undocumented_public_functions(&text)
                        .into_iter()
                        .map(|(at, name)| format!("{named}:{at} {name}")),
                );
            }
        }
    }
    assert!(
        crates >= 1,
        "found no crate whose root is `main.rs`, which means this is looking in the wrong place \
         rather than that the workspace has no binary crate"
    );
    assert!(
        read >= 15,
        "read {read} source files across {crates} binary crate(s), which is too few to be the \
         whole of one"
    );
    assert!(
        undocumented.is_empty(),
        "these public functions have no doc comment:\n  {}\n\
         `#![deny(missing_docs)]` does not reach them: a binary crate's modules are private, so \
         nothing in them is publicly reachable and the lint has no surface to check. This test is \
         that lint's reach, and `docs/repository.md`'s \"29 of the 29 crates deny\" is true \
         because of it.",
        undocumented.join("\n  ")
    );
}

/// Returns every `pub fn` in a file with no doc comment above it.
///
/// The line above the signature, past any attribute lines, has to start with
/// `///`. A `#[cfg(test)]` module is skipped whole: it ends at the first line
/// that is exactly the closing brace at the attribute's own indent, which is
/// how [`function_lengths`] finds the end of a body too.
///
/// @param text - the file's contents
fn undocumented_public_functions(text: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut found = Vec::new();
    let mut closing: Option<String> = None;
    for (at, line) in lines.iter().enumerate() {
        if let Some(brace) = &closing {
            if *line == *brace {
                closing = None;
            }
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("#[cfg(test)]") {
            let indent = line.len().saturating_sub(trimmed.len());
            closing = Some(format!("{}}}", " ".repeat(indent)));
            continue;
        }
        if !trimmed.starts_with("pub ") {
            continue;
        }
        let Some(name) = opens_a_function(trimmed) else {
            continue;
        };
        if declaration(&lines, at) {
            continue;
        }
        // Back past the attributes, to whatever is above the signature.
        let mut above = at;
        while above > 0 {
            let previous = lines
                .get(above.saturating_sub(1))
                .unwrap_or(&"")
                .trim_start();
            if previous.starts_with("#[") {
                above = above.saturating_sub(1);
                continue;
            }
            break;
        }
        let documented = above > 0
            && lines
                .get(above.saturating_sub(1))
                .is_some_and(|previous| previous.trim_start().starts_with("///"));
        if !documented {
            found.push((at.saturating_add(1), name));
        }
    }
    found
}

/// Returns where a crate's manifest lives.
///
/// **Two directories, because the driver crates are in `drivers/`.** Every
/// governed crate was under `crates/` until task-1932 added
/// `inillucent-driver` and `inillucent-driver-capi`, and a check that looked
/// only in `crates/` would have reported them as having no crate root rather
/// than as being ungoverned.
///
/// @param root - the workspace root
/// @param crate_name - the crate's directory name
fn crate_directory(root: &std::path::Path, crate_name: &str) -> std::path::PathBuf {
    let under_crates = root.join("crates").join(crate_name);
    if under_crates.is_dir() {
        return under_crates;
    }
    root.join("drivers").join(crate_name)
}

/// Every module must open with a comment, and the first paragraph must state
/// the invariant it keeps. This is the rule that makes the codebase readable
/// six months from now, and it is cheap enough to check.
#[test]
fn every_module_states_its_invariant() {
    let root = workspace_root();
    let mut offenders = Vec::new();
    for crate_name in GOVERNED {
        for file in rust_files(&crate_directory(&root, crate_name)) {
            let name = relative(&root, &file);
            let text = std::fs::read_to_string(&file).expect("the source reads");
            if !text.starts_with("//!") {
                offenders.push(format!("{name}: no module comment"));
                continue;
            }
            let header: String = text
                .lines()
                .take_while(|line| line.starts_with("//!") || line.trim().is_empty())
                .collect::<Vec<&str>>()
                .join("\n");
            if !header.contains("Invariant:") {
                offenders.push(format!("{name}: the module comment states no invariant"));
            }
        }
    }
    assert!(offenders.is_empty(), "{offenders:#?}");
}

/// The governed crates must be formatted, so a diff shows what changed rather
/// than how it was wrapped.
#[test]
fn the_governed_crates_are_formatted() {
    let root = workspace_root();
    let mut command = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()));
    command.current_dir(&root).arg("fmt");
    for crate_name in GOVERNED {
        command.arg("-p").arg(crate_name);
    }
    let output = command
        .args(["--", "--check"])
        .output()
        .expect("cargo fmt runs");
    assert!(
        output.status.success(),
        "run `cargo fmt` on the governed crates:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// Nothing in the reference register may be a production dependency. The
/// register is the record of what was consulted; the moment one of its entries
/// becomes a dependency, the first-party charter is broken.
#[test]
fn no_reference_is_a_production_dependency() {
    let root = workspace_root();
    let text = std::fs::read_to_string(root.join("docs/reference-register.toml"))
        .expect("the register reads");
    assert!(text.contains("production_dependency = false"));
    assert!(
        !text.contains("production_dependency = true"),
        "a consulted reference has become a dependency"
    );
    let entries = text.matches("[[reference]]").count();
    let declarations = text
        .lines()
        .filter(|line| line.trim_start().starts_with("production_dependency"))
        .count();
    assert_eq!(
        entries, declarations,
        "every reference must declare whether it is a production dependency"
    );
    assert!(
        entries >= 8,
        "the register looks incomplete: {entries} entries"
    );
}

/// The dependency policy has to name every third-party crate the layering
/// contract allows, so the argument for each one is written down somewhere a
/// reviewer will find it.
#[test]
fn the_dependency_policy_covers_what_the_contract_allows() {
    let root = workspace_root();
    let policy =
        std::fs::read_to_string(root.join("docs/dependency-policy.md")).expect("the policy reads");
    for named in ["libc", "windows-sys", "allow-list", "first-party"] {
        assert!(
            policy.contains(named),
            "the policy does not mention `{named}`"
        );
    }
    assert!(
        policy.contains("other database engine installed"),
        "the policy has lost its ownership rule"
    );
}

/// No tracked file has a name that begins with two dashes.
///
/// **A file called `--db` is a database the command line wrote after reading
/// its own flag as a path.** `inillucent --db app.rdb ...` with the flag in the
/// wrong position creates a file literally named `--db`, plus its log segments,
/// in whatever directory the command ran in. Two of those were committed in
/// `c9f82ea` and stayed tracked through a whole release, because `.gitignore`
/// matches on `.rdb` and `.db` and a file called `--db` has no extension at all
/// for a rule to match (task-1946, H8).
///
/// `.gitignore` now carries a `--*` rule, so the next one is never offered for
/// staging. This is the half of the fix that speaks: an ignore rule does
/// nothing about a file that is *already* tracked, and only a test that reads
/// `git ls-files` can see one.
///
/// **Why the two known files do not fail this test.** They are binary output an
/// agent did not create and does not delete, so `tasks/task-1946-inillucent-code-review-round-two-tdd.md`
/// section 6 lists them for a person to remove. They are named below, and a
/// name that is no longer tracked is passed over rather than reported - unlike
/// the ratchets elsewhere in this file, which fail when a recorded row
/// disappears. Removing them is the intended end state, and a test that turned
/// red on the commit that did it would be a test arguing against its own fix.
#[test]
fn no_tracked_file_is_named_like_a_flag() {
    /// The flag-named files tracked when this check was written, for
    /// `git rm` by a person. Nothing may be added to this list.
    const AWAITING_DELETION: [&str; 2] = [
        // 131,072 bytes, magic `RDB2`: a database.
        "crates/inillucent-compat/--db",
        // 64 bytes: its first log segment.
        "crates/inillucent-compat/--db-wal.0000000001",
    ];

    let root = workspace_root();
    let listing = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["ls-files", "-z"])
        .output()
        .expect("git ls-files runs");
    assert!(
        listing.status.success(),
        "git ls-files failed: {}",
        String::from_utf8_lossy(&listing.stderr)
    );
    let tracked = String::from_utf8_lossy(&listing.stdout);

    let mut offenders: Vec<String> = Vec::new();
    for path in tracked.split('\0').filter(|entry| !entry.is_empty()) {
        let named_like_a_flag = path
            .rsplit('/')
            .next()
            .is_some_and(|name| name.starts_with("--"));
        if !named_like_a_flag {
            continue;
        }
        if AWAITING_DELETION.contains(&path) {
            continue;
        }
        offenders.push(path.to_string());
    }

    assert!(
        offenders.is_empty(),
        "these tracked files are named like a command line flag:\n{}\n\
         A file whose name begins with `--` is what a command line writes when it \
         reads its own flag as a path. Delete it, and check the command that made it.",
        offenders.join("\n")
    );
}

/// The crates the rearchitecture retires are named by a shrinking list, and
/// the list is the test.
///
/// **A ratchet rather than a rule.** Prose does not stop a new edge: the way a
/// crate acquires one is that somebody adds a line to a manifest because the
/// type they wanted lives there, and nothing says no. So the crates that may
/// still name `inillucent-storage` and `inillucent-transaction` - the pager and
/// transaction manager `inillucent-sqlite-reader` reads a SQLite file through,
/// which stay in the workspace for exactly that - are listed here by name, and
/// a crate that is not on the list fails this test the moment it grows the
/// edge.
///
/// The list only ever gets shorter. Removing a name is the work; adding one is
/// a decision somebody has to argue for in a review, which is exactly the
/// difference between this and a comment.
///
/// `inillucent-ext` was removed from this list before `inillucent-vm` was
/// deleted; it was the only crate on it that the *new* engine links, and
/// therefore the only entry that put two storage models in a shipped binary
/// rather than merely in the workspace. `inillucent-vm` itself came off the
/// list when the crate was deleted along with the rest of the old engine
/// (`inillucent-session`, `inillucent-legacy`, `inillucent-capi`) - there is no
/// longer a bytecode engine anywhere in the workspace for a new crate to grow
/// an edge to.
#[test]
fn no_new_crate_reaches_into_the_retired_engine() {
    /// The crates this ratchet still watches.
    ///
    /// `inillucent-vm` came off this list when the crate itself was deleted:
    /// `inillucent-session`, `inillucent-legacy` and `inillucent-capi` went with
    /// it, and there is no longer a bytecode engine anywhere in the workspace
    /// for a new crate to grow an edge to. What is left is the *other* half of
    /// the rearchitecture's retirement list - `inillucent-storage` and
    /// `inillucent-transaction`, which stay in the workspace because
    /// `inillucent-sqlite-reader` reads SQLite's own file format through the
    /// old pager, and migrating away from SQLite is what that reader is for.
    /// This test is not vacuous with the bytecode engine gone: it still counts
    /// every new edge to the pager and the transaction manager it retired.
    const RETIRED: [&str; 2] = ["inillucent-storage", "inillucent-transaction"];

    /// Who may still name one, and why each is still there.
    ///
    /// - the retired crates themselves, and each other;
    /// - `inillucent-catalog`, whose old-engine schema reader is the arm the new
    ///   engine's `paged` module already replaces - it goes once nothing calls
    ///   that arm any more;
    /// - `inillucent-sqlite-reader`, which reads *SQLite's* file format and uses
    ///   the old pager as the format reader it is, so removing this edge means
    ///   writing a second b-tree reader rather than deleting a dependency.
    const ALLOWED: [&str; 4] = [
        "inillucent-storage",
        "inillucent-transaction",
        "inillucent-catalog",
        "inillucent-sqlite-reader",
    ];

    let root = workspace_root();
    let mut offenders: Vec<String> = Vec::new();
    for crate_name in GOVERNED.iter().chain(
        [
            "inillucent-remote",
            "inillucent-migrate",
            "inillucent-driver",
        ]
        .iter(),
    ) {
        // The harness links both engines on purpose: comparing them is what it
        // is for, and a test-only crate cannot put an edge in a shipped binary.
        if ALLOWED.contains(crate_name) || *crate_name == "inillucent-compat" {
            continue;
        }
        let manifest = crate_directory(&root, crate_name).join("Cargo.toml");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        // Only the production section: a dev-dependency on a retired crate is a
        // test comparing the two engines, which is the thing that proves the
        // replacement works.
        let production = text
            .split("[dev-dependencies]")
            .next()
            .unwrap_or_default()
            .to_string();
        for retired in RETIRED {
            if production.contains(&format!("{retired} = ")) {
                offenders.push(format!("{crate_name} -> {retired}"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "these crates reach into the retired engine and are not on the list that may:\n{}\n\
         The list is in this test and it only gets shorter. If the edge is genuinely needed, \
         say why in a review and add the name; if it is not, the type you wanted is probably \
         behind a trait the new engine also implements.",
        offenders.join("\n")
    );
}

/// How long each module is allowed to be.
///
/// **Hoisted out of the test that reads it (task-1932).** The argument for
/// every number below is three hundred lines, which made
/// `no_module_grows_past_the_size_it_is_recorded_at` the longest function in
/// this file - reported by `no_function_grows_past_the_length_it_is_recorded_at`,
/// the ratchet the same ticket added. The argument is worth keeping and the
/// function is not the place for it, so it sits here beside
/// `FUNCTION_CEILINGS`.
/// Every module over 2,500 lines, with the ceiling it is held to.
///
/// `ImportedDatabase::import_into` - 493 lines that did seven things - was
/// moved into `inillucent-engine/src/import.rs` as seven named phases,
/// which is what took `lib.rs` from 8,415 to its number here.
///
/// The seek-union feature (`AccessPath::RowidSeekUnion` and
/// `IndexSeekUnion`, turning an `IN` list and a keyset page's disjunction
/// into seeks instead of a scan) touches four of these. `plan.rs`'s own
/// growth was cut from 652 lines to 275 by moving the union-construction
/// functions into `crates/inillucent-sql/src/plan/seek_union.rs`; the
/// rest - the enum variants themselves, and the `describe`/cost/ordering
/// match arms that must stay beside the rest of `AccessPath` - has
/// nowhere else to go. `physical.rs`, `compile.rs` and `compile_dml.rs`
/// each need one new executor arm per engine and are raised as measured,
/// with no further extraction attempted this pass.
///
/// `physical.rs` is raised by one more line for task-1900. Making a
/// registered function reachable from `ORDER BY` - which is what a semantic
/// search *is*, `ORDER BY vector_distance_cos(v, embed('...')) LIMIT k` -
/// meant handing the catalog to the space a statement's stages are viewed
/// through, at the three call sites that already hold one. That is three
/// added lines and two saved on a field comment whose claim had stopped
/// being true. The extraction the message asks for is available and is not
/// small: `literal_value` and `rowid_seek_key` would move cleanly, and they
/// are named by path from twelve call sites in `inillucent-engine`, so it is
/// a cross-crate rename in a file this ticket otherwise has no business in.
/// task-1886 needed one counter and one accessor on `ImportedDatabase` -
/// how many statements a connection has compiled, which is what the plan
/// cache guard in `crates/inillucent/tests/budget.rs` asserts on now that it
/// no longer asserts on a stopwatch. `lib.rs` was two lines under its
/// ceiling, so there was no version of that addition this test would take.
///
/// The extraction it asked for was available and was one idea: the plan
/// cache. `plan_key`, `cacheable`, `compiled`, `cached_plan_count` and the
/// new `compiled_statement_count` are all about *whether* to compile, and
/// they moved to `crates/inillucent-engine/src/plans.rs`; `compile`, which
/// is about *how*, stayed with the parser and binder plumbing it is written
/// in terms of. `lib.rs` went from 8,128 to 8,068, and its number here is
/// left where it is rather than followed down to 8,070 - a ceiling two lines
/// above the file is what sent somebody here in the first place, and this
/// test's own slack rule allows 200.
/// task-1907 is the second one, and it is the extraction this test's own
/// message named. `physical.rs` needed a catalog on the fold that produces a
/// vector index's probe vector - without one, adding an
/// `inillucent_hnsw` index to a column made the documented semantic search
/// refuse - and the doc comment explaining why is longer than the change.
/// `literal_value`, `constant_value`, `fold` and the evaluation behind them
/// are one idea, *what is constant and what is it worth*, and they moved to
/// `crates/inillucent-exec/src/constant.rs`. 6,742 back to 6,563. `physical`
/// re-exports `literal_value`, so the twelve call sites in
/// `inillucent-engine` that name it by path did not move.
/// **The `fts5/mod.rs` number went back up, and that is the record following
/// the work rather than the other way round.** It came down to 2,899 when the
/// segment layer was extracted to `fts5/segment.rs`; task-1911 then measured
/// the segment format, found it cost roughly half of `extension.fts.query`
/// for no build-side gain, and reverted it. The extraction went with the
/// feature that needed it, so the ceiling returns to what it was before
/// either existed. A number that stayed at 2,899 with nothing left to
/// extract would refuse the next ordinary addition for a reason that had
/// stopped being true.
///
/// task-1911's second extraction is `crates/inillucent-ext/src/vtab/fts5`.
/// Merging the dictionary row and the doclist row into one - `%_idx` now
/// carries the doclist where it used to carry an integer naming a `%_data`
/// row - touches every reader of both, and `mod.rs` was 80 lines under its
/// ceiling before it started. The doclist's encoding and decoding is one
/// idea and moved to `fts5/doclist.rs`, which took `mod.rs` to 2,933 and is
/// recorded here at that rather than at the 3,135 it was allowed.
///
/// task-1911 is the third. Fixing the vector index that answered zero rows
/// after a reopen needed a transaction number on the connection, a `seal`,
/// and a flush - a few lines each, in two files that were both at their
/// ceiling. What moved is one idea, *the engine's half of a vector index a
/// module owns*: `follow_vector_indexes`, `nearest_rowids`, `probe_module`
/// and `refresh_vector_indexes` out of `lib.rs`, and `create_vector_index`
/// out of `ddl.rs`, into `crates/inillucent-engine/src/vectors.rs`. They
/// name each other and nothing else names them but the write path and the
/// planner's one question. `lib.rs` 8,128 to 7,876 and `ddl.rs` 2,975 to
/// 2,806, both recorded here at their new sizes.
///
/// Closing roadmap items 13 and 15 is the fourth. Both needed a catalog
/// parameter threaded onto `RowSpace::compile` and the write-path callers
/// that reach it, so a registered function stopped refusing by name in a
/// `VALUES` row, an `UPDATE` assignment and a `RETURNING` clause - three
/// or four lines each, at a dozen call sites, in a file that was 34 lines
/// from its ceiling before any of them landed. What moved is one idea,
/// *what one `INSERT`'s row looks like before any row exists to write*:
/// `InsertPlan` and the two structs and two enums it alone uses, out of
/// `dml.rs` into `crates/inillucent-exec/src/insert_plan.rs`. It is named
/// from exactly the three functions in `dml.rs` that compile or drive it,
/// and nothing else needs to see inside it. `dml.rs` 3,274 to 3,060,
/// recorded here at its new size.
///
/// `lib.rs` also moves, by thirteen lines, and nothing there extracts.
/// `WriteView` - the write path's own view of the trees, which is what
/// `WriteTarget::catalog` hands `RowSpace::compile` - answered `None` for
/// every registered function, because `TreeCatalog::user_scalar` defaults
/// to that and nothing had ever overridden it on this type. The catalog
/// parameter item 13 threads through was therefore reaching a catalog
/// that could never resolve anything, which is the second half of why a
/// `VALUES` row calling a registered scalar kept refusing after the first
/// half was fixed. `WriteView` needed a reference to the registry and the
/// two methods that read it - thirteen lines with their comments trimmed
/// to one line each, which is as far as trimming goes without losing the
/// argument. There is no second copy of this logic anywhere in the file to
/// fold into it, so the number moves instead.
///
/// Roadmap item 3, Stage 1 - a `Cached::Select` chain reused across
/// executions rather than rebuilt every time - is the next two moves.
/// `physical.rs` needed `build_chain` split into the part that borrows the
/// catalog and the part that does not, plus `Compiled`, `Slot` and
/// `try_compile` to hold the borrow-free half with no lifetime at all.
/// Everything actually new - `Compiled`, `Slot`, `try_compile` - is one
/// idea, *a compiled chain kept with no lifetime*, and moved whole into
/// `crates/inillucent-exec/src/compiled.rs`, re-exported from `physical` so
/// no existing `physical::Slot` reference had to move with it. What did
/// not move is `build_upper`: it is 99% the body `build_chain` already had,
/// entangled with a dozen of this file's own translation and aggregate
/// helpers, and splitting *that* out as well is a second, larger
/// extraction this pass did not attempt. 6,973 down to 6,756, still short
/// of the 6,691 this row was at, and raised to match rather than chasing
/// the rest of the split for a number this test's own slack already
/// tolerates. `lib.rs`'s matching half - `execute_select_cached`, deciding
/// whether to build or reuse - moved to `plans.rs`, which already answers
/// exactly this question for the outer, per-text cache; 7,973 down to
/// 7,902, nine over 7,893, raised the same way and for the same reason.
///
/// Stage 3 is the same shape again, on the write path this time:
/// `Cached::Update`, `Delete`, `Insert`'s `SELECT` source, `VirtualUpdate`
/// and `VirtualDelete` each held their own `(Box<PhysicalPlan>,
/// Box<Prepared>)` pair with no slot, so `keys_of` rebuilt the keys query
/// on every execution the way `Cached::Select` used to. `CachedQuery` is
/// the one struct all five now carry, and the dispatch itself -
/// `execute_select_cached`'s try-the-slot, build-once, reuse-after body -
/// generalised into `plans.rs`'s `run_cached_query`, which
/// `execute_select_cached` now calls too rather than duplicating. `lib.rs`
/// 7,902 to 7,920, eighteen over; there is no second copy of `CachedQuery`
/// or `keys_of` to fold into, so the number moves again.
///
/// task-1911's delta-area work on `inillucent-tree/src/leaf.rs` and its
/// FTS5 segment-format work on `inillucent-ext/src/vtab/fts5/mod.rs` are
/// the next two, both extractions this test's own message asked for
/// rather than a raised number.
///
/// `leaf.rs`: `locate` used to ask [`LeafRef::delta_value`] once per key
/// column, redecoding a delta row from its first byte every time, and
/// moved to walking the row's cursor forward once instead - which is what
/// `delta_column_at`, `row_key_matches` and `delta_row_values` are. The
/// delta area is one idea, *rows a write staged since the page was last
/// packed*, and it moved whole into `crates/inillucent-tree/src/leaf/delta.rs`:
/// the directory (`delta_count`, `delta_start`), one row's bytes
/// (`delta_row`), and every reader that walks a row once it has them.
/// `locate`, `live` and `live_source` stay behind, because each reads the
/// sorted region and the delta area together and moving them would have
/// meant picking one of the two an arbitrary home. That left four
/// functions the sorted-region code still calls - `validate_delta` from
/// `parse`, `any_delta_extent_unchecked` from `integrity`,
/// `delta_row_values` from `live` and `live_source`, `row_key_matches`
/// from `locate` - which is the `pub(super)` this extraction cost; every
/// other moved item was already `pub`, since a method's visibility does
/// not depend on which file its `impl` block sits in, only a free
/// function's does. 5,446 down to 5,175.
///
/// `fts5/mod.rs`: the merged dictionary-and-doclist row from `mod.rs`'s
/// own earlier paragraph grew a manifest of which segments are live, and
/// `SegmentMeta` plus everything that reads or writes one -
/// `get_segment_meta`, `put_segment_meta`, `automerge_threshold`,
/// `tombstone_key`, `resolve_term`, `terms_with_prefix`,
/// `merge_live_segments` - is one idea, *the segment layer*, and moved to
/// `fts5/segment.rs` beside `doclist.rs`. `resolve_doclist` and the
/// `TermValue` it decodes moved with it: both exist only to serve
/// `resolve_term`'s per-segment read and calling them from nowhere else
/// would have left a private pair in `mod.rs` with nothing left to use
/// them. `resolve_term` and `terms_with_prefix` are named by path from
/// `expr.rs` and `vocab.rs` as `super::resolve_term` and
/// `super::terms_with_prefix`; `mod.rs` re-exports both, along with
/// everything else it still calls unqualified, so neither call site
/// changed. 3,287 down to 2,899.
///
/// task-1911's fourth extraction is `lib.rs` again, from two unrelated
/// pieces of work landing together: a fix to `total_changes()`, which read
/// `changed_ever` - one counter shared by every session a database ever
/// hands out - and so reported a fresh connection every row a *different*
/// connection had already written, and the write path's `CachedQuery`
/// generalising `Cached::Select`'s compiled-chain slot onto
/// `Update`/`Delete`/`VirtualUpdate`/`VirtualDelete`/`Insert`. Both are one
/// idea each and neither is `lib.rs`'s to keep: the session baseline moved
/// to `crates/inillucent-engine/src/session_changes.rs`, a new module,
/// because nothing else in the crate reaches for it; `CachedQuery` moved
/// to `plans.rs` beside `run_cached_query`, the method its slot exists to
/// be read by. 8,015 down to 7,969.
///
/// A later pass in the same ticket raised three rows without an
/// extraction to match, and is recorded rather than chased further: each
/// fix is a handful of lines scattered through logic already local to the
/// file, not a second copy of anything or a self-contained idea with
/// somewhere else to live. `lib.rs` 7,969 to 8,030: a module's own write
/// (`insert_into_module`, `VirtualUpdate`, `VirtualDelete`) never called
/// `record_changes`, so `changes()`/`total_changes()` stayed at zero after
/// one; `RELEASE` of a savepoint stack's own implicit transaction never
/// checked whether it had emptied the stack, so `autocommit()` stayed
/// false after the equivalent of a `COMMIT`; and `VACUUM` swaps the whole
/// `ImportedDatabase` for a freshly opened one, which was quietly zeroing
/// `changes()`/`total_changes()`/`last_insert_rowid()` along with every
/// other cell a fresh connection starts at zero. `ddl.rs` 2,810 to 2,821:
/// the `RELEASE` fix's own dispatch, and one error miscoded `SQLITE_ERROR`
/// as `SQLITE_MISUSE`. `dml.rs` 3,060 to 3,084: a sibling of `count_row`
/// for a view's `INSTEAD OF` trigger, which must never count as the
/// *outer* statement's own write - only the trigger body's nested write
/// does, and that path already counted correctly.
///
/// A third pass, same ticket, is `dml.rs` again: 3,084 to 3,122, for the
/// `Stored` enum that tells `write_one`'s caller a genuine insert from an
/// `ON CONFLICT ... DO UPDATE` resolved onto a row already there.
/// `last_insert_rowid()` moves only for the first - SQLite's rule, and one
/// this file answered wrong by feeding both into the same
/// `Changes::last_rowid` - and the enum is the seam the fix needed:
/// `write_one`'s three return points now say which happened rather than
/// handing back a bare row. Small, and not a second copy of anything to
/// fold into.
///
/// `lib.rs` 8,030 to 8,041, for the free-map checkpoint defect: eleven
/// lines threaded into `ImportedDatabase::checkpoint` so it logs and
/// stamps the free map's own pages, the same way `inillucent-txn`'s
/// `Engine::checkpoint` now does, before installing them, and so that
/// `set_log_position` reads the durable point from *after* that logging
/// rather than before it - see
/// `inillucent_txn::engine::log_free_map_pages`. The idea the extraction
/// would be *about* already moved, whole, into that shared function; what
/// is left in this file is the handful of lines that call it and keep this
/// harness's own durability order, which has nowhere else to live.
///
/// `ddl.rs` 2,821 to 2,837, for a defect the same ticket found alongside
/// the free-map one: `refresh_statistics` logged a stale catalog row's
/// rewrite under `current_txn()`, which outside a batch or a running
/// statement is a transaction number nobody ever commits unless
/// `ImportedDatabase::seal` is called - and nothing called it from a
/// checkpoint, so the rewrite sat in the log forever uncommitted and
/// unreplayable. The fix is `refresh_statistics` calling `self.seal()`
/// after its own writes, which is a doc comment and six lines; there is no
/// second copy of this logic anywhere in the file to fold into.
/// `physical.rs` 6,756 down to 6,663, the same way. `reads_a_column` gained
/// the `used.rowid` case it was missing - a join keyed on a rowid alias
/// read no outer column as far as it was concerned, so a value that only
/// exists per outer row was folded once as a statement-wide constant - and
/// the comment recording why is worth more than the eight lines it costs.
/// `run_recursive`, `distinct_rows` and `MAX_RECURSIVE_PASSES` moved whole
/// into `crates/inillucent-exec/src/recursive.rs`, which is one job with
/// one caller, rather than eight lines taken from somewhere to make a
/// number fit.
///
/// `lib.rs` 7,910 down to 7,863, again by moving rather than trimming.
/// `attach_statistics`, `statistics_rows` and `apply_statistics` went into
/// `analyze.rs`, which is where the writing half of `ANALYZE` already lives
/// and which was already calling two of them through `super::`. What pushed
/// `lib.rs` over was `journal_for`, and that stays: it is the one place
/// that decides a connection in `wal` still needs a rollback journal to
/// make a checkpoint undoable, and it belongs beside the two callers that
/// install one.
///
/// `lib.rs` 8,041 down to 7,910, and this one came *down*. The durability
/// work of task-1911 pushed it to 8,152, and the answer to that is the one
/// this list has always asked for: `OpenedFile`, `open_file`,
/// `read_checkpointed_catalog` and `resume_above_every_stamp` moved whole
/// into `crates/inillucent-engine/src/recovery.rs`, which is a coherent
/// unit - opening one file and replaying its log into it - rather than a
/// slice taken to make a number fit. Nothing in them changed in the move.
// **Three rows went up in task-1932 and one came down.** `lib.rs` and
// `ddl.rs` take the three `#![deny]` lines each was missing and the
// paragraph saying why nothing had noticed - `policy.rs` matched on the
// lint's name, which is in the `cfg_attr(test, allow(...))` block, so a
// crate could satisfy the check while allowing all four. `ddl.rs` takes
// H3's undo floor, which is the paragraph explaining why a directive is
// several writes and why nothing put the earlier ones back; `plan.rs`
// takes M6's walk of an aggregate's `FILTER` and inner `ORDER BY`. Both
// are arguments rather than code - the code in each is a handful of lines
// - and the ratchet's own rule is that an argument a later reader needs is
// not what to cut to make a number fit. `paged.rs` paid for its own
// addition with an extraction, below.
// `paged.rs` came down from 3,685 to 3,570 in task-1932, because H7's
// guard - a rowid is looked up by an integer or not at all - had to go
// somewhere and the ratchet asks for an extraction rather than a raised
// number. `KeyEncoding` and its four encode methods moved whole into
// `crates/inillucent-tree/src/keyenc.rs`: one question, how a key tuple
// becomes the bytes a tree is ordered by, rather than a slice taken to
// make a number fit. Nothing in them changed in the move, and `paged.rs`
// re-exports the type so no caller's path moved either.
// Four rows for `inillucent-vm/src/{compile,compile_dml,machine}.rs` and
// `inillucent-session/src/connection.rs` came off this list along with the
// crates that held them: a ceiling on a file that is not in the workspace
// any more is not a ratchet, it is a row nobody can act on, and the test
// below already fails loudly with "is not there any more; remove its row"
// for exactly this reason - removing them here is answering that failure
// before it happens rather than after.
// **Two rows added and one lowered in task-2006.** `pool.rs` had grown 295 lines
// past its ceiling and `paged.rs` 74, both while designs 1 and 2 of task-2000
// changed what a fold and a bulk build do, and this list's own rule is that the
// answer is an extraction. `crates/inillucent-pool/src/pool/fold.rs` took the fold
// and the meta record - how a dirty page reaches the file and how the file is made
// to account for it - and `crates/inillucent-tree/src/paged/bulk.rs` took the bulk
// build. Both are whole units with nothing changed in the move, and both get a row
// here at their post-split size, because a new file of four hundred lines with
// nothing watching it is the shape every module on this list started as. `paged.rs`
// is recorded at 2,300 from 2,450, which is what the shrunk check below asks for.
const CEILINGS: [(&str, usize); 17] = [
    ("crates/inillucent-pool/src/pool/fold.rs", 600),
    ("crates/inillucent-tree/src/paged/bulk.rs", 600),
    // **The facade's own size, which had no ratchet (task-1979, Q2).** It is
    // the harness that runs every assertion against `inillucent::{Database,
    // Connection, Value}` rather than against `inillucent-engine`, so it grows
    // whenever a suite is pointed at the facade - 39 lines when task-1962
    // measured it, 528 now. A file that grows a hundred lines a ticket with
    // nothing watching is the shape every module on this list started as.
    ("crates/inillucent-compat/src/facade.rs", 700),
    // Added at its post-split size in task-1946 (M12). It was 2,728 lines
    // holding the frame table, eviction, the journal's sync gating and the
    // swip logic together; the last three are child modules now.
    ("crates/inillucent-pool/src/pool.rs", 1_587),
    // Lowered from 7,875 in task-1932. The plan cache's value type
    // (`Cached`) and its ceiling moved to `plans.rs`, which is the module
    // whose header explains when a plan is reused - the two halves of one
    // idea were ninety lines apart in a file of nearly eight thousand.
    // Lowered again in task-1932, this time by moving the savepoint
    // boundary - `savepoint`, `rollback_to` and `release` - to `marks.rs`.
    // All three changed in this ticket, because a virtual table module now
    // hears about a savepoint and a release where before it heard about
    // neither, so the seam was where the work already was.
    // Lowered again to 7,428 in task-1932. `LearningRows`, `shape_of`,
    // `identifier_of` and `decode_row` - the applier that decides what a
    // log record means - moved whole into `recovery.rs`, which is where
    // the half that decides *which* records to replay already lives. They
    // were seven hundred lines apart in a file of nearly eight thousand.
    //
    // **Lowered to 3,300 in task-1962 (A1 step 1).** The one `impl
    // ImportedDatabase` block of 3,773 lines is ten modules under
    // `engine/`, each a run of methods that was already contiguous in it:
    // opening, the accessors, the statement path, the counters, the
    // transaction manager, the integrity walk, the function registry, the
    // write path, `EXPLAIN` rendering and the row layout. Nothing moved
    // that was not adjacent, no signature changed, and the type is still
    // one type - splitting the file and splitting the type are different
    // changes with different risks, and doing the second without the first
    // would have been one diff nobody could read.
    // **Lowered to 2,650 in task-1962 (A1 step 2).** Sixty-three fields became
    // six groups, and the six structs and the methods that touch only one of
    // them are `engine/state.rs`. A1 step 3 takes it further.
    ("crates/inillucent-engine/src/lib.rs", 1_300),
    // Lowered from 6,663 in task-1932. The window pass - `run_windowed` and
    // the seven helpers only it calls - moved whole to
    // `crates/inillucent-exec/src/windowpass.rs`, which is 575 lines this
    // file no longer holds. It is reached from one line of
    // `run_any_prepared` and nothing else here called any of it, so the
    // seam was already there.
    //
    // Lowered again to 5,709, for M7's union fix. The nine functions that
    // turn a plan's constraints into the keys and spans a cursor is
    // positioned with - `nested_key`, `point_key`, the two union key
    // builders, `range_union_bounds`, `span_bounds`, `index_affinity`,
    // `bound_value` and `with_affinity`, along with `SpanBounds` - moved
    // whole to `crates/inillucent-exec/src/physical/keys.rs`. They are one
    // question, which is what bytes a cursor is asked to find, and the
    // three places that position a cursor already reached for them
    // together. Nothing in them changed in the move, and `physical.rs`
    // re-exports `nested_key` and `SpanBounds` so no caller's path moved.
    //
    // 5,709 became 5,732 when the function ratchet below asked for the
    // union's own decision to come out of `plan_stages` - which was 375
    // lines and is now 353 - and `probes_one_entry_each` carries the
    // argument that used to sit inside it. The file is 931 lines shorter
    // than this ticket found it either way.
    // **Lowered to 400 in task-1962 (A7).** 5,708 lines doing five jobs became
    // seven modules under `physical/`, beside the `keys.rs` an earlier ticket
    // had already carved out and left alone: the catalog and the layouts, the
    // parameter set, the stages a statement is planned into, the operator
    // chain, the joins, the run and compound paths, and the expression
    // translator. What is left in `physical.rs` is the module list and the
    // re-exports, so every `physical::Params`, `physical::prepare` and
    // `physical::run` reference in the workspace resolves where it did.
    ("crates/inillucent-exec/src/physical.rs", 400),
    // task-1913 lowered this to 5,111 in the shared checkout, with this
    // note: "the ratchet asks for an extraction, so the ten items that
    // answer 'what does this name in a `WITH` stand for' are `bind/cte.rs`:
    // the two CTE types, `push_ctes`, `pop_ctes`, `find_cte`,
    // `bind_recursive_cte`, `push_recursive_self` and the three that read
    // whether a definition names itself."
    //
    // **It is back at 5,315 here until that extraction is committed
    // (task-1932).** `bind/cte.rs` and the `bind.rs` it was taken out of are
    // still uncommitted work in the checkout, and this file had to be
    // committed for task-1932's own rows - so a number describing a state
    // the repository does not hold would fail every clean checkout of it.
    // Nothing of task-1913's was reverted: only this number, and it goes
    // back to 5,111 when the extraction beside it lands.
    //
    // **Lowered to 4,968 in task-2048.** Row values are `bind/rowvalue.rs`:
    // the four that bind one - `bind_row_in`, `bind_row_against_query`,
    // `bind_row_comparison` and the `row_value_parts` they read the parse
    // arena with - and the three chains they desugar through,
    // `equality_chain`, `lexicographic_chain` and `compare_bound_rows`, which
    // sat 700 lines away at the bottom of the file and had no other caller.
    // The seam is that nothing below the binder has a row value in it:
    // `BoundExpr` has no tuple, so every spelling is rewritten into scalar
    // comparisons here and the idea ends at this module's edge.
    //
    // The ticket was filed because this row was red on `main` for four
    // commits at 5,422, and it went green on its own when task-2026's
    // allocation work happened to take 136 lines out. That is the argument
    // for extracting rather than raising: the number had moved 5,282 to
    // 5,422 and back to 5,290 in four days without anyone deciding it
    // should, and 25 lines of headroom in the file four tickets edited that
    // week is a gate that fails next on somebody who did not cause it.
    //
    // **Lowered to 4,788 in task-2088.** Which collation a comparison, a sort
    // or a grouping uses is `bind/collation.rs`: `BoundExpr::collation`,
    // `BoundExpr::explicit_collation`, `comparison_rules`,
    // `result_collation`, `apply_collation` and their tests. task-2088 and
    // task-2089 made those rules walk an expression's operands, which took
    // this file to 5,033, and the rules are one question with no other
    // business in the binder.
    //
    // **Lowered to 4,728 in task-2094.** `bind/aggregate.rs` took
    // `bind_external_call` and the new `aggregate_slot`, which is where every
    // aggregate reference is made and where it picks up its arguments'
    // explicit collation. Without the move the aggregate and window references
    // carrying a collation took this file to 4,824.
    ("crates/inillucent-sql/src/bind.rs", 4_728),
    // Its own row from the day it was split out of `bind.rs` (task-2088).
    // Lowered to 158 in task-2094, when its tests moved to
    // `bind/collation/tests.rs`; the aggregate and window rules and a test
    // for them had taken it to 319.
    ("crates/inillucent-sql/src/bind/collation.rs", 158),
    // **Lowered to 2,200 in task-1962 (A8).** 5,026 lines, the largest file
    // in the workspace, became four modules under `leaf/` beside the `delta.rs`
    // that was already there: `layout` (where a value goes in the page),
    // `encode` (building one out of rows), `read` (taking a value back out) and
    // `compare` (ordering two rows, and searching a page with that order).
    // `impl LeafRef` alone was 1,470 lines; `read` and `compare` hold half of
    // it each and reopen it, so no signature changed.
    //
    // **Lowered to 1,856 in task-2074.** The delta area gained a directory,
    // format 1's rules and a reference merge to grade `live_order` against,
    // and its tests went with it into `leaf/delta/tests.rs`: the delta area was
    // already its own module, and its hand-built pages and their tests are
    // about that module rather than about the leaf.
    ("crates/inillucent-tree/src/leaf.rs", 1_856),
    // **Lowered to 2,450 in task-1962 (A8).** The 2,124 line `impl PagedTree`
    // block became three modules under `paged/`: `descent` (root to leaf),
    // `cursor` (walking leaves between two bounds, in either direction) and
    // `skip` (the distinct prefix walk). What is left here is the tree itself:
    // its fields, its statistics, its key encoding, the extent store and the
    // integrity check.
    ("crates/inillucent-tree/src/paged.rs", 2_300),
    // **Lowered to 200 in task-1962 (A7).** 3,003 lines holding the write
    // target, the key search and the four statements became six modules under
    // `dml/`, beside the `index.rs` that was already there: `target` (where a
    // row goes and what a write counts), `keys` (finding the rows to change),
    // `insert`, `conflict`, `update` and `delete`. What is left here is the
    // module list and the re-exports.
    ("crates/inillucent-exec/src/dml.rs", 200),
    // **Lowered to 1,100 in task-1962 (A8).** 2,933 lines with `Fts5Table`'s
    // two `impl` blocks nine hundred lines apart became three modules beside
    // the five that were already there: `index` (the pending buffer, the
    // doclists and the totals), `merge` (what a write does to the index) and
    // `query` (the cursor a scan reads).
    ("crates/inillucent-ext/src/vtab/fts5/mod.rs", 1_100),
    // **Lowered to 800 in task-1962 (A1 step 1).** 2,753 lines in one `impl`
    // block became five modules under `ddl/`, beside the `reindex.rs` that was
    // already there: `catalog` (the rows a statement writes and the view the
    // binder reads), `tree` (building one for a new object), `table`, `index`
    // and `alter`. What is left here is the directive dispatcher.
    ("crates/inillucent-engine/src/ddl.rs", 800),
    // Lowered from 2,925 in task-1932. M7 added three functions for the
    // anchored `LIKE` and `GLOB` range and a walk of an equality prefix
    // ahead of an `IN` list, and the ratchet asks for an extraction rather
    // than a raised number, so two went out. `pattern_range`,
    // `anchored_prefix` and `next_prefix` are `plan/pattern.rs`: what
    // range an anchored pattern selects, and the pairing rule that decides
    // whether a range may be used at all. `comparison_collation`,
    // `collation_of`, `comparison_against_column`,
    // `comparison_against_rowid` and `mirror` are `plan/terms.rs`: reading
    // one `WHERE` term as a comparison against one column, which
    // `plan/pattern.rs` and `plan/seek_union.rs` were already reaching
    // back into `plan.rs` for.
    ("crates/inillucent-sql/src/plan.rs", 2_851),
    // 2,600 until task-1970's `cargo fmt --all` reflowed this crate, which took the file to 2,799
    // without changing what it does. Measured at the character level rather than assumed: with all
    // whitespace stripped, the only differences between `ef4b630` and the formatted file are 13
    // added commas and rebalanced braces, and every identifier removed reappears added - rustfmt
    // reordering `use` statements. The ratchet's own rule is that the numbers only go down, so this
    // one is raised deliberately and said out loud (task-1966).
    // **Lowered to 2,662 in task-1973.** Its 3,214 lines were 415 past the
    // 2,799 recorded here, from the doc comments the crate now carries and from
    // splitting `build`, `build_source`, `check` and `embed` - and the
    // ratchet's rule is that a module comes down by an extraction rather than
    // up by a raised number. Two halves came out whole, each answering a
    // question of its own and each reached from one place: `synth/check.rs`,
    // which is the whole of what `synth-check` does, and `synth/postgres.rs`,
    // which is the only part of the module that talks to a database.
    ("crates/inillucent-bench/src/synth.rs", 2_662),
    // **Its first row, added in task-1973 (the task-1969 part six review, 7.2).**
    // It is the second largest file in the workspace after
    // `inillucent-sql/src/bind.rs` and it has never had a ceiling, so every
    // addition to it since the crate was written was invisible to this test.
    // 3,048 when the review measured it, 3,430 by the time task-1973 started
    // and 3,815 after splitting `run` into the ten functions above it - the
    // structs and the doc comments a split needs are lines the file did not
    // have. Recorded at its post-split size, which is what `pool.rs` above was
    // recorded at for the same reason. **Splitting the file itself is not this
    // ticket**: the review asked for the row.
    //
    // **Lowered to 3,094 in task-1977, which is the ticket that split it.**
    // `type LaneScores` through `print_summary` was one contiguous run of 733
    // lines answering one question - turn the collected scores into the card -
    // and it is `gradeembed/card.rs` now. Nothing in there reads a cache,
    // embeds a query or times a model, which is the line the split is on. The
    // test module stayed where it is: its fixtures build a whole grading run on
    // disk and are shared by the tests of both halves, so splitting it would
    // mean two copies of them or a `#[cfg(test)]` module reaching into a
    // sibling, and either is a worse thing to keep in step than one test module
    // that imports the card by name.
    ("crates/inillucent-bench/src/gradeembed.rs", 3_094),
    // Its own row from the day it was split out of `gradeembed.rs`
    // (task-1977), so it cannot do what `gradeembed.rs` did and grow through
    // four tickets with nothing watching.
    ("crates/inillucent-bench/src/gradeembed/card.rs", 766),
];

/// No module grows past the size it is recorded at, and the record only comes
/// down.
///
/// **A ratchet, for the same reason the retired-engine one is.** The review
/// named five files of several thousand lines each and asked for the
/// responsibilities inside them to be split out. Splitting one is work; keeping
/// it split is a different problem, because the way a module gets to eight
/// thousand lines is that every individual addition to it was reasonable.
///
/// So each large module's line count is written down here and a file that grows
/// past its number fails. Adding to one of these means either extracting
/// something first or lowering somebody else's number - which is a conversation
/// in a review rather than a diff nobody notices.
///
/// **The numbers only go down.** A file that shrinks below its entry is
/// recorded at its new size in the same change, so the ceiling follows the work
/// rather than the other way round.
#[test]
fn no_module_grows_past_the_size_it_is_recorded_at() {
    let root = workspace_root();
    let mut over: Vec<String> = Vec::new();
    let mut shrunk: Vec<String> = Vec::new();
    for (named, ceiling) in CEILINGS {
        let path = root.join(named);
        let Ok(text) = std::fs::read_to_string(&path) else {
            over.push(format!("{named} is not there any more; remove its row"));
            continue;
        };
        let lines = text.lines().count();
        if lines > ceiling {
            over.push(format!("{named}: {lines} lines, past its {ceiling}"));
        }
        // 200 lines of slack, so an ordinary edit does not send somebody back
        // here to move a number by three.
        if lines.saturating_add(200) < ceiling {
            shrunk.push(format!("{named}: {lines} lines, recorded at {ceiling}"));
        }
    }
    assert!(
        over.is_empty(),
        "these modules grew past the size they are recorded at:\n{}\n\
         Extract something rather than raising the number: the way a module reaches eight \
         thousand lines is that every individual addition to it was reasonable.",
        over.join("\n")
    );
    assert!(
        shrunk.is_empty(),
        "these modules are well under the size they are recorded at, so lower the numbers in \
         this test:\n{}\n\
         The ceiling follows the work rather than the other way round.",
        shrunk.join("\n")
    );
}

/// H10 (task-1920, task-1969): every skip site goes through the one helper.
///
/// **A skip nobody can see is a suite that reports green having asserted
/// nothing, which is exactly what `--strict` exists to make visible.** Before
/// task-1932 there were three phrasings and `testrun`'s classifier held a list
/// of six substrings trying to catch them. Two of the three matched none of the
/// six: `crates/inillucent-remote/tests/transport.rs` printed `...; case
/// skipped` and the ONNX suites printed `skipping: ...`. The TLS one mattered
/// most, because that binary runs other tests too - so it was invisible to
/// `--strict` by both routes at once, and a CI image without Python's `ssl`
/// module passed the TLS verification suite without running any of it.
///
/// **The rule got stricter in task-1969 (4.6), and this check moved with it.**
/// It used to read the *message* of an `eprintln!` that preceded an early
/// return and demand the `; skipping` marker, which let a print that carried
/// the marker pass while doing only half the job: it printed the phrase, and
/// under `INILLUCENT_STRICT` it did not panic, so the case returned and
/// `--strict` counted it as a run. Twenty-four sites in nine files were in that
/// state, including the nine in `differential.rs` that shadowed the library
/// helper with a local one of the same name.
///
/// So the marker is no longer a thing a print may carry. It is what
/// `inillucent_base::testing::skipping` writes, and the check is that nothing
/// else writes it: a print followed by an early return is a skip that did not
/// go through the helper, whatever it says. The floor below counts helper call
/// sites rather than print sites, because the old floor counted the thing the
/// fix removes and would have failed the moment the fix was complete - which is
/// how this check first reported "no skip site was found at all".
#[test]
fn every_skip_site_goes_through_the_one_helper() {
    let root = workspace_root();
    let mut printed: Vec<String> = Vec::new();
    let mut through_the_helper = 0usize;
    for file in rust_sources(&root) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        for (at, line) in lines.iter().enumerate() {
            // The needle is built rather than written, so this file does not
            // itself carry the text it forbids - the same reason
            // `PRIVATE_REFERENCES` in `tools/doc-facts/check.mjs` is the one
            // place its patterns are allowed to live.
            let a_definition = format!("fn {}", "skipping(");
            if line.contains("skipping(") && !line.contains(&a_definition) {
                through_the_helper = through_the_helper.saturating_add(1);
            }
            let Some(message) = announces_its_own_skip(&lines, at) else {
                continue;
            };
            printed.push(format!(
                "{}:{}: {message}",
                file.strip_prefix(&root).unwrap_or(&file).display(),
                at.saturating_add(1)
            ));
        }
    }
    assert!(
        through_the_helper >= 40,
        "found {through_the_helper} calls to the skip helper, which means this check is \
         looking in the wrong place rather than that the workspace barely skips"
    );
    assert!(
        printed.is_empty(),
        "these skips print their own sentence instead of calling \
         `inillucent_base::testing::skipping`:\n{}\n\
         A print carries the marker `inillucent-testrun` matches and does not panic under \
         `INILLUCENT_STRICT`, so the case returns and the run counts it. The helper does \
         both. A production crate that cannot depend on this harness reaches it from its \
         own `[dev-dependencies]`, which is the edge `docs/invariants/layering.toml` \
         records for `inillucent-core`.",
        printed.join("\n")
    );
}

/// Every early return in a test says why, one way or another.
///
/// **The companion to the check above, and the one that would have found this
/// class rather than waiting for somebody to run the suite without a build
/// (task-1913).** `every_skip_site_carries_the_one_marker` reads the *message*
/// a skip prints and demands the marker. It cannot see a skip that prints
/// nothing at all, and eighteen of them were sitting in `cli_arguments.rs` and
/// `confinement.rs` written as
///
/// ```ignore
/// let Some(program) = binary("inillucent") else { return; };
/// ```
///
/// A build that did not produce the binary made all eighteen report success
/// having asserted nothing - including the ten that check a confined server
/// cannot be talked into opening a file outside its root. `--strict` could not
/// see them either, because there was no message for its classifier to read.
/// task-1944 found the same shape behind a missing fixture, where a data-loss
/// bug sat behind two durability tests that had been skipping rather than
/// passing.
///
/// The rule: a `let ... else { return; }` in a `#[test]` function announces,
/// and it may do so in any of the three ways this workspace already uses -
/// `differential::skipping` in the else-block, an `eprintln!` ending in the
/// `; skipping` marker, or an announcement inside the helper the `let` calls.
/// A `panic!` or an `assert!` counts too: a test that fails is not a test that
/// silently passed.
///
/// **It reads inline `#[cfg(test)] mod` blocks under `src/` as well now
/// (task-1946, H10).** It used to require a `tests` component in the path, so
/// every test written beside the code it tests was invisible to it - and that is
/// where the next five silent skips were: `crates/inillucent-core/src/residency.rs`
/// had a helper returning `None` through a `?` when no weights were installed,
/// and the five `#[cfg(feature = "onnx")]` tests that called it reported green
/// on a machine with no model. `every_early_return_in_a_test_says_why` could not
/// see them because of the path filter, and
/// `every_skip_site_carries_the_one_marker` could not see them because there was
/// no message to read.
///
/// **And a test-module helper that can answer `None` has to say why.** That is
/// the shape `managed_or_skip` had: not a `let ... else`, but a function whose
/// `Option` return *is* the skip. The rule is stated on the signature rather
/// than on the body, because a `?` on an `Option` is an early return with no
/// syntax of its own to look for.
#[test]
fn every_early_return_in_a_test_says_why() {
    let root = workspace_root();
    let mut silent: Vec<String> = Vec::new();
    let mut found = 0usize;
    for file in rust_sources(&root) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let under_tests = file.components().any(|part| part.as_os_str() == "tests");
        if !under_tests && !text.contains("#[cfg(test)]") {
            continue;
        }
        let lines: Vec<&str> = text.lines().collect();
        let helpers = announcing_helpers(&lines);
        for at in silent_option_helpers(&lines) {
            found = found.saturating_add(1);
            silent.push(format!(
                "{}:{}: {}",
                file.strip_prefix(&root).unwrap_or(&file).display(),
                at.saturating_add(1),
                lines.get(at).map(|line| line.trim()).unwrap_or_default()
            ));
        }
        for at in test_function_lines(&lines) {
            let Some(line) = lines.get(at) else {
                continue;
            };
            // Two shapes: a `let ... else { return; }`, and a bare `return;` or
            // `return None;` standing on its own inside the test. The second was
            // added in task-1946's H10 - `return None;` is how a helper skips
            // when its own answer is the prerequisite.
            let bare = matches!(line.trim(), "return;" | "return None;");
            if !bare && !line.trim_end().ends_with("else {") {
                continue;
            }
            let block = if bare {
                // Everything from the start of the enclosing statement is too
                // much to find; the four lines above are where an announcement
                // belongs and where every one in this workspace is.
                lines
                    .get(at.saturating_sub(4)..=at)
                    .unwrap_or_default()
                    .join("\n")
            } else {
                let block = lines
                    .get(at..at.saturating_add(8))
                    .unwrap_or_default()
                    .join("\n");
                match block.find("};") {
                    Some(end) => block.get(..end).unwrap_or(&block).to_string(),
                    None => block,
                }
            };
            if !bare && !block.lines().any(|line| line.trim() == "return;") {
                continue;
            }
            found = found.saturating_add(1);
            if announces(&block) {
                continue;
            }
            // The helper the `let` called may announce instead, which is how
            // `cli_arguments.rs` and `confinement.rs` do it: one place to get
            // right rather than one per case. Only the `let` line is read -
            // taking the comment above it as well let a helper *named* in
            // prose stand in for one that was called. For a bare `return;` the
            // `let` is one of the few lines above, so the same rule reads those.
            let call = if bare {
                block
                    .lines()
                    .map(|source| source.split("//").next().unwrap_or(""))
                    .collect::<Vec<&str>>()
                    .join("\n")
            } else {
                line.split("//").next().unwrap_or("").to_string()
            };
            if helpers
                .iter()
                .any(|name| call.contains(&format!("{name}(")))
            {
                continue;
            }
            silent.push(format!(
                "{}:{}: {}",
                file.strip_prefix(&root).unwrap_or(&file).display(),
                at.saturating_add(1),
                line.trim()
            ));
        }
    }
    assert!(
        found > 10,
        "found {found} early returns in tests, which means this is looking in the wrong place \
         rather than that no test has one"
    );
    assert!(
        silent.is_empty(),
        "these tests return early without saying why, so a missing prerequisite makes them \
         report success having run nothing:\n{}\n\
         Announce it with `differential::skipping`, which `--strict` turns into a failure, or \
         from the helper the `let` calls.",
        silent.join("\n")
    );
}

/// Returns how the braces on one line change the depth, ignoring strings and
/// comments.
///
/// **A `{` inside a string literal is not a block.** Counting `line.matches('{')`
/// made `unsafe_code_is_confined_and_justified` - which has a `format!` and a
/// message with unbalanced braces in them - look as if it ran from its own line
/// to the end of the file, so every helper below it was read as part of a test
/// (task-1946, H10). This is a scanner rather than a parser: it handles `"..."`
/// with backslash escapes, `r"..."` and `r#"..."#`, a `//` comment, and a
/// character literal, which is every shape this file's sources use.
///
/// @param line - the source line
/// @returns the net change in brace depth
fn brace_delta(line: &str) -> i32 {
    let mut depth = 0i32;
    let bytes = line.as_bytes();
    let mut at = 0usize;
    while at < bytes.len() {
        let byte = bytes.get(at).copied().unwrap_or(0);
        match byte {
            b'/' if bytes.get(at.saturating_add(1)) == Some(&b'/') => break,
            b'\'' => {
                // A character literal, or a lifetime. A lifetime has no closing
                // quote, so it is the one that must not eat the rest of the line.
                let close = line
                    .get(at.saturating_add(1)..)
                    .and_then(|rest| rest.find('\''))
                    .filter(|found| *found <= 3);
                match close {
                    Some(found) => at = at.saturating_add(found).saturating_add(2),
                    None => at = at.saturating_add(1),
                }
            }
            b'r' if matches!(bytes.get(at.saturating_add(1)), Some(&b'"') | Some(&b'#')) => {
                let hashes = line
                    .get(at.saturating_add(1)..)
                    .map(|rest| rest.chars().take_while(|c| *c == '#').count())
                    .unwrap_or(0);
                let terminator = format!("\"{}", "#".repeat(hashes));
                let body = at.saturating_add(2).saturating_add(hashes);
                match line.get(body..).and_then(|rest| rest.find(&terminator)) {
                    Some(found) => {
                        at = body.saturating_add(found).saturating_add(terminator.len());
                    }
                    None => break,
                }
            }
            b'"' => {
                let mut cursor = at.saturating_add(1);
                while cursor < bytes.len() {
                    match bytes.get(cursor).copied().unwrap_or(0) {
                        b'\\' => cursor = cursor.saturating_add(2),
                        b'"' => break,
                        _ => cursor = cursor.saturating_add(1),
                    }
                }
                at = cursor.saturating_add(1);
            }
            b'{' => {
                depth = depth.saturating_add(1);
                at = at.saturating_add(1);
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                at = at.saturating_add(1);
            }
            _ => at = at.saturating_add(1),
        }
    }
    depth
}

/// Returns the line numbers inside every `#[cfg(test)] mod ... { }` block.
///
/// **A file that merely holds one somewhere is not the same thing.** The first
/// cut of `silent_option_helpers` read every `-> Option<T>` in any file with a
/// `#[cfg(test)]` in it, which caught `inillucent-search`'s `traversal_width`
/// and `legacy_manifest` - ordinary production functions whose `None` means
/// "the configured default" and "this segment has no legacy manifest", neither
/// of which is a skip. The rule only makes sense inside the test module, so
/// that is what this finds.
///
/// @param lines - the file's lines
fn test_module_lines(lines: &[&str]) -> Vec<usize> {
    let mut inside = Vec::new();
    let mut at = 0usize;
    while at < lines.len() {
        let is_attribute = lines
            .get(at)
            .is_some_and(|line| line.trim() == "#[cfg(test)]");
        let opens_module = lines
            .get(at.saturating_add(1))
            .is_some_and(|line| line.trim_start().starts_with("mod "));
        if !is_attribute || !opens_module {
            at = at.saturating_add(1);
            continue;
        }
        let mut depth = 0i32;
        let mut opened = false;
        let mut cursor = at.saturating_add(1);
        while cursor < lines.len() {
            let Some(line) = lines.get(cursor) else {
                break;
            };
            depth = depth.saturating_add(brace_delta(line));
            if brace_delta(line) > 0 || (line.contains('{') && brace_delta(line) == 0) {
                opened = true;
            }
            inside.push(cursor);
            if opened && depth <= 0 {
                break;
            }
            cursor = cursor.saturating_add(1);
        }
        at = cursor.saturating_add(1);
    }
    inside
}

/// Returns the line of every function in an inline test module whose `Option`
/// return is a skip it does not announce.
///
/// **The shape `managed_or_skip` had (task-1946, H10).** A helper inside
/// `#[cfg(test)] mod tests` that returns `Option<T>` is answering "here is the
/// thing, or this machine has not got it" - which is a skip, and the five tests
/// that called this one reported green on a machine with no weights. There is no
/// syntax to look for, because the early return was a `?`; the rule is therefore
/// stated on the signature: a test helper that can answer `None` says why
/// somewhere in its body.
///
/// A helper that never returns `None` at all is not one of these, and neither is
/// one that panics or asserts - a test that fails is not a test that silently
/// passed.
///
/// @param lines - the file's lines
fn silent_option_helpers(lines: &[&str]) -> Vec<usize> {
    let mut found = Vec::new();
    for at in test_module_lines(lines) {
        let Some(line) = lines.get(at) else {
            continue;
        };
        let Some(_) = function_name(line) else {
            continue;
        };
        if !line.contains("-> Option<") {
            continue;
        }
        let mut body: Vec<&str> = Vec::new();
        let mut inner = 0i32;
        let mut opened = false;
        for cursor in at..lines.len() {
            let Some(source) = lines.get(cursor) else {
                break;
            };
            inner = inner.saturating_add(brace_delta(source));
            if brace_delta(source) > 0 || (source.contains('{') && brace_delta(source) == 0) {
                opened = true;
            }
            body.push(source);
            if opened && inner <= 0 {
                break;
            }
        }
        let body = body.join("\n");
        // A helper with no way to answer `None` is not a skip site at all.
        let can_refuse =
            body.contains("return None") || body.contains("?;") || body.contains("?\n");
        if !can_refuse {
            continue;
        }
        if announces(&body) {
            continue;
        }
        found.push(at);
    }
    found
}

/// Reports whether a block announces that it did not run.
///
/// **Comments do not count.** The doc comment above `binary()` in
/// `cli_arguments.rs` explains why the helper announces, so a check that read
/// the text would go on passing after somebody deleted the call it describes -
/// which is the check grading its own documentation rather than the code.
///
/// @param block - the source to read
fn announces(block: &str) -> bool {
    announces_by_saying_so(block)
        || code_of(block).any(|code| code.contains("panic!") || code.contains("assert!"))
}

/// Reports whether source says, in one of this workspace's three spellings,
/// that the work did not run.
///
/// @param block - the source to read
fn announces_by_saying_so(block: &str) -> bool {
    code_of(block).any(|code| {
        code.contains("skipping(")
            // **Qualified, because the bare name was a hole (task-1969,
            // 4.2).** `crates/inillucent-compat/tests/differential.rs` - the
            // file the differential tier is named after - defined its own
            // `announce_skip` that printed neither the marker nor the panic,
            // and this check waved through all nine of its call sites because
            // the *name* matched the library helper written for. Nine of that
            // file's ten tests passed on a fresh clone having compared nothing
            // to SQLite. Only the library function announces; a local one of
            // the same name is now a defect by itself, which
            // `no_test_file_defines_its_own_skip_helper` below refuses.
            || code.contains("differential::announce_skip")
            || code.contains("testing::skipping")
            || code.contains("; skipping")
            // A helper that announces for its caller - see [`ANNOUNCERS`].
            || ANNOUNCERS
                .iter()
                .any(|(name, _)| code.contains(&format!("{name}(")))
    })
}

/// The helpers that announce a skip on behalf of whoever called them, and the
/// file each is defined in.
///
/// **Naming them here rather than teaching the scan to follow calls across
/// crates.** Each of these has one way to answer "nothing to do" and announces
/// before it does: `differential::compare` returns zero only when
/// `start_oracle` answered `None`, after which it has already called
/// `announce_skip`. So a caller that returns early on that zero is a skip that
/// was announced by the only code that knew what was missing.
///
/// The cost of naming them is that the list can go stale - a helper could stop
/// announcing and forty call sites would silently become silent skips - and
/// [`every_helper_this_check_trusts_actually_announces`] is what pays it.
///
/// **`cliproc::program` was on this list from task-1970 until task-2106.** It
/// returned `None` and announced a skip when the build of `inillucent-cli`
/// failed, which `--strict` then reported as a missing prerequisite. It now
/// panics with cargo's output and returns the path, so its callers have no
/// early return left to account for and it announces nothing.
const ANNOUNCERS: [(&str, &str); 2] = [
    ("compare", "crates/inillucent-compat/src/differential.rs"),
    (
        "compare_queries",
        "crates/inillucent-compat/src/differential.rs",
    ),
];

/// Every helper [`ANNOUNCERS`] trusts to announce a skip does announce one.
///
/// **A list of names is a claim, and this is the test that pays for it.**
/// `announces_by_saying_so` accepts a call to any of them as an announcement,
/// so a helper that stopped calling the skip helper would turn every one of its
/// call sites into a silent skip at once - forty of them, in the case of
/// `cliproc::program` while it was on the list - and
/// `every_early_return_in_a_test_says_why` would go on passing.
/// That is the exact shape of the defect task-1969 4.2 found, one level up: a
/// check that matched a helper by name.
#[test]
fn every_helper_this_check_trusts_actually_announces() {
    let root = workspace_root();
    let mut wrong: Vec<String> = Vec::new();
    for (name, file) in ANNOUNCERS {
        let path = root.join(file);
        let Ok(text) = std::fs::read_to_string(&path) else {
            wrong.push(format!(
                "{file} is not there, and {name} is trusted to be in it"
            ));
            continue;
        };
        let Some(at) = text.find(&format!("pub fn {name}(")) else {
            wrong.push(format!("{file} does not define `{name}`"));
            continue;
        };
        // The body runs to the next closing brace at the file's own left
        // margin, which is where a free function ends.
        let rest = text.get(at..).unwrap_or_default();
        let body = match rest.find("\n}\n") {
            Some(end) => rest.get(..end).unwrap_or_default(),
            None => rest,
        };
        let announces = body.contains("skipping(")
            || body.contains("announce_skip(")
            || ANNOUNCERS
                .iter()
                .any(|(other, _)| *other != name && body.contains(&format!("{other}(")));
        if !announces {
            wrong.push(format!(
                "{file}::{name} is trusted to announce a skip for its callers and its body \
                 calls no skip helper"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{}\n\
         Every name in ANNOUNCERS is accepted as an announcement wherever it is called, so a \
         helper that stops announcing turns every one of its call sites into a silent skip.",
        wrong.join("\n")
    );
}

/// Returns each line of some source with its trailing comment removed.
///
/// @param block - the source to read
fn code_of(block: &str) -> impl Iterator<Item = &str> {
    block
        .lines()
        .map(|line| line.split("//").next().unwrap_or(""))
}

/// Returns the names of the functions in a file that announce a skip.
///
/// The body ends where its braces balance, so a function that does not
/// announce cannot borrow the announcement of the one written after it.
///
/// @param lines - the file's lines
fn announcing_helpers(lines: &[&str]) -> Vec<String> {
    let mut names = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        let Some(name) = function_name(line) else {
            continue;
        };
        let mut depth = 0i32;
        let mut opened = false;
        let mut body: Vec<&str> = Vec::new();
        for cursor in at..lines.len() {
            let Some(source) = lines.get(cursor) else {
                break;
            };
            depth = depth.saturating_add(brace_delta(source));
            if brace_delta(source) > 0 || (source.contains('{') && brace_delta(source) == 0) {
                opened = true;
            }
            body.push(source);
            if opened && depth <= 0 {
                break;
            }
        }
        // An `assert!` or a `panic!` is not an announcement when it is inside
        // a helper: nearly every helper in these files asserts something, and
        // accepting that made every `let ... else` whose right-hand side named
        // one look announced. In an `else` block it does count, because a case
        // that fails is not a case that silently passed.
        if announces_by_saying_so(&body.join("\n")) {
            names.push(name);
        }
    }
    names
}

/// Returns the name a `fn` line declares.
///
/// @param line - the source line
fn function_name(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let rest = trimmed
        .strip_prefix("fn ")
        .or_else(|| trimmed.strip_prefix("pub fn "))
        .or_else(|| trimmed.strip_prefix("pub(crate) fn "))
        .or_else(|| trimmed.strip_prefix("pub(super) fn "))?;
    let name: String = rest
        .chars()
        .take_while(|letter| letter.is_alphanumeric() || *letter == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Returns the line numbers that lie inside a `#[test]` function.
///
/// @param lines - the file's lines
fn test_function_lines(lines: &[&str]) -> Vec<usize> {
    let mut inside = Vec::new();
    let mut at = 0usize;
    while at < lines.len() {
        if lines.get(at).is_none_or(|line| line.trim() != "#[test]") {
            at = at.saturating_add(1);
            continue;
        }
        let mut depth = 0i32;
        let mut opened = false;
        let mut cursor = at;
        while cursor < lines.len() {
            let Some(line) = lines.get(cursor) else {
                break;
            };
            depth = depth.saturating_add(brace_delta(line));
            if brace_delta(line) > 0 || (line.contains('{') && brace_delta(line) == 0) {
                opened = true;
            }
            inside.push(cursor);
            if opened && depth <= 0 {
                break;
            }
            cursor = cursor.saturating_add(1);
        }
        at = cursor.saturating_add(1);
    }
    inside
}

/// Returns the message of a skip a test announces itself, or `None`.
///
/// A skip is a print whose message carries the marker followed by an early
/// return. Anything else a print says is progress or a warning, and neither is
/// a claim that a suite ran.
///
/// **Both reads used to be narrower than the code they read** (task-2066
/// §4.4.11). The message was taken off the line the macro opens on, so a
/// `rustfmt`-wrapped
///
/// ```ignore
/// eprintln!(
///     "... is not set, so no server is available to \
///      migrate; skipping. ..."
/// );
/// return None;
/// ```
///
/// read as an empty message and was passed over; and the early return was
/// matched against four exact spellings of an indented `return;`, none of which
/// is `return None;`. `live_postgres.rs` and `live_mysql.rs` were written in
/// exactly that shape, so both were invisible here and neither panicked under
/// `INILLUCENT_STRICT`. `the_guard_sees_a_wrapped_macro_that_returns_none`
/// hands this function that text, so the guard can be shown to fail rather than
/// assumed to.
///
/// @param lines - the file's source lines
/// @param at - the line the print opens on
fn announces_its_own_skip(lines: &[&str], at: usize) -> Option<String> {
    /// How many lines past a print's opening line this reads.
    ///
    /// Eight covers a `rustfmt`-wrapped macro whose literal runs to three
    /// source lines and whose early return follows the closing `);`, which is
    /// the shape both live-server suites were written in. Their return sits
    /// five lines below the `eprintln!(`, where the old window was four.
    const MACRO_WINDOW: usize = 8;

    // The window is clamped rather than demanded, because `get` on a range
    // past the end answers `None`: a skip inside the last eight lines of a
    // file would have been passed over, which is exactly the class of
    // blindness this check exists to remove.
    let end = at.saturating_add(MACRO_WINDOW).min(lines.len());
    let window = lines.get(at..end)?;
    let invocation = window.join("\n");
    let message = quoted_after(&invocation, "eprintln!(")
        .or_else(|| quoted_after(&invocation, "println!("))?;
    if !message.contains("skipping") {
        return None;
    }
    let returns = window.iter().skip(1).any(|following| {
        let trimmed = following.trim();
        trimmed == "return;" || trimmed == "return None;" || trimmed.starts_with("return Ok(());")
    });
    returns.then_some(message)
}

/// The guard sees a wrapped macro that ends `return None;`.
///
/// This is `live_postgres.rs` as it stood at `8607adf`, the one shape the check
/// was written to catch and could not. It fails before the §4.4.11 fix by both
/// routes at once - an empty message and an unrecognised return - so one of the
/// two being restored still fails it.
#[test]
fn the_guard_sees_a_wrapped_macro_that_returns_none() {
    // The marker is assembled rather than written, so this file does not
    // itself carry the text it forbids - the same reason the scan above
    // builds the name of the helper it looks for.
    // Both the marker and the early return are assembled rather than written.
    // The sibling check `every_early_return_in_a_test_says_why` reads a bare
    // `return None;` as a test bailing out in silence, and a fixture that spells
    // one out is indistinguishable from the thing it describes.
    let marker = format!("{}ping", "skip");
    let bail = format!("return {};", "None");
    let source = format!(
        r#"fn url() -> Option<ConnectionUrl> {{
    let Ok(text) = std::env::var("INILLUCENT_TEST_POSTGRES_URL") else {{
        eprintln!(
            "INILLUCENT_TEST_POSTGRES_URL is not set, so no PostgreSQL server is available to \
             migrate; {marker}. See this file header for the two psql commands."
        );
        {bail}
    }};
}}"#
    );
    let lines: Vec<&str> = source.lines().collect();
    let at = lines
        .iter()
        .position(|line| line.trim() == "eprintln!(")
        .expect("the fixture opens a print");
    let found = announces_its_own_skip(&lines, at).expect("the guard reads the wrapped literal");
    assert!(
        found.contains(&marker),
        "the message came back without the marker: {found}"
    );
}

/// A print that says `skipping` and does not return is not a skip.
///
/// The counterpart to the case above: widening the window is only correct if it
/// did not also widen what counts as a skip. A suite that announces it is
/// skipping one fixture and carries on running is reporting progress.
#[test]
fn a_print_with_no_early_return_is_not_a_skip() {
    let marker = format!("{}ping", "skip");
    let source = format!(
        r#"fn run() {{
    eprintln!(
        "the optional fixture is absent, so {marker} that one case and running the rest"
    );
    for case in cases() {{
        check(case);
    }}
}}"#
    );
    let lines: Vec<&str> = source.lines().collect();
    let at = lines
        .iter()
        .position(|line| line.trim() == "eprintln!(")
        .expect("the fixture opens a print");
    assert!(
        announces_its_own_skip(&lines, at).is_none(),
        "a print with no early return was read as a skip"
    );
}

/// Returns the text between the first pair of quotes after a marker.
///
/// The haystack may be several source lines joined by newlines, because a
/// `rustfmt`-wrapped macro puts its literal below the line that opens it. A
/// literal continued with a trailing backslash therefore comes back with the
/// backslash and the newline still in it, which is harmless: every caller here
/// asks whether a word appears in the message, not what the message renders as.
///
/// @param line - the source line, or several joined by newlines
/// @param marker - what the string follows
fn quoted_after(line: &str, marker: &str) -> Option<String> {
    let at = line.find(marker)?;
    let rest = line.get(at.saturating_add(marker.len())..)?;
    let open = rest.find('"')?;
    let body = rest.get(open.saturating_add(1)..)?;
    let close = body.find('"')?;
    Some(body.get(..close)?.to_string())
}

/// Returns every `.rs` file in the workspace's own sources.
///
/// @param root - the workspace root
fn rust_sources(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.join("crates"), root.join("drivers")];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name == "target") {
                    continue;
                }
                pending.push(path);
            } else if path.extension().is_some_and(|kind| kind == "rs") {
                found.push(path);
            }
        }
    }
    found
}

/// The shell may reach past the driver only where it is recorded, and the
/// numbers only go down.
///
/// **The driver's README calls it "the one surface every language binding
/// reaches the engine through", and the shell was not using it (task-1932,
/// M1).** Eight files under `crates/inillucent-cli/src` named
/// `inillucent_engine` directly, which makes the driver one of two surfaces
/// rather than the one - and a difference between them is a difference every
/// binding inherits while the shell, the program most people meet first, does
/// not.
///
/// This is a ratchet rather than a ban, because moving the shell onto the
/// driver is not one change. `Shell` opens sessions, registers virtual table
/// modules, installs an authorizer, reads pool statistics and drives `ATTACH` -
/// several of which the driver does not offer, and each one is a decision about
/// what the driver's surface should be rather than a mechanical substitution.
/// What a ratchet buys is that the number cannot go up while that is decided:
/// a new file reaching past the driver fails here, and a file that moves has
/// its row deleted.
///
/// The count is of *lines* naming the crate rather than of files, so a file
/// that moves half of its uses still shows progress.
#[test]
fn no_shell_file_reaches_past_the_driver_more_than_it_is_recorded_at() {
    // Every file under `crates/inillucent-cli/src` that names
    // `inillucent_engine`, and how many lines of it do. Measured at
    // task-1932; a row at zero is a file that has moved and whose row should
    // be deleted.
    const REACHES: [(&str, usize); 8] = [
        // The shell itself. Down from ten to one in task-1962 (roadmap item
        // 7): the virtual table modules, the authorizer, the cache statistics,
        // the statement budget and `leading_trivia` are all on the driver's
        // surface now. The one left is `use inillucent_engine::connect::
        // {Connection, Database}` - the types the shell's statement loop is
        // built on. See the comment below this table for why that one is a
        // decision about the shell's value type rather than a substitution.
        ("crates/inillucent-cli/src/shell.rs", 1),
        // Down from eight to zero in task-1962 (roadmap item 7). The VFS
        // confinement root and the statement budget were both already
        // re-exported by the driver; these lines named the engine for types
        // that were one `use` away.
        ("crates/inillucent-cli/src/command/mod.rs", 0),
        // Down from seven to zero in task-1962 (roadmap item 7). Every one was
        // the authorizer trait and its two enums, which the driver re-exports
        // now: installing an authorizer is part of every binding's surface.
        ("crates/inillucent-cli/src/commands.rs", 0),
        // The VFS, for `inillucent diagnose`. Down from two to zero in
        // task-1946 (M11): it was reaching through the engine's own re-export
        // rather than into engine internals, which made it a dependency
        // question, and the driver re-exports `vfs` now.
        ("crates/inillucent-cli/src/diagnose.rs", 0),
        // Down from two to zero in task-1962 (roadmap item 7). `migrate`
        // staged its import through `ImportedDatabase::import_into` because
        // the driver only derived the target; `Database::import_sqlite_into`
        // takes one, which is the property a migration needs.
        ("crates/inillucent-cli/src/command/verbs.rs", 0),
        // One function signature taking an engine connection, which follows
        // `shell.rs`.
        ("crates/inillucent-cli/src/import.rs", 1),
        // Down from one to zero in task-1946 (M11). It was never a
        // dependency at all - the one hit was a sentence in a module comment
        // naming the crate by path, counted because the check reads lines
        // rather than imports. The sentence says "the engine's `pragma`
        // module" now, which is the same fact and not a reach.
        ("crates/inillucent-cli/src/dbconfig.rs", 0),
        // The MCP server. Down from one to zero in task-1932: the budget types
        // it needed are re-exported by the driver now.
        ("crates/inillucent-cli/src/mcp.rs", 0),
    ];

    let root = workspace_root();
    let mut over: Vec<String> = Vec::new();
    let mut gone: Vec<String> = Vec::new();
    for (relative, recorded) in REACHES {
        let path = root.join(relative);
        let Ok(source) = std::fs::read_to_string(&path) else {
            gone.push(format!("{relative} is not there any more; remove its row"));
            continue;
        };
        let reaching = source
            .lines()
            .filter(|line| line.contains("inillucent_engine"))
            .count();
        if reaching > recorded {
            over.push(format!(
                "{relative}: {reaching} lines name `inillucent_engine`, past its {recorded}"
            ));
        }
    }

    // And no file outside the list may name it at all.
    let mut unlisted: Vec<String> = Vec::new();
    for path in rust_files(&root.join("crates/inillucent-cli/src")) {
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if REACHES.iter().any(|(named, _)| *named == relative) {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        if source.contains("inillucent_engine") {
            unlisted.push(relative);
        }
    }

    assert!(
        gone.is_empty(),
        "the ratchet names files that are not there:\n{}",
        gone.join("\n")
    );
    assert!(
        over.is_empty(),
        "these shell files reach past the driver more than they are recorded at:\n{}\n\
         Reach the engine through `inillucent_driver`. If the driver does not offer what \
         the file needs, the driver gains it - that is what makes its README true.",
        over.join("\n")
    );
    assert!(
        unlisted.is_empty(),
        "these shell files name `inillucent_engine` and are not in the ratchet:\n{}\n\
         A new file reaching past the driver is the thing this check exists to stop.",
        unlisted.join("\n")
    );
}

/// No public item is named only where it is defined.
///
/// **The check the compiler cannot do.** `dead_code` sees a private item nothing
/// calls and warns; a `pub` one it says nothing about, because anything outside
/// the crate might use it. Nothing outside this workspace does - none of these
/// crates is published - so a `pub fn` that appears exactly once, at its own
/// definition, is dead code that never warns.
///
/// A mechanical sweep in task-1946 found 101 of them, about two thousand lines,
/// including a 724 line module re-exported and named nowhere, a 767 line
/// integrity checker with no production caller, and six of seven public
/// functions in a vacuum that the shipping `VACUUM` does not use. Deleting them
/// is M1, M2 and M3 of that review; this is what stops the list regrowing.
///
/// **What counts as a caller.** The identifier appearing anywhere else in any
/// tracked `.rs` file, including another line of the file it is defined in. That
/// is deliberately generous - a mention in a doc comment counts - because the
/// alternative is parsing Rust, and a grep that occasionally forgives is far
/// better than no check at all. What it catches is the item nothing anywhere
/// says the name of twice.
///
/// **The exclusions, each with its reason:**
///
/// - **Trait methods.** A method is called through the trait, so its name
///   appears at the declaration and at each implementation and nowhere else in a
///   crate whose callers are generic.
/// - **`extern "C"` exports.** The C ABI's whole purpose is to be called from
///   outside this workspace, where no grep can see it.
/// - **`main`.** Called by the operating system.
/// - **Items under `#[cfg(test)]`.** A test function is named by the harness.
/// - **`#[allow(dead_code)]` with a reason beside it.** Thirteen of those exist
///   and each carries an argument; this check does not relitigate them.
/// - **The three files section 6 of the review leaves for a person to delete.**
///   They are unreferenced by design at this commit and the build does not need
///   them, which is what that section asked for.
#[test]
fn no_public_item_is_callerless() {
    let root = workspace_root();
    let sources = rust_sources(&root);

    // One pass to read every file, because the answer for each item is a
    // question about all of them.
    let mut corpus: Vec<(String, String)> = Vec::new();
    for path in &sources {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        corpus.push((relative(&root, path), text));
    }

    let mut callerless: Vec<String> = Vec::new();
    for (named, text) in &corpus {
        if AWAITING_DELETION_FILES.iter().any(|file| named == file) {
            continue;
        }
        let lines: Vec<&str> = text.lines().collect();
        let in_tests = test_module_lines(&lines);
        for (at, line) in lines.iter().enumerate() {
            if in_tests.contains(&at) {
                continue;
            }
            let Some((kind, name)) = public_item(line) else {
                continue;
            };
            if is_trait_item(&lines, at) || exported_to_c(&lines, at) || allowed_dead(&lines, at) {
                continue;
            }
            if name == "main" {
                continue;
            }
            let mentions = corpus
                .iter()
                .map(|(_, other)| mentions_of(other, &name))
                .fold(0usize, usize::saturating_add);
            if mentions > 1 {
                continue;
            }
            callerless.push(format!("{named}:{}: {kind} {name}", at.saturating_add(1)));
        }
    }

    assert!(
        callerless.is_empty(),
        "these public items are named only where they are defined, so nothing calls \
         them and the compiler cannot say so:\n{}\n\
         Delete it, or give it the caller it was written for. If it is genuinely an \
         entry point nothing in this workspace uses, say so with \
         `#[allow(dead_code)]` and the reason.",
        callerless.join("\n")
    );
}

/// The files section 6 of the task-1946 review leaves for a person to delete.
///
/// Every reference to them is gone and the build does not need them, which is
/// what that section asked the implementation ticket for; an unreferenced `.rs`
/// file is not compiled, so both orders build. Until `git rm` runs they are
/// still tracked, and every item in them is callerless by design.
///
/// **`crates/inillucent-storage/src/check.rs` is not here, and section 6 is
/// wrong about it.** Two of its seven callers are `inillucent-storage`'s own
/// `#[cfg(test)]` modules, in `mutate.rs` and `vacuum.rs`, and those cannot
/// reach `inillucent-compat` - that crate depends on this one. Deleting the
/// file takes two working tests with it. It is behind an off-by-default
/// `check` feature instead, so a release build does not carry its 767 lines,
/// and its items have the callers this check asks for.
const AWAITING_DELETION_FILES: [&str; 2] = [
    "crates/inillucent-transaction/src/state.rs",
    "crates/inillucent-catalog/src/rebuild.rs",
];

/// Returns what kind of public item a line declares, and its name.
///
/// @param line - the source line
fn public_item(line: &str) -> Option<(&'static str, String)> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("pub ")?;
    for (keyword, kind) in [
        ("fn ", "fn"),
        ("const ", "const"),
        ("struct ", "struct"),
        ("static ", "static"),
    ] {
        let Some(after) = rest.strip_prefix(keyword) else {
            continue;
        };
        let name: String = after
            .chars()
            .take_while(|letter| letter.is_alphanumeric() || *letter == '_')
            .collect();
        if name.is_empty() {
            return None;
        }
        // A tuple struct with no body and a unit struct are still declarations;
        // what is excluded is a `pub fn` whose name is followed by nothing that
        // could be a parameter or generic list, which is not a declaration.
        if kind == "fn" {
            let following = after.get(name.len()..)?;
            if !following.starts_with('(') && !following.starts_with('<') {
                return None;
            }
        }
        return Some((kind, name));
    }
    None
}

/// Reports whether the item at `at` is inside a `trait` block.
///
/// Read backwards rather than by brace counting, because a trait's items are the
/// only ones indented under a line that begins `trait` or `pub trait` - and an
/// `impl Trait for Type` block's items are calls of the trait's own names, which
/// is what makes them mentions rather than definitions.
///
/// @param lines - the file's lines
/// @param at - the line the item is on
fn is_trait_item(lines: &[&str], at: usize) -> bool {
    let indent = lines
        .get(at)
        .map(|line| line.len().saturating_sub(line.trim_start().len()))
        .unwrap_or(0);
    if indent == 0 {
        return false;
    }
    for earlier in (0..at).rev() {
        let Some(line) = lines.get(earlier) else {
            return false;
        };
        let trimmed = line.trim_start();
        let outer = line.len().saturating_sub(trimmed.len());
        if outer >= indent || trimmed.is_empty() {
            continue;
        }
        return trimmed.starts_with("trait ")
            || trimmed.starts_with("pub trait ")
            || trimmed.starts_with("pub(crate) trait ");
    }
    false
}

/// Reports whether the item at `at` is exported through the C ABI.
///
/// @param lines - the file's lines
/// @param at - the line the item is on
fn exported_to_c(lines: &[&str], at: usize) -> bool {
    lines
        .get(at)
        .is_some_and(|line| line.contains("extern \"C\""))
        || lines
            .get(at.saturating_sub(3)..at)
            .unwrap_or_default()
            .iter()
            .any(|line| line.contains("#[no_mangle]") || line.contains("#[unsafe(no_mangle)]"))
}

/// Reports whether the item at `at` carries `#[allow(dead_code)]`.
///
/// @param lines - the file's lines
/// @param at - the line the item is on
fn allowed_dead(lines: &[&str], at: usize) -> bool {
    lines
        .get(at.saturating_sub(3)..at)
        .unwrap_or_default()
        .iter()
        .any(|line| line.contains("allow(dead_code)"))
}

/// Counts how many times an identifier appears in some source, as a whole word.
///
/// @param text - the source to read
/// @param name - the identifier
fn mentions_of(text: &str, name: &str) -> usize {
    let bytes = text.as_bytes();
    let mut count = 0usize;
    let mut from = 0usize;
    while let Some(found) = text.get(from..).and_then(|rest| rest.find(name)) {
        let at = from.saturating_add(found);
        let before = at
            .checked_sub(1)
            .and_then(|earlier| bytes.get(earlier).copied());
        let after = bytes.get(at.saturating_add(name.len())).copied();
        let word = |byte: Option<u8>| {
            byte.is_some_and(|letter| letter.is_ascii_alphanumeric() || letter == b'_')
        };
        if !word(before) && !word(after) {
            count = count.saturating_add(1);
        }
        from = at.saturating_add(name.len().max(1));
    }
    count
}

/// How long each function over 150 lines is allowed to be.
///
/// **The function ratchet, beside the module one (task-1932, TDD section 7).**
/// The review counted twenty functions over 150 lines and recommended exactly
/// this: record each at its current length, so new code cannot add to the list
/// and a fix that touches one of them leaves it no longer than it found it. A
/// module ceiling alone does not do that - a file can stay the same size while
/// one function inside it absorbs every change, which is how a five hundred
/// line function is arrived at.
///
/// Measured the way `function_lengths` measures: from the line that opens the
/// function to the first line that is exactly its indentation and a closing
/// brace. That is a lexical rule rather than a parse, and it is the same rule
/// for every entry, which is what a ratchet needs.
/// A function that falls under 150 lines loses its row rather than keeping a
/// lowered one: the list is what is over the threshold, and a row on a short
/// function is a hole the width of its old number. Seven left in task-1962 A8.
const FUNCTION_CEILINGS: [(&str, &str, usize); 52] = [
    // `gradeembed.rs::run`, `main.rs::main`, `scenarios.rs::grade`,
    // `report.rs::render`, `synth.rs::build`, `synth.rs::build_source`,
    // `synth.rs::check` and `synth.rs::embed` came off this list in task-1973,
    // which split all eight. They are 94, 127, 86, 14, 8, 25, 20 and 77 lines
    // now, and a row for a function that is not over 150 is a row nobody can
    // act on: the unrecorded bar below catches it if it ever grows back, and
    // this list is meant to be exactly what was over 150 when it was written.
    (
        "crates/inillucent-engine/src/engine/open.rs",
        "open_on",
        160,
    ),
    // Not on this list before task-1970's `cargo fmt --all`: it was 148 lines and the limit for an
    // unrecorded function is 150. Reflow took it to 160, adding 4 commas with its identifiers
    // unchanged character for character. Recorded rather than split, because the code did not
    // change (task-1966).
    (
        "crates/inillucent-bench/src/scenarios.rs",
        "filtered_vector",
        160,
    ),
    ("crates/inillucent-compat/src/bin/fullgate.rs", "run", 263),
    ("crates/inillucent-compat/src/bin/readgate.rs", "run", 280),
    ("crates/inillucent-compat/src/bin/writegate.rs", "run", 226),
    ("crates/inillucent-sql/src/bind.rs", "bind_expr", 303),
    ("crates/inillucent-engine/src/ddl.rs", "run_directive", 273),
    ("crates/inillucent-tree/src/paged/skip.rs", "skip_scan", 249),
    ("crates/inillucent-sql/src/bind.rs", "bind_call_with", 248),
    // 237 in task-2088, which lifted the `IN` list arm into `in_list`.
    ("crates/inillucent-exec/src/expr/tree.rs", "compile", 237),
    (
        "crates/inillucent-tree/src/leaf/encode.rs",
        "encode_rows_with",
        239,
    ),
    (
        "crates/inillucent-compat/src/bin/readperf.rs",
        "measure",
        232,
    ),
    // 229 before task-1946 H4 made `search_branches` fallible and the six
    // probes it is asked through became one helper. The row moved from
    // `retrieval` to `retrieval_checks` in the same ticket and is the same body:
    // `inillucent-migrate` denies `clippy::expect_used`, so a refused probe has
    // to travel out as a value, and `retrieval` is now the four lines that turn
    // that value into a failed check for a caller that wants a `Vec<Check>`.
    //
    // **Lowered from 213 in task-2067**, which added a check and took it to 214.
    // The vector and hybrid comparisons - everything guarded by the source
    // having a vector per chunk - are `vector_checks` now, which is a hundred
    // and twenty lines this one no longer holds. They were already one
    // contiguous block behind one `if`, so the seam was where the work was.
    (
        "crates/inillucent-migrate/src/verify.rs",
        "retrieval_checks",
        116,
    ),
    ("crates/inillucent-storage/src/mutate.rs", "balance", 224),
    ("crates/inillucent-compat/src/bin/analytical.rs", "run", 223),
    // The group name in front of the fields it reads, from task-1962 A1
    // step 2; the formatter then wraps what it used to fit on one line.
    (
        "crates/inillucent-engine/src/engine/compiled.rs",
        "write",
        220,
    ),
    (
        "crates/inillucent-compat/src/bin/storageprofile.rs",
        "run",
        218,
    ),
    // 215 before task-1962 A9: the `trigger::fire` calls pass a
    // `TriggerFiring` literal.
    (
        "crates/inillucent-exec/src/dml/update.rs",
        "update_at_cached",
        219,
    ),
    ("crates/inillucent-model/tests/campaign.rs", "segment", 213),
    // The group name in front of the fields it reads, from task-1962 A1
    // step 2; the formatter then wraps what it used to fit on one line.
    (
        "crates/inillucent-engine/src/engine/open.rs",
        "import_into",
        221,
    ),
    // 206 before task-1946 M12 moved the row encoding, the locate and the
    // orphaned-extent read into `encode_row_spilling_wide_values`,
    // `locate_and_read_previous` and `orphaned_extents`.
    ("crates/inillucent-tree/src/write.rs", "write_row", 137),
    // 205 before task-1962 A9: the `write_one` and `upsert_row` calls pass a
    // `WriteRequest` and an `Upsert`, written as literals.
    ("crates/inillucent-exec/src/dml/insert.rs", "insert_at", 211),
    // The group name in front of the fields it reads, from task-1962 A1
    // step 2; the formatter then wraps what it used to fit on one line.
    (
        "crates/inillucent-engine/src/ddl/index.rs",
        "create_index",
        198,
    ),
    (
        "crates/inillucent-compat/src/fixtures.rs",
        "malformed_fixtures",
        192,
    ),
    ("crates/inillucent-remote/src/migrate.rs", "run", 191),
    ("crates/inillucent-sql/src/plan.rs", "plan_select_with", 189),
    ("crates/inillucent-migrate/src/lib.rs", "migrate", 189),
    // 183 before task-1962 A9: `record` takes a `Timed` and the eight call
    // sites are struct literals, which rustfmt writes one field per line.
    (
        "crates/inillucent-compat/src/bin/writeperf.rs",
        "measure_scale",
        197,
    ),
    ("crates/inillucent-storage/src/check.rs", "check_tree", 182),
    (
        "crates/inillucent-core/src/embed_onnx.rs",
        "run_encodings",
        182,
    ),
    ("crates/inillucent-compat/src/bin/release.rs", "run", 176),
    ("crates/inillucent-engine/src/recovery.rs", "open_file", 173),
    (
        "crates/inillucent-compat/src/fixtures.rs",
        "valid_fixtures",
        173,
    ),
    // Moved to `paged/bulk.rs` in task-2006 and split into three named passes -
    // `plan_leaves`, `write_leaf_run` and `build_interior_levels` - which took it
    // from 197 lines to 71. Recorded where it lives now, because the `gone` check
    // below matches on the path as well as the name.
    (
        "crates/inillucent-tree/src/paged/bulk.rs",
        "bulk_build_rows",
        80,
    ),
    ("crates/inillucent-core/src/bm25.rs", "search", 167),
    // 166 before task-1946 M12 moved the decision into `choose_fit`, which is
    // 133 and is the half with the argument in it.
    ("crates/inillucent-tree/src/write.rs", "make_room", 40),
    (
        "crates/inillucent-sql/src/bind.rs",
        "bind_column_reference",
        165,
    ),
    (
        "crates/inillucent-engine/src/import.rs",
        "carry_tables",
        165,
    ),
    // 164 before task-1962 A9: the destructure of the `CandidateContext` it now takes.
    ("crates/inillucent-sql/src/plan.rs", "index_candidate", 167),
    ("crates/inillucent-compat/src/bin/testrun.rs", "report", 164),
    ("crates/inillucent-compat/src/bin/planperf.rs", "run", 163),
    (
        "crates/inillucent-engine/src/vtab.rs",
        "create_virtual_table",
        161,
    ),
    ("crates/inillucent-tree/src/write.rs", "merge_if_small", 157),
    ("crates/inillucent-scalar/src/builtin.rs", "call_with", 157),
    // 154 before task-1962 A9: the destructure of the `CandidateContext` it now takes.
    (
        "crates/inillucent-sql/src/plan/seek_union.rs",
        "in_list_union_path",
        157,
    ),
    (
        "crates/inillucent-remote/src/tls/windows.rs",
        "handshake",
        154,
    ),
    (
        "crates/inillucent-engine/src/engine/compiled.rs",
        "apply_compiled",
        154,
    ),
    (
        "crates/inillucent-compat/src/bin/probeprofile.rs",
        "probe_stages",
        154,
    ),
    // The group name in front of the fields it reads, from task-1962 A1
    // step 2; the formatter then wraps what it used to fit on one line.
    (
        "crates/inillucent-engine/src/vectors.rs",
        "create_vector_index",
        154,
    ),
    ("crates/inillucent-core/src/filter.rs", "compile", 153),
    // 151 before task-1962 A14 moved the SSL policy check into
    // `ssl_policy_says`, which is the half the host name is for.
    (
        "crates/inillucent-remote/src/tls/windows.rs",
        "verify_against",
        109,
    ),
    (
        "crates/inillucent-compat/src/bin/fullgate.rs",
        "report_costs",
        151,
    ),
];

/// The length past which a function that is not already recorded fails.
///
/// The same number the review counted at, so the recorded list is exactly what
/// was over 150 lines when the ratchet was written.
const LONGEST_NEW_FUNCTION: usize = 150;

/// Returns the length of every function in one file, longest wins per name.
///
/// A name that appears twice in a file - the same method on two types, or a
/// trait method implemented several times - is recorded once, at its longest,
/// because the ratchet is about the function that is too long rather than about
/// which impl block it sits in.
///
/// @param text - the file's contents
fn function_lengths(text: &str) -> std::collections::BTreeMap<String, usize> {
    let lines: Vec<&str> = text.lines().collect();
    let mut found: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for (at, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let indent = line.len().saturating_sub(trimmed.len());
        let Some(name) = opens_a_function(trimmed) else {
            continue;
        };
        if declaration(&lines, at) {
            continue;
        }
        let closing = format!("{}}}", " ".repeat(indent));
        for (offset, later) in lines.iter().enumerate().skip(at.saturating_add(1)) {
            if *later == closing {
                let length = offset.saturating_sub(at).saturating_add(1);
                let held = found.entry(name).or_insert(0);
                *held = (*held).max(length);
                break;
            }
        }
    }
    found
}

/// Reports whether the `fn` at `at` is a declaration rather than a definition.
///
/// **A declaration has no body to measure.** A trait method written
/// `fn read(&self, ...) -> VfsResult<()>;` ends at the semicolon, and measuring
/// from it to the next closing brace at the same indent measures the rest of the
/// trait instead - so `VfsFile::read` reported 159 lines, and adding a method
/// further down the file made it grow. Found in task-1946 by adding
/// `Vfs::rename` above it.
///
/// A signature may run over several lines, so this reads forward to whichever
/// comes first: the `{` that opens a body, or the `;` that ends a declaration.
///
/// @param lines - the file's lines
/// @param at - the line the `fn` is on
fn declaration(lines: &[&str], at: usize) -> bool {
    for offset in at..lines.len().min(at.saturating_add(12)) {
        let Some(line) = lines.get(offset) else {
            return false;
        };
        let code = line.split("//").next().unwrap_or("");
        match (code.find('{'), code.rfind(';')) {
            (Some(_), _) => return false,
            (None, Some(_)) => return true,
            (None, None) => continue,
        }
    }
    false
}

/// Returns the name a line declares, when the line opens a function.
///
/// Keywords are stripped in the order Rust writes them. Anything else - a `fn`
/// inside a string, a function pointer type - answers `None`, because the name
/// has to be followed by a parameter list or a generic list for this to be a
/// declaration.
///
/// @param trimmed - the line with its leading spaces removed
fn opens_a_function(trimmed: &str) -> Option<String> {
    let mut rest = trimmed;
    for keyword in [
        "pub(crate) ",
        "pub(super) ",
        "pub(self) ",
        "pub ",
        "default ",
        "const ",
        "async ",
        "unsafe ",
    ] {
        while let Some(shorter) = rest.strip_prefix(keyword) {
            rest = shorter;
        }
    }
    if let Some(shorter) = rest.strip_prefix("extern \"") {
        rest = shorter
            .split_once('"')
            .map(|(_, after)| after.trim_start())?;
    }
    let rest = rest.strip_prefix("fn ")?;
    let name: String = rest
        .chars()
        .take_while(|letter| letter.is_alphanumeric() || *letter == '_')
        .collect();
    let after = rest.get(name.len()..)?;
    (!name.is_empty() && (after.starts_with('(') || after.starts_with('<'))).then_some(name)
}

/// No function grows past the length it is recorded at, and no new one joins
/// the list.
#[test]
fn no_function_grows_past_the_length_it_is_recorded_at() {
    let root = workspace_root();
    let mut measured: std::collections::BTreeMap<(String, String), usize> =
        std::collections::BTreeMap::new();
    // `crates/` and `drivers/` are the workspace's own sources. Walking the
    // root instead also walks `target/`, which holds a copy of several crates
    // under `target/package/` and the retired `inillucent-vm` among them - so
    // the ratchet reported functions nobody can edit, and took five minutes.
    let mut sources = rust_files(&root.join("crates"));
    sources.extend(rust_files(&root.join("drivers")));
    for path in sources {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let named = relative(&root, &path);
        for (name, length) in function_lengths(&text) {
            measured.insert((named.clone(), name), length);
        }
    }

    let mut over: Vec<String> = Vec::new();
    let mut joined: Vec<String> = Vec::new();
    for ((path, name), length) in &measured {
        let recorded = FUNCTION_CEILINGS
            .iter()
            .find(|(held, called, _)| held == path && called == name)
            .map(|(_, _, ceiling)| *ceiling);
        match recorded {
            Some(ceiling) if *length > ceiling => {
                over.push(format!(
                    "{path}::{name}: {length} lines, past its {ceiling}"
                ));
            }
            Some(_) => {}
            None if *length > LONGEST_NEW_FUNCTION => {
                joined.push(format!("{path}::{name}: {length} lines"));
            }
            None => {}
        }
    }
    let gone: Vec<String> = FUNCTION_CEILINGS
        .iter()
        .filter(|(path, name, _)| {
            !measured.contains_key(&((*path).to_string(), (*name).to_string()))
        })
        .map(|(path, name, _)| format!("{path}::{name} is not there any more; remove its row"))
        .collect();

    assert!(
        over.is_empty(),
        "these functions grew past the length they are recorded at:\n{}\n\
         Split one out rather than raising the number - a function reaches five hundred lines \
         because every individual addition to it was reasonable.",
        over.join("\n")
    );
    assert!(
        joined.is_empty(),
        "these functions are over {LONGEST_NEW_FUNCTION} lines and are not on the recorded \
         list:\n{}\n\
         The list is what already exists, not a budget: write the new one shorter.",
        joined.join("\n")
    );
    assert!(gone.is_empty(), "{}", gone.join("\n"));
}

/// No test file may define its own skip helper.
///
/// **The defect this refuses shipped and hid nine tests (task-1969, 4.2).**
/// `crates/inillucent-compat/tests/differential.rs` defined
///
/// a private helper of its own named `announce_skip`, whose whole body was an
/// `eprintln!` of the sentence the library helper prints - and which therefore
/// printed neither the `; skipping` marker nor the panic.
///
/// It shadowed `inillucent_compat::differential::announce_skip` at nine call
/// sites. Neither guard saw it. `every_skip_site_carries_the_one_marker` reads
/// an `eprintln!` only when a `return;` follows within four lines, and here the
/// `eprintln!` was a helper body followed by a closing brace.
/// `every_early_return_in_a_test_says_why` accepted every call site because it
/// matched the helper by name. `inillucent-testrun --strict` printed `ok`,
/// because the output carried no `; skipping` and the tests that returned still
/// counted as run. So the namesake of the differential tier compared nothing to
/// SQLite on any machine without the oracle, and said it had.
///
/// The rule is stated on the definition rather than on the call, because a call
/// to a local helper and a call to the library one are the same three words.
/// There is exactly one `skipping` in this workspace -
/// `inillucent_base::testing::skipping` - and exactly one `announce_skip` -
/// `inillucent_compat::differential::announce_skip`, which calls it. Anything
/// else with either name is a second implementation of a thing whose whole
/// value is that there is one of it.
#[test]
fn no_test_file_defines_its_own_skip_helper() {
    let root = workspace_root();
    // The two definitions there are meant to be, by path rather than by name:
    // naming them would let a third file take the same name and pass.
    let allowed = [
        "crates/inillucent-base/src/testing.rs",
        "crates/inillucent-compat/src/differential.rs",
    ];
    let mut defined: Vec<String> = Vec::new();
    let mut read = 0usize;
    for file in rust_sources(&root) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        read = read.saturating_add(1);
        let relative = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        if allowed.contains(&relative.as_str()) {
            continue;
        }
        for (at, line) in text.lines().enumerate() {
            let Some(name) = function_name(line) else {
                continue;
            };
            if name == "skipping" || name == "announce_skip" {
                // Matched on the parsed name rather than on the text, which is
                // also what keeps the forbidden spelling out of this file.
                defined.push(format!("{relative}:{}", at.saturating_add(1)));
            }
        }
    }
    assert!(
        read >= 300,
        "read {read} source files, which means this is looking in the wrong place \
         rather than that the workspace has no source"
    );
    assert!(
        defined.is_empty(),
        "these files define their own `skipping` or `announce_skip`:\n  {}\n\
         There is one of each in the workspace - `inillucent_base::testing::skipping` \
         and `inillucent_compat::differential::announce_skip` - and a local copy \
         prints no marker and does not panic under `INILLUCENT_STRICT`, so the \
         suite that calls it skips invisibly. A production crate that cannot \
         depend on this harness uses `inillucent_base::testing::skipping` from \
         its own `[dev-dependencies]`.",
        defined.join("\n  ")
    );
}

/// The names the length cap lets through, and the ticket each is waiting on.
///
/// **A ratchet with no cap is how a criterion reading "no production function
/// over 300 lines" was satisfied with eight functions over 300 (task-1969,
/// 7.2).** `FUNCTION_CEILINGS` freezes a function at its current length and
/// fails when it grows, which stops the tree getting worse and does nothing
/// about what is already there: a new entry of any length is accepted, so a
/// five-hundred-line function could be added tomorrow and the ratchet would
/// record it.
///
/// So the list below is the whole of what is allowed to be over 300, every
/// entry carries the ticket that removes it, and the test refuses anything
/// else. An entry that is not on this list and not under 300 fails, and so
/// does a *new* entry over 150 - which is the length the review counted at, so
/// the recorded list is exactly what was over 150 when the ratchet was written.
const OVER_THREE_HUNDRED: [(&str, &str, &str); 1] = [
    // A15: an `Identifier` type and the `bind.rs` split. task-1962 re-measured
    // that only 11 of the 114 byte-or-string identifier signatures are in this
    // file, so A15 does not depend on the split; both are one ticket of their
    // own, which section 6.4 of the task-1969 review designs.
    ("crates/inillucent-sql/src/bind.rs", "bind_expr", "A15"),
    // The three `inillucent-bench` rows that were here came off in task-1973,
    // which split all three: `gradeembed.rs::run` is 94 lines, `main.rs::main`
    // is 127 and `scenarios.rs::grade` is 86. `bind_expr` is the last function
    // in the workspace over 300 lines.
];

/// No recorded ceiling is over 300 lines, and no new one is over 150.
///
/// The companion to [`no_function_grows_past_the_length_it_is_recorded_at`],
/// which stops the list getting worse. This is what stops it staying bad:
/// every entry over 300 is named in [`OVER_THREE_HUNDRED`] with the ticket that
/// removes it, and the day a ticket lands its rows come out of both lists
/// together.
#[test]
fn no_function_ceiling_is_over_three_hundred() {
    let waiting: std::collections::BTreeMap<(&str, &str), &str> = OVER_THREE_HUNDRED
        .iter()
        .map(|(file, function, ticket)| ((*file, *function), *ticket))
        .collect();

    let mut unexcused: Vec<String> = Vec::new();
    let mut stale: Vec<String> = Vec::new();
    for (file, function, length) in FUNCTION_CEILINGS {
        let excused = waiting.get(&(file, function));
        if length > 300 && excused.is_none() {
            unexcused.push(format!("{file}::{function} at {length}"));
        }
        if length <= 300 {
            if let Some(ticket) = excused {
                stale.push(format!(
                    "{file}::{function} is {length} lines and is still listed as waiting on {ticket}"
                ));
            }
        }
    }
    assert!(
        unexcused.is_empty(),
        "these recorded ceilings are over 300 lines and no ticket is named for them:\n  {}\n\
         task-1961's seventh criterion is \"no production function over 300 lines\". Split one \
         out, or add it to OVER_THREE_HUNDRED with the ticket that will.",
        unexcused.join("\n  ")
    );
    assert!(
        stale.is_empty(),
        "these are under 300 and still listed as exceptions, so the list is describing a tree \
         that has moved:\n  {}",
        stale.join("\n  ")
    );

    // And the other half: a function recorded for the first time may not be
    // over the length the review counted at. Without this the cap above is one
    // ticket away from being wrong again.
    let mut over: Vec<String> = Vec::new();
    for (file, function, length) in FUNCTION_CEILINGS {
        if length > LONGEST_NEW_FUNCTION
            && length <= 300
            && !waiting.contains_key(&(file, function))
        {
            over.push(format!("{file}::{function} at {length}"));
        }
    }
    assert!(
        over.len() <= RECORDED_OVER_THE_NEW_BAR,
        "{} recorded ceilings are over {LONGEST_NEW_FUNCTION} lines, and {RECORDED_OVER_THE_NEW_BAR} \
         were when this bar was written. A new function over {LONGEST_NEW_FUNCTION} lines does not \
         get a row; it gets split.\n  {}",
        over.len(),
        over.join("\n  ")
    );
}

/// How many recorded ceilings were between the new-function bar and 300 when
/// this test was written.
///
/// A number rather than a list, because the list is `FUNCTION_CEILINGS` itself
/// and a second copy of it would be a second thing to keep in step. What the
/// count refuses is a *new* long function: the total may go down freely and may
/// not go up.
///
/// 55 when this was written; 48 since task-1973 split seven `inillucent-bench`
/// functions and took five of their rows out of this band.
const RECORDED_OVER_THE_NEW_BAR: usize = 48;

/// No function outside a test module takes more than eight parameters.
///
/// **There was no parameter check at all, which is how `synth_embed` keeps nine
/// under an `#[allow]` (task-1969, 7.2).** task-1961's eighth criterion says no
/// function takes more than eight parameters and that the ten which did now
/// take structs; the ten do, and nothing stopped an eleventh. Fifty functions
/// take seven or eight today - thirty-six take seven and fourteen take eight -
/// so the bar is where the next one would cross it rather than where the tree
/// already is.
///
/// Eight is also the bar `clippy.toml` sets, where
/// `too-many-arguments-threshold = 8` carries the argument for it. Clippy
/// lints above its threshold and counts the receiver, so it fires at nine
/// arguments with the receiver among them, and this test fails at nine
/// without it. They are the same bar, and
/// `no_attribute_turns_off_the_parameter_lint` below is what stops an
/// `#[allow]` from moving it for one function.
///
/// **The count was thirty-eight when this was written, and it was wrong.**
/// `parameter_list` read `pub(crate) fn foo(` as a list whose one parameter was
/// `crate`, so every `pub(crate) fn` and `pub(super) fn` in the workspace was
/// counted as taking one. task-1977 fixed that and re-measured: forty-two of
/// the functions that were invisible take five parameters or more.
///
/// The receiver does not count - `&self` is not an argument a caller passes -
/// and neither does anything under `#[cfg(test)]`, because a test builder that
/// takes ten values is a test fixture rather than an interface.
#[test]
fn no_function_takes_more_than_eight_parameters() {
    // **Nothing is excused.** `synth_embed` was, in task-1970, because it took
    // nine parameters under an `#[allow(clippy::too_many_arguments)]`; task-1973
    // gave it a `SynthEmbedRequest` and the allow came off with the exemption.
    // A new function that crosses the bar has no list to be added to.
    let root = workspace_root();
    let mut wide: Vec<String> = Vec::new();
    let mut read = 0usize;
    for file in rust_sources(&root) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        read = read.saturating_add(1);
        let relative = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        for (function, count) in parameter_counts(&text) {
            if count <= 8 {
                continue;
            }
            wide.push(format!("{relative}::{function} takes {count}"));
        }
    }
    assert!(
        read >= 300,
        "read {read} source files, which means this is looking in the wrong place rather than \
         that the workspace has no source"
    );
    assert!(
        wide.is_empty(),
        "these functions take more than eight parameters:\n  {}\n\
         Group them into a struct, the way task-1961's A8 did for the ten that used to. A \
         call with nine positional arguments is one nobody can read at the call site, and \
         `#[allow(clippy::too_many_arguments)]` is not an answer - it is the warning being \
         turned off.",
        wide.join("\n  ")
    );
}

/// No attribute in `crates/` or `drivers/` turns off `too_many_arguments`.
///
/// **Thirty-two of them were left behind by the refactors that fixed the
/// functions they guarded (task-1977).** `clippy.toml` sets
/// `too-many-arguments-threshold = 8` and clippy lints above its threshold, so
/// an `#[allow]` only does something for a function taking nine arguments
/// counting the receiver. The widest function carrying one took eight.
/// `bench/src/tune.rs` carried one above `label_for(dials: &Dials<'_>)`, which
/// takes one parameter, and `compat/src/bin/walperf.rs` and
/// `compat/src/bin/writeperf.rs` carried one above `struct Timed<'a>` - not
/// above a function at all - left there when task-1962 turned those argument
/// lists into types, with the doc comment of the function each used to guard
/// stranded above it.
///
/// **The check is for the attribute rather than for the word.** Criterion 28 of
/// the task-1969 part six TDD asks for `grep -rn 'too_many_arguments' crates/
/// drivers/` to return nothing, and six of that grep's hits are sentences
/// explaining why an argument list became a struct. Deleting those would delete
/// the argument, so no state of this tree can satisfy the criterion as it is
/// written; what it is asking for is this.
///
/// An `#[expect]` is refused on the same terms as an `#[allow]`: both turn the
/// lint off, and which one is written is a question about whether the warning
/// is expected to come back rather than about the parameter list.
#[test]
fn no_attribute_turns_off_the_parameter_lint() {
    let root = workspace_root();
    let mut found: Vec<String> = Vec::new();
    let mut read = 0usize;
    for file in rust_sources(&root) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        read = read.saturating_add(1);
        let relative = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        for (at, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if !trimmed.starts_with("#[") || !trimmed.contains("too_many_arguments") {
                continue;
            }
            found.push(format!("{relative}:{}", at.saturating_add(1)));
        }
    }
    assert!(
        read >= 300,
        "read {read} source files, which means this is looking in the wrong place rather than \
         that the workspace has no source"
    );
    assert!(
        found.is_empty(),
        "these attributes turn off `clippy::too_many_arguments`:\n  {}\n\
         The threshold is set once, in `clippy.toml`, with the argument for where it is. A \
         function that has grown past it takes a struct, the way task-1961's A8 did for the \
         ten that used to; an attribute here moves the bar for one function and says nothing \
         about why.",
        found.join("\n  ")
    );
}

/// Returns each function in a file and how many parameters it declares.
///
/// The receiver is not counted, and anything inside a `#[cfg(test)]` module is
/// skipped: a test builder that takes ten values is a fixture rather than an
/// interface, and holding it to an interface's bar would be asking the wrong
/// question of the right rule.
///
/// The parameters are counted at the signature's own bracket depth, so a
/// closure argument - `impl Fn(&str) -> bool` - is one parameter rather than
/// two, and a generic bound with a comma in it is not two either.
///
/// @param text - the file's contents
fn parameter_counts(text: &str) -> Vec<(String, usize)> {
    let mut found = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut test_module_at: Option<usize> = None;
    let mut depth = 0i32;
    for (at, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("#[cfg(test)]") {
            test_module_at = Some(depth.max(0) as usize);
        }
        let opened = depth;
        depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
        if let Some(held) = test_module_at {
            if depth <= held as i32 && opened > held as i32 {
                test_module_at = None;
            }
            continue;
        }
        let Some(name) = function_name(line) else {
            continue;
        };
        // The signature runs to the line whose brackets balance, which is the
        // same rule `declaration` uses one test above.
        let mut signature = String::new();
        let mut brackets = 0i32;
        for later in lines.iter().skip(at) {
            signature.push_str(later);
            brackets += later.matches('(').count() as i32 - later.matches(')').count() as i32;
            if brackets == 0 && signature.contains('(') {
                break;
            }
            signature.push(' ');
        }
        let Some(inside) = parameter_list(&signature) else {
            continue;
        };
        found.push((name, count_parameters(&inside)));
    }
    found
}

/// Returns the text between a signature's parameter brackets.
///
/// **The bracket that closes the parameters, not the last one on the line.**
/// This read `rfind(')')`, which for `fn f(a: u8) -> Option<(usize, T)>` is the
/// bracket inside the *return* type - so the "parameters" it counted ran past
/// the end of the list and `find_equality`, which takes eight, was reported as
/// taking ten.
///
/// **And the bracket that opens them is not the first one either (task-1977).**
/// This read `find('(')`, which for `pub(crate) fn create_index(` is the
/// bracket inside `pub(crate)` - so the parameter list it returned was the text
/// `crate`, and every `pub(crate) fn` and `pub(super) fn` in the workspace was
/// counted as taking one parameter. Forty-two of them take five or more. None
/// was over the bar when this was found, so the test had been passing for the
/// wrong reason rather than hiding a failure, but a new `pub(crate) fn` taking
/// twelve would have passed it too.
///
/// @param signature - the function's signature, brackets balanced
fn parameter_list(signature: &str) -> Option<String> {
    let open = parameter_bracket(signature)?;
    let mut depth = 0i32;
    for (at, character) in signature.char_indices().skip(open) {
        match character {
            '(' => depth = depth.saturating_add(1),
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return signature
                        .get(open.saturating_add(1)..at)
                        .map(str::to_string);
                }
            }
            _ => {}
        }
    }
    None
}

/// `parameter_counts` reads a `pub(crate) fn`'s real parameter list.
///
/// **The bug this holds shut made the whole check blind to two thirds of the
/// engine (task-1977).** `parameter_list` took the signature's first `(`, which
/// for `pub(crate) fn` is the bracket in the visibility, so the list it counted
/// was the text `crate` and the function was recorded as taking one parameter.
/// Every `pub(crate) fn` and `pub(super) fn` in `crates/` was counted that way.
///
/// The `pub(crate) fn` case is asserted at nine rather than at eight, because
/// nine is the count that has to fail
/// `no_function_takes_more_than_eight_parameters` and eight is the count that
/// has to pass it - an assertion at one or the other alone would still hold if
/// the bracket moved by one.
#[test]
fn parameter_counts_reads_past_a_visibility_bracket() {
    let counts = parameter_counts(
        "pub(crate) fn wide(a: u8, b: u8, c: u8, d: u8, e: u8, f: u8, g: u8, h: u8, i: u8) {}\n\
         pub(super) fn narrow(&self, a: u8, b: u8) {}\n\
         fn generic<F: Fn(u8) -> bool>(first: F, second: u8) {}\n\
         pub fn plain(only: u8) {}\n",
    );
    assert_eq!(
        counts,
        vec![
            ("wide".to_string(), 9),
            ("narrow".to_string(), 2),
            ("generic".to_string(), 2),
            ("plain".to_string(), 1),
        ],
        "the parameter list is the one after the name, and the receiver is not in it"
    );
}

/// Returns where a signature's parameter list opens.
///
/// The search starts after the `fn` keyword and skips the generic list, because
/// both `pub(crate) fn f(` and `fn f<F: Fn(u8) -> bool>(` have a bracket before
/// the one that opens the parameters.
///
/// @param signature - the function's signature, brackets balanced
fn parameter_bracket(signature: &str) -> Option<usize> {
    let keyword = signature.find("fn ")?;
    let mut at = keyword.saturating_add(3);
    let after_keyword = signature.get(at..)?;
    let name_end = after_keyword
        .char_indices()
        .find(|(_, letter)| !(letter.is_alphanumeric() || *letter == '_' || letter.is_whitespace()))
        .map(|(offset, _)| offset)?;
    at = at.saturating_add(name_end);
    if signature
        .get(at..)
        .is_some_and(|rest| rest.starts_with('<'))
    {
        at = at.saturating_add(generic_list_end(signature.get(at..)?)?);
    }
    signature
        .get(at..)
        .and_then(|rest| rest.find('('))
        .map(|offset| at.saturating_add(offset))
}

/// Returns the offset one past a generic list's closing angle bracket.
///
/// The `>` of an `->` inside a bound is an arrow rather than a closing bracket,
/// which is the same distinction `count_parameters` makes one function below.
///
/// @param text - the signature from its opening `<` onwards
fn generic_list_end(text: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut previous = ' ';
    for (offset, letter) in text.char_indices() {
        match letter {
            '<' => depth = depth.saturating_add(1),
            '>' if previous == '-' => {}
            '>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(offset.saturating_add(1));
                }
            }
            _ => {}
        }
        previous = letter;
    }
    None
}

/// Counts the parameters in a signature's bracket list.
///
/// Commas at the list's own depth, so a `Vec<(u8, u8)>` or an
/// `impl Fn(&str) -> bool` is one parameter. A receiver - a parameter that is
/// exactly `self`, `&self`, `&mut self` or `mut self` - is not counted.
///
/// @param inside - the text between the signature's outermost brackets
fn count_parameters(inside: &str) -> usize {
    let mut parameters: Vec<String> = Vec::new();
    let mut held = String::new();
    let mut depth = 0i32;
    let mut previous = ' ';
    for character in inside.chars() {
        match character {
            '(' | '<' | '[' => depth = depth.saturating_add(1),
            // `->` inside a parameter - `impl Fn(&str) -> bool` - is an arrow
            // rather than a closing angle bracket, and counting it as one put
            // the depth below zero for the rest of the list.
            '>' if previous == '-' => {}
            ')' | '>' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parameters.push(std::mem::take(&mut held));
                previous = character;
                continue;
            }
            _ => {}
        }
        previous = character;
        held.push(character);
    }
    if !held.trim().is_empty() {
        parameters.push(held);
    }
    parameters
        .iter()
        .map(|held| held.trim())
        .filter(|held| !held.is_empty())
        .filter(|held| !matches!(*held, "self" | "&self" | "&mut self" | "mut self"))
        .filter(|held| !held.starts_with("&'") || !held.ends_with(" self"))
        .count()
}
