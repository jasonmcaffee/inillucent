//! The BOM's variables index, written by hand because `apple-bom` writes it in
//! a shape its own reader cannot read.
//!
//! A variable entry is a big-endian `u32` block index, a `u8` name length, and
//! that many bytes of name. `BomVar::write` writes the name **and** a NUL
//! terminator and sets the length to the name plus one; `BomVar::try_from_ctx`
//! then reads `length` bytes as the name, so the name comes back with a NUL
//! inside it and `find_variable("Paths")` never matches. Reading is the half of
//! `apple-bom` that has been run against BOMs Apple produced — `odumpbom` is
//! built on it — so the reader is taken as the definition and the writer here
//! matches it.
//!
//! `a_written_bom_reads_back_with_the_same_paths` in `bom.rs` is what holds
//! this: it asks the reader for the `Paths` variable by name, which only
//! resolves if the bytes written here are the shape the reader expects.

use {
    anyhow::{bail, Result},
    std::io::Write,
};

/// One variable: a name and the block its data starts at.
pub struct Variable {
    /// The variable's name, such as `Paths`.
    pub name: String,
    /// The index of the block holding the variable's data.
    pub block_index: u32,
}

/// Serialises the variables index.
///
/// @param variables - every variable, in the order they were added
pub fn to_vec(variables: &[Variable]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.write_all(&(variables.len() as u32).to_be_bytes())?;
    for variable in variables {
        let name = variable.name.as_bytes();
        if name.len() > 255 {
            bail!("{} is too long to be a BOM variable name", variable.name);
        }
        out.write_all(&variable.block_index.to_be_bytes())?;
        out.write_all(&[name.len() as u8])?;
        out.write_all(name)?;
    }
    Ok(out)
}
