//! The header, the manifest and the implementation are one surface.
//!
//! Three files describe this ABI and each is written by hand:
//! `include/inillucent_driver.h` is what a binding compiles against,
//! `drivers/abi.toml` is what each symbol promises, and this crate's sources are what
//! actually exists. Any two of them can drift from the third, and each way of
//! drifting fails somewhere unhelpful:
//!
//! - **declared and not implemented** - a linker error for whoever binds to it,
//!   discovered in their build rather than in ours;
//! - **implemented and not declared** - a symbol nobody can call, and nobody
//!   knows is a promise until somebody finds it with `nm` and starts calling
//!   it;
//! - **not in the manifest** - a symbol with no stated stability, which a
//!   binding author has no way to know whether they may rely on.
//!
//! So all three are read and compared. This is the same instrument
//! `docs/invariants/layering.toml` is: *"checked, not documented: an
//! architecture rule that is only written down is a rule that has already been
//! broken somewhere."*
//!
//! Invariant: **the C ABI's published manifest and its implementation say the
//! same thing.** A header that promises a symbol the library does not export,
//! or a note that says `unsupported` about a function that works, is a contract
//! a caller writes against and then finds untrue at run time.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Returns the crate's own directory.
fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Returns the names the header declares as functions.
///
/// Block comments are stripped first. The header's prose names its own
/// functions constantly - it would be a poor header if it did not - and a
/// parser that read those would report every mention as a declaration.
///
/// @param header - the header's text
fn declared(header: &str) -> BTreeSet<String> {
    let stripped = strip_block_comments(header);
    let mut names = BTreeSet::new();
    let bytes: Vec<char> = stripped.chars().collect();
    let mut word = String::new();
    for (at, character) in bytes.iter().enumerate() {
        if character.is_ascii_alphanumeric() || *character == '_' {
            word.push(*character);
            continue;
        }
        // A name followed by `(` is a call or a declaration; a name followed by
        // anything else is a type or a mention.
        if word.starts_with("inillucent_") && *character == '(' {
            names.insert(word.clone());
        }
        let _ = at;
        word.clear();
    }
    // The opaque handle typedefs are `typedef struct inillucent_db
    // inillucent_db;` and are never followed by `(`, so they cannot appear
    // here - but a future one might, and a type in the function list would be
    // a confusing failure rather than a caught one.
    for handle in HANDLES {
        names.remove(*handle);
    }
    names
}

/// The opaque handle typedefs, which are types rather than functions.
const HANDLES: &[&str] = &[
    "inillucent_db",
    "inillucent_conn",
    "inillucent_stmt",
    "inillucent_rows",
    "inillucent_txn",
    "inillucent_error",
];

/// Removes `/* ... */` comments.
///
/// @param text - the source
fn strip_block_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("/*") {
        out.push_str(rest.get(..start).unwrap_or_default());
        let after = rest.get(start.saturating_add(2)..).unwrap_or_default();
        match after.find("*/") {
            Some(end) => rest = after.get(end.saturating_add(2)..).unwrap_or_default(),
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Returns the names the crate exports, with the `#[no_mangle]` marker.
///
/// Read from the source rather than from the built library, because the check
/// has to run without having linked anything and because what is being compared
/// is the *declaration*: a symbol the compiler emitted for some other reason is
/// not a promise this ABI made.
///
/// @param source - every Rust source of the crate, concatenated
fn exported(source: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut marked = false;
    for line in source.lines() {
        let line = line.trim();
        if line == "#[no_mangle]" {
            marked = true;
            continue;
        }
        if !marked {
            continue;
        }
        if let Some(rest) = line
            .strip_prefix("pub extern \"C\" fn ")
            .or_else(|| line.strip_prefix("pub unsafe extern \"C\" fn "))
        {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            names.insert(name);
        }
        marked = false;
    }
    names
}

/// Returns each symbol's stability, read from the manifest.
///
/// A deliberately small reader for the one shape the file uses, for the reason
/// `inillucent-compat`'s own manifest reader gives: the check must run with no
/// network, no lockfile and no dependency of its own.
///
/// @param manifest - `abi.toml`'s text
fn promised(manifest: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut name: Option<String> = None;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if line == "[[symbol]]" {
            name = None;
            continue;
        }
        if let Some(value) = line.strip_prefix("name = ") {
            name = value.trim().trim_matches('"').to_string().into();
            continue;
        }
        if let Some(value) = line.strip_prefix("stability = ") {
            if let Some(held) = name.clone() {
                out.insert(held, value.trim().trim_matches('"').to_string());
            }
        }
    }
    out
}

/// Returns each symbol's `note`, when it has one.
///
/// Separate from [`promised`], which reads the stability: a note is prose and
/// can run over several lines, so it is joined into one string rather than read
/// line by line. It is what `no_note_calls_a_symbol_unsupported_that_the_capability_table_says_works`
/// checks against the capability table (task-1932, M9).
///
/// @param manifest - the text of `drivers/abi.toml`
fn noted(manifest: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut name: Option<String> = None;
    let mut note: Option<String> = None;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        if trimmed == "[[symbol]]" {
            name = None;
            note = None;
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("name = ") {
            name = Some(value.trim().trim_matches('"').to_string());
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("note = ") {
            note = Some(value.trim().trim_start_matches('"').to_string());
        } else if let Some(held) = note.as_mut() {
            // A continuation line of the same note.
            held.push(' ');
            held.push_str(trimmed.trim_end_matches('"'));
        }
        if let (Some(held), Some(said)) = (name.clone(), note.clone()) {
            out.insert(held, said);
        }
    }
    out
}

/// Reads a file beside the crate, or beside the workspace.
///
/// @param relative - the path from the crate root
fn read(relative: &str) -> String {
    let path: PathBuf = crate_root().join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

/// Returns every Rust source of this crate, concatenated.
///
/// **The whole crate, not `lib.rs` (task-1962, A8).** The fifty entry points
/// were in one file when these checks were written and are in four modules
/// under `capi/` now. Walking the tree is what keeps the checks true the next
/// time one moves.
///
/// Each file is separated by a newline, so a scan that walks lines cannot run
/// the last line of one file into the first line of the next.
fn crate_sources() -> String {
    let mut out = String::new();
    let mut pending = vec![std::path::PathBuf::from("src")];
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|held| held == "rs") {
                paths.push(path);
            }
        }
    }
    // Sorted, so a line number this reports is the same on every machine.
    paths.sort();
    for path in paths {
        out.push_str(&read(&path.to_string_lossy()));
        out.push('\n');
    }
    out
}

/// Every symbol is in the header, the manifest and the implementation.
#[test]
fn the_header_the_manifest_and_the_implementation_agree() {
    let header = declared(&read("include/inillucent_driver.h"));
    let source = exported(&crate_sources());
    let manifest = promised(&read("../abi.toml"));
    let manifest_names: BTreeSet<String> = manifest.keys().cloned().collect();

    let mut wrong: Vec<String> = Vec::new();
    for name in header.difference(&source) {
        wrong.push(format!(
            "`{name}` is declared in inillucent_driver.h and not implemented - every binding \
             that calls it gets a linker error in its own build rather than ours"
        ));
    }
    for name in source.difference(&header) {
        wrong.push(format!(
            "`{name}` is exported and not declared in inillucent_driver.h - a promise nobody \
             can find and nobody knows was made"
        ));
    }
    for name in header.difference(&manifest_names) {
        wrong.push(format!(
            "`{name}` is in the header and not in abi.toml, so it states no stability and a \
             binding author has no way to know whether they may rely on it"
        ));
    }
    for name in manifest_names.difference(&header) {
        wrong.push(format!(
            "`{name}` is in abi.toml and not in the header - it promises the stability of \
             something that is not there"
        ));
    }
    assert!(
        wrong.is_empty(),
        "the C ABI's three descriptions disagree:\n  - {}",
        wrong.join("\n  - ")
    );
    assert!(
        header.len() >= 40,
        "only {} symbols were found, which means the parser broke rather than the ABI shrinking",
        header.len()
    );
}

/// Every stability is one of the two the manifest defines.
///
/// A typo in a stability is a symbol with no promise at all, and it would read
/// as one that had a promise.
#[test]
fn every_stability_is_one_of_the_two_that_mean_something() {
    let manifest = promised(&read("../abi.toml"));
    for (name, stability) in &manifest {
        assert!(
            stability == "stable" || stability == "provisional",
            "`{name}` is `{stability}`, which is neither `stable` nor `provisional`"
        );
    }
    let provisional: Vec<&String> = manifest
        .iter()
        .filter(|(_, stability)| stability.as_str() == "provisional")
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        provisional,
        vec!["inillucent_cancel"],
        "the provisional set changed. That is allowed, but it is a decision: a symbol moving \
         from stable to provisional is a promise being taken back."
    );
}

/// The version the manifest states and the version the library reports are the
/// same number.
#[test]
fn the_manifest_and_the_library_state_the_same_abi_version() {
    let manifest = read("../abi.toml");
    let stated = manifest
        .lines()
        .find_map(|line| line.trim().strip_prefix("abi_version = "))
        .map(|value| value.trim().trim_matches('"').to_string())
        .expect("abi.toml states an abi_version");
    let reported = inillucent_driver_capi::inillucent_abi_version();
    let major = reported / 1_000_000;
    let minor = (reported / 1_000) % 1_000;
    let patch = reported % 1_000;
    assert_eq!(
        stated,
        format!("{major}.{minor}.{patch}"),
        "abi.toml says {stated} and inillucent_abi_version() reports {reported}"
    );
}

/// The header's status and value constants are the numbers the driver's own
/// enums carry.
///
/// **The numbers are the ABI.** A binding reads them out of the header and
/// compares them to what a call returned, so a variant renumbered on the Rust
/// side without the header changing would make every binding in every language
/// silently misread every error - and nothing else would catch it, because both
/// sides would still compile.
#[test]
fn the_headers_constants_are_the_drivers_own_numbers() {
    use inillucent_driver::{Status, ValueKind};
    let header = read("include/inillucent_driver.h");
    let defined = |name: &str| -> i64 {
        header
            .lines()
            .find_map(|line| {
                let line = line.trim();
                let rest = line.strip_prefix(&format!("#define {name} "))?;
                let value = rest.split_whitespace().next()?;
                value.trim_matches(['(', ')']).parse::<i64>().ok()
            })
            .unwrap_or_else(|| panic!("inillucent_driver.h defines no {name}"))
    };

    assert_eq!(defined("INILLUCENT_OK"), 0);
    for (name, status) in [
        ("INILLUCENT_UNSUPPORTED", Status::Unsupported),
        ("INILLUCENT_SYNTAX", Status::Syntax),
        ("INILLUCENT_NOT_FOUND", Status::NotFound),
        ("INILLUCENT_CONSTRAINT", Status::Constraint),
        ("INILLUCENT_READONLY", Status::ReadOnly),
        ("INILLUCENT_BUSY", Status::Busy),
        ("INILLUCENT_INTERRUPTED", Status::Interrupted),
        ("INILLUCENT_CORRUPT", Status::Corrupt),
        ("INILLUCENT_IO", Status::Io),
        ("INILLUCENT_FULL", Status::Full),
        ("INILLUCENT_TOO_BIG", Status::TooBig),
        ("INILLUCENT_INVALID_STATE", Status::InvalidState),
        ("INILLUCENT_INTERNAL", Status::Internal),
    ] {
        assert_eq!(
            defined(name),
            status as i64,
            "{name} in the header is not what Status::{status:?} is in the driver"
        );
    }
    for (name, kind) in [
        ("INILLUCENT_NULL", ValueKind::Null),
        ("INILLUCENT_INTEGER", ValueKind::Integer),
        ("INILLUCENT_REAL", ValueKind::Real),
        ("INILLUCENT_TEXT", ValueKind::Text),
        ("INILLUCENT_BLOB", ValueKind::Blob),
    ] {
        assert_eq!(
            defined(name),
            kind as i64,
            "{name} in the header is not what ValueKind::{kind:?} is in the driver"
        );
    }
    for (name, support) in [
        ("INILLUCENT_SUPPORT_NO", inillucent_driver::Support::No),
        ("INILLUCENT_SUPPORT_YES", inillucent_driver::Support::Yes),
        (
            "INILLUCENT_SUPPORT_PARTIAL",
            inillucent_driver::Support::Partial,
        ),
    ] {
        assert_eq_il(defined(name), support as i64, name);
    }
}

/// Asserts two numbers are equal, naming the constant.
///
/// @param header - what the header defines
/// @param driver - what the driver's enum carries
/// @param name - the constant's name
fn assert_eq_il(header: i64, driver: i64, name: &str) {
    assert_eq!(
        header, driver,
        "{name} in the header is not what the driver's enum carries"
    );
}

/// The path helper is used, which keeps the unused-import lint honest.
#[test]
fn the_header_is_where_it_is_expected_to_be() {
    let path: &Path = &crate_root().join("include/inillucent_driver.h");
    assert!(path.is_file(), "{} is not there", path.display());
}

/// M9 (task-1920): a note that says UNSUPPORTED about a symbol the capability
/// table says works is a contract a caller writes against and then finds
/// untrue.
///
/// **`inillucent_cancel` was exactly that.** `abi.toml` said "Returns
/// UNSUPPORTED today: the engine runs a statement whole rather than a row at a
/// time, so there is no point at which a cancel flag could be read", and the C
/// ABI's own doc comment said "which this engine cannot do". Both had been true
/// and had stopped being true: `capability.rs` records `cancel` as `Partial`
/// with a note describing where the flag is read, and the implementation
/// returns `Ok`. A binding written against the manifest would have wired a Stop
/// button it believed did not work.
///
/// The check is on the *pair*, not on the wording of either: a note that claims
/// a symbol is unsupported while the capability table says otherwise is what
/// fails, whichever of the two moved.
#[test]
fn no_note_calls_a_symbol_unsupported_that_the_capability_table_says_works() {
    let notes = noted(&read("../abi.toml"));

    // The capability names the driver declares, and how well each is supported.
    let capabilities: Vec<(String, String)> = inillucent_driver::capability::CAPABILITIES
        .iter()
        .map(|held| (held.name.to_string(), format!("{:?}", held.support)))
        .collect();
    assert!(
        !capabilities.is_empty(),
        "the driver declares no capabilities at all, so this check is looking in \
         the wrong place rather than finding nothing wrong"
    );

    let mut wrong: Vec<String> = Vec::new();
    for (symbol, note) in &notes {
        let said_unsupported = note.contains("UNSUPPORTED")
            || note.contains("which this engine cannot do")
            || note.contains("cannot be done");
        if !said_unsupported {
            continue;
        }
        // Which capability this symbol is about: the longest declared name the
        // symbol's own name ends with, so `inillucent_cancel` matches `cancel`
        // and nothing matches by accident on a two-letter prefix.
        let about = capabilities
            .iter()
            .filter(|(name, _)| symbol.ends_with(name.as_str()))
            .max_by_key(|(name, _)| name.len());
        let Some((name, support)) = about else {
            continue;
        };
        if support != "No" {
            wrong.push(format!(
                "{symbol}: the note says it is unsupported, and \
                 `capability::supports(\"{name}\")` says {support}"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "these notes in drivers/abi.toml disagree with the capability table:\n{}",
        wrong.join("\n")
    );
}

/// M9 (task-1920): every exported entry point catches a panic.
///
/// **The claim was in the code and was false by 38 of 53.**
/// `guarded`'s doc said "a panic unwinding into a C caller is undefined
/// behaviour, so every entry point wraps its body in this", and fifteen did.
/// The rest could not: `guarded` answers an `i32` status and writes into an
/// error out-parameter, and those return a count, a pointer, a `f64` or nothing
/// at all, most with nowhere to put an error. `guarded_value` is the companion
/// for those, and this is the check that keeps the sentence true.
///
/// It is a grep because the property is about *every* entry point, and a type
/// cannot be put on "there is no other one". A new `extern "C" fn` that forgets
/// the guard fails here on the day it is written.
#[test]
fn every_exported_entry_point_catches_a_panic() {
    let source = crate_sources();
    let lines: Vec<&str> = source.lines().collect();
    let mut unguarded: Vec<String> = Vec::new();
    let mut checked = 0usize;

    for (at, line) in lines.iter().enumerate() {
        if !line.starts_with("pub extern \"C\" fn ")
            && !line.starts_with("pub unsafe extern \"C\" fn ")
        {
            continue;
        }
        let Some(name) = line
            .split("fn ")
            .nth(1)
            .and_then(|rest| rest.split('(').next())
        else {
            continue;
        };
        checked += 1;
        // The body runs from this line to the next line that is exactly `}`,
        // which is the closing brace of a top-level item in a formatted file.
        let end = lines
            .iter()
            .enumerate()
            .skip(at)
            .find(|(_, held)| **held == "}")
            .map(|(index, _)| index)
            .unwrap_or(lines.len());
        let body = lines.get(at..end).unwrap_or_default().join("\n");
        if !body.contains("guarded(") && !body.contains("guarded_value(") {
            unguarded.push(format!("line {}: {name}", at.saturating_add(1)));
        }
    }

    assert!(
        checked >= 40,
        "the C ABI should export many entry points, found {checked} - which means \
         this scan is matching nothing rather than finding nothing wrong"
    );
    assert!(
        unguarded.is_empty(),
        "these entry points let a panic unwind into a C caller, which is undefined \
         behaviour:\n{}\nA function answering a status takes `guarded`; one \
         answering a value takes `guarded_value`.",
        unguarded.join("\n")
    );
}
