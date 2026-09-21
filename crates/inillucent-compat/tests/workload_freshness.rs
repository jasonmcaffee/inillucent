//! The checked-in workload is still what the consumer's source says.
//!
//! Invariant: **`tests/workloads/nikaya/statements.sql` is a copy, and a copy
//! goes stale, so something has to notice.** Nikaya adds a statement, nobody
//! re-runs the extractor, and the corpus this repository grades itself against
//! is the corpus Nikaya had in September - which reads as coverage of a
//! consumer and is coverage of a consumer's past.
//!
//! ## Why it runs the extractor rather than re-implementing it
//!
//! There is one extractor, `tools/extract-nikaya-workload.py`, and this asks it
//! whether the checked-in file is what it would write. Writing a second
//! extractor in Rust would be two implementations of "what counts as a
//! statement literal" that agree on the day they are written.
//!
//! ## Loud here, silent on a clone
//!
//! Nikaya is not part of this repository and most machines that build it do not
//! have it. So the extractor answers three ways - the file matches, the file is
//! stale, the checkout is not here - and only the middle one is a failure. The
//! third prints `; skipping`, which is what `--strict` counts, so a machine
//! that cannot check this says so rather than reporting a pass.

use std::path::PathBuf;
use std::process::Command;

use inillucent_compat::workspace_root;

/// Where Nikaya is, unless `NIKAYA_ROOT` says otherwise.
///
/// The extractor holds the same default, and reads the same variable; this is
/// here so the skip message can name the path it looked at.
fn nikaya_root() -> PathBuf {
    match std::env::var("NIKAYA_ROOT") {
        Ok(path) => PathBuf::from(path),
        Err(_) => PathBuf::from("C:/jason/dev/nikaya/server"),
    }
}

/// The interpreter to run the extractor with, if one is on the path.
fn python() -> Option<&'static str> {
    for name in ["python", "python3"] {
        if Command::new(name)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
        {
            return Some(name);
        }
    }
    None
}

/// The workload file holds what Nikaya's source holds.
///
/// **Remove a statement from `statements.sql` and this fails**, which is rule
/// 1.5: the guard is checked by taking away the thing it guards. The check was
/// run both ways on the machine this was written on.
#[test]
fn the_workload_matches_the_consumers_source() {
    let Some(python) = python() else {
        inillucent_compat::differential::skipping(
            "no python on the path, so the Nikaya workload cannot be re-extracted",
        );
        return;
    };
    let root = nikaya_root();
    if !root.join("src").is_dir() {
        inillucent_compat::differential::skipping(&format!(
            "the Nikaya checkout is not at {}, so the workload cannot be compared to it",
            root.display()
        ));
        return;
    }

    let script = workspace_root().join("tools/extract-nikaya-workload.py");
    let ran = Command::new(python)
        .arg(&script)
        .arg("--check")
        .arg("--nikaya")
        .arg(&root)
        .output()
        .unwrap_or_else(|why| panic!("{} did not run: {why}", script.display()));
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&ran.stdout),
        String::from_utf8_lossy(&ran.stderr)
    );

    match ran.status.code() {
        Some(0) => {}
        // The extractor decided the checkout was not usable after all - a
        // partial clone, a directory that is there and empty. Its own word for
        // that is 2, and it is a skip rather than a failure for the same reason
        // the directory check above is.
        Some(2) => inillucent_compat::differential::skipping(&format!(
            "the extractor could not read {}: {said}",
            root.display()
        )),
        _ => panic!(
            "tests/workloads/nikaya/statements.sql is not what Nikaya's source says it should \
             be. The corpus this repository grades a consumer's statements against is a copy \
             of an older Nikaya, which reads as coverage of the consumer and is coverage of \
             the consumer's past:\n{said}"
        ),
    }
}
