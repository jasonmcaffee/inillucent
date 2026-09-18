//! Every pragma the engine recognises is on exactly one side of the read only
//! line.
//!
//! Invariant: **the two lists in `inillucent_engine::readonly` cover the pragma
//! register exactly.** A pragma in neither would be decided by whichever
//! branch the filter happened to fall through, and a pragma in both would mean
//! the answer depends on which list is read first. Both are the shape
//! task-1979 section 5.2 asked for: "a list, in one place, with a test that
//! every pragma in `docs/pragmas.md` is in exactly one of the two lists".
//!
//! The register is the same one `docs/pragmas.md` is generated from, so this
//! reads the generator's source rather than the page and cannot disagree with
//! it.

use inillucent_engine::readonly::{ALWAYS_WRITING_PRAGMAS, CONNECTION_PRAGMAS, FILE_PRAGMAS};
use inillucent_sql::pragma_register::REGISTER;

/// Every pragma the engine recognises is in exactly one of the two lists.
#[test]
fn every_pragma_is_on_one_side_of_the_read_only_line() {
    assert!(
        REGISTER.len() >= 60,
        "the register holds {} pragmas, which means this test is reading the wrong thing",
        REGISTER.len()
    );
    let mut missing: Vec<&str> = Vec::new();
    let mut twice: Vec<&str> = Vec::new();
    for entry in REGISTER {
        let connection = CONNECTION_PRAGMAS
            .iter()
            .any(|name| name.eq_ignore_ascii_case(entry.name));
        let file = FILE_PRAGMAS
            .iter()
            .any(|name| name.eq_ignore_ascii_case(entry.name));
        match (connection, file) {
            (false, false) => missing.push(entry.name),
            (true, true) => twice.push(entry.name),
            _ => {}
        }
    }
    assert!(
        missing.is_empty(),
        "these pragmas are in neither `CONNECTION_PRAGMAS` nor `FILE_PRAGMAS`, so what a read \
         only connection does with them is whatever the filter falls through to:\n  {}",
        missing.join("\n  ")
    );
    assert!(
        twice.is_empty(),
        "these pragmas are in both lists:\n  {}",
        twice.join("\n  ")
    );
}

/// Neither list names a pragma the engine does not recognise.
///
/// **The other direction, and it is not decoration.** A name removed from the
/// register and left in a list is a rule about a pragma that no longer exists,
/// which reads as coverage and is not.
#[test]
fn neither_list_names_a_pragma_the_engine_has_not_got() {
    let mut unknown: Vec<&str> = Vec::new();
    for name in CONNECTION_PRAGMAS.iter().chain(FILE_PRAGMAS.iter()) {
        if !REGISTER
            .iter()
            .any(|entry| entry.name.eq_ignore_ascii_case(name))
        {
            unknown.push(name);
        }
    }
    assert!(
        unknown.is_empty(),
        "these names are in a read only list and in no pragma the engine recognises:\n  {}",
        unknown.join("\n  ")
    );
}

/// The pragmas that write with no argument are all file pragmas.
#[test]
fn a_pragma_that_writes_with_no_argument_is_a_file_pragma() {
    for name in ALWAYS_WRITING_PRAGMAS {
        assert!(
            FILE_PRAGMAS
                .iter()
                .any(|other| other.eq_ignore_ascii_case(name)),
            "{name} writes with no argument and is not among the file pragmas"
        );
    }
}
