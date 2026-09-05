//! Builds the fixture corpus with the pinned SQLite shell, and verifies it.
//!
//! Invariant: this tool is the only thing that writes `compat/fixtures`, and
//! it writes them with SQLite 3.53.4 and nothing else. Run with `--verify` it
//! writes nothing and reports whether the checked-in corpus matches the
//! manifest's digests, which is what the test suite runs so a corpus edited by
//! hand is caught rather than trusted.
//!
//! Usage:
//!   `cargo run -p inillucent-compat --bin inillucent-fixtures`             regenerate
//!   `cargo run -p inillucent-compat --bin inillucent-fixtures -- --verify` check only

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use inillucent_compat::fixtures::{
    self, apply_damage, malformed_fixtures, page_size_from_name, valid_fixtures,
};
use inillucent_compat::hash::sha256_hex;
use inillucent_compat::workspace_root;

/// Builds or verifies the corpus.
fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let verify = arguments.iter().any(|argument| argument == "--verify");
    let root = workspace_root();
    match run(&root, verify) {
        Ok(report) => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

/// Finds the pinned SQLite shell.
fn pinned_shell(root: &Path) -> Result<PathBuf, String> {
    if let Ok(explicit) = std::env::var("INILLUCENT_SQLITE_SHELL") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Ok(path);
        }
    }
    let directory = root.join(".sqlite-ref/3.53.4/shell");
    // The name to try first is the one this platform runs. Both are on disk
    // when the workspace is shared between Windows and WSL, and a Linux process
    // that picks the `.exe` gets a *Windows* SQLite through binfmt interop -
    // which then cannot open a Linux path, and says "unable to open database"
    // for a reason that has nothing to do with the database.
    let names: [&str; 2] = if cfg!(windows) {
        ["sqlite3.exe", "sqlite3"]
    } else {
        ["sqlite3", "sqlite3.exe"]
    };
    for name in names {
        let path = directory.join(name);
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(format!(
        "the pinned SQLite shell is not at {}; run tools/sqlite-reference.ps1 or .sh",
        directory.display()
    ))
}

/// Generates or verifies the whole corpus.
fn run(root: &Path, verify: bool) -> Result<String, String> {
    let corpus = fixtures::corpus_root(root);
    if !verify {
        std::fs::create_dir_all(&corpus).map_err(|error| error.to_string())?;
    }
    let shell = if verify {
        None
    } else {
        Some(pinned_shell(root)?)
    };

    let mut manifest = String::new();
    manifest.push_str(
        "# The fixture corpus.\n\
         #\n\
         # Every valid fixture was written by the pinned SQLite 3.53.4 binary; every\n\
         # malformed one is a single named edit to a valid fixture, applied by\n\
         # crates/inillucent-compat/src/fixtures.rs. Regenerate with:\n\
         #   cargo run -p inillucent-compat --bin inillucent-fixtures\n\
         # and verify the checked-in copies with `-- --verify`.\n\
         #\n\
         # This file is generated. Editing a digest here rather than regenerating the\n\
         # corpus is how a fixture stops being what SQLite wrote.\n\n\
         schema_version = 1\n\
         reference = \"sqlite-3.53.4\"\n\n",
    );

    let mut problems: Vec<String> = Vec::new();
    let mut written = 0usize;

    for fixture in valid_fixtures() {
        let target = corpus.join(fixture.name);
        let page_size = page_size_from_name(fixture.name)
            .ok_or_else(|| format!("{} does not declare a page size", fixture.name))?;
        let encoding = encoding_from_name(fixture.name);
        let image = if let Some(shell) = &shell {
            let image = build_with_sqlite(shell, &fixture, page_size, encoding)?;
            std::fs::write(&target, &image).map_err(|error| error.to_string())?;
            written = written.saturating_add(1);
            image
        } else {
            std::fs::read(&target).map_err(|error| format!("{}: {error}", target.display()))?
        };
        manifest.push_str(&format!(
            "[[fixture]]\nname = \"{}\"\nkind = \"valid\"\npage_size = {page_size}\n\
             encoding = \"{encoding}\"\nbytes = {}\nsha256 = \"{}\"\npurpose = \"{}\"\n\n",
            fixture.name,
            image.len(),
            sha256_hex(&image),
            fixture.purpose
        ));
    }

    for fixture in malformed_fixtures() {
        let base_path = corpus.join(fixture.base);
        let mut image = std::fs::read(&base_path)
            .map_err(|error| format!("{}: {error}", base_path.display()))?;
        let page_size = page_size_from_name(fixture.base)
            .ok_or_else(|| format!("{} does not declare a page size", fixture.base))?;
        apply_damage(&mut image, &fixture.damage, page_size)
            .map_err(|reason| format!("{}: {reason}", fixture.name))?;
        let target = corpus.join(fixture.name);
        if shell.is_some() {
            std::fs::write(&target, &image).map_err(|error| error.to_string())?;
            written = written.saturating_add(1);
        } else {
            let on_disk =
                std::fs::read(&target).map_err(|error| format!("{}: {error}", target.display()))?;
            if on_disk != image {
                problems.push(format!(
                    "{} on disk is not the damage its definition describes",
                    fixture.name
                ));
            }
        }
        manifest.push_str(&format!(
            "[[fixture]]\nname = \"{}\"\nkind = \"malformed\"\nbase = \"{}\"\n\
             fails_at_open = {}\nbytes = {}\nsha256 = \"{}\"\nlie = \"{}\"\n\n",
            fixture.name,
            fixture.base,
            fixture.fails_at_open,
            image.len(),
            sha256_hex(&image),
            fixture.lie
        ));
    }

    let manifest_path = corpus.join("manifest.toml");
    if shell.is_some() {
        std::fs::write(&manifest_path, manifest.as_bytes()).map_err(|error| error.to_string())?;
        Ok(format!(
            "wrote {written} fixtures and {}",
            manifest_path.display()
        ))
    } else {
        let on_disk = std::fs::read_to_string(&manifest_path)
            .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
        if on_disk.replace("\r\n", "\n") != manifest.replace("\r\n", "\n") {
            problems.push("the manifest does not describe the corpus on disk".to_string());
        }
        if problems.is_empty() {
            Ok(format!(
                "the corpus matches its manifest ({} fixtures)",
                valid_fixtures().len() + malformed_fixtures().len()
            ))
        } else {
            Err(problems.join("\n"))
        }
    }
}

/// Returns the encoding a fixture's name asks for.
fn encoding_from_name(name: &str) -> &'static str {
    if name.contains("utf16le") {
        "UTF-16le"
    } else if name.contains("utf16be") {
        "UTF-16be"
    } else {
        "UTF-8"
    }
}

/// Runs the pinned shell to build one fixture and returns the file's bytes.
fn build_with_sqlite(
    shell: &Path,
    fixture: &fixtures::ValidFixture,
    page_size: usize,
    encoding: &str,
) -> Result<Vec<u8>, String> {
    let scratch = std::env::temp_dir().join(format!(
        "inillucent-fixture-{}-{}",
        std::process::id(),
        fixture.name
    ));
    let _ = std::fs::remove_file(&scratch);
    let _ = std::fs::remove_file(scratch.with_extension("db-journal"));

    let mut script = String::new();
    // The page size and the encoding must both be chosen before the file has
    // any content, because neither can be changed afterwards without a VACUUM.
    script.push_str(&format!("PRAGMA page_size = {page_size};\n"));
    script.push_str(&format!("PRAGMA encoding = '{encoding}';\n"));
    for command in fixture.dot_commands {
        script.push_str(command);
        script.push('\n');
    }
    script.push_str("BEGIN;\n");
    script.push_str(fixture.sql);
    script.push_str("COMMIT;\n");
    if !fixture.trailing_sql.is_empty() {
        script.push_str(fixture.trailing_sql);
        script.push('\n');
    }
    // A deterministic file needs the journal gone and the header settled.
    script.push_str("PRAGMA journal_mode = delete;\n");
    script.push_str(".quit\n");

    let script_path = scratch.with_extension("sql");
    std::fs::write(&script_path, script.as_bytes()).map_err(|error| error.to_string())?;

    let output = Command::new(shell)
        .arg("-batch")
        .arg("-init")
        .arg(&script_path)
        .arg(&scratch)
        .arg(".quit")
        .output()
        .map_err(|error| format!("could not run {}: {error}", shell.display()))?;
    if !output.status.success() {
        return Err(format!(
            "{} failed for {}: {}{}",
            shell.display(),
            fixture.name,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    // The shell exits zero after a parse error in an init script, and writes
    // the complaint to stderr. Ignoring that is how a fixture ends up empty
    // and checked in, so any mention of an error is fatal here.
    let noise = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if noise.to_ascii_lowercase().contains("error") {
        return Err(format!("{} reported: {noise}", fixture.name));
    }

    let image = std::fs::read(&scratch).map_err(|error| {
        format!(
            "{} produced no database at {}: {error}",
            fixture.name,
            scratch.display()
        )
    })?;
    if image.len() < 100 {
        return Err(format!(
            "{} produced a {}-byte file, which cannot even hold a header",
            fixture.name,
            image.len()
        ));
    }
    let _ = std::fs::remove_file(&scratch);
    let _ = std::fs::remove_file(&script_path);
    Ok(image)
}
