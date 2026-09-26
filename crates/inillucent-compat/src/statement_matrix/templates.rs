//! Placeholder while the matrix is built.
//!
//! Invariant: nothing here yet.

use crate::statement_matrix::case::Case;

/// The generated cases of a family at a strength.
///
/// @param family - the family
/// @param strength - two or three
pub fn generate(family: &str, strength: usize) -> Result<Vec<Case>, String> {
    let _ = (family, strength);
    Ok(Vec::new())
}

/// The strength three cases that strength two does not already produce.
///
/// @param family - the family
pub fn generate_only_triples(family: &str) -> Result<Vec<Case>, String> {
    let _ = family;
    Ok(Vec::new())
}

/// The `counts.toml` text for every family.
pub fn counts_toml() -> Result<String, String> {
    Ok(String::new())
}
