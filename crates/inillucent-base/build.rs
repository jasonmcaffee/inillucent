//! Generates the stable error table from `compat/errors.toml`.
//!
//! Invariant: the Rust error table is never hand-edited. `compat/errors.toml`
//! is the single source of truth, so a code that exists in the manifest but not
//! in the engine - or the reverse - cannot happen.
//!
//! The parser below understands only the strict subset the manifest uses:
//! top-level `key = value` scalars and `[[array]]` tables of scalars. Anything
//! else is a hard error rather than a silent skip, because a manifest row that
//! is quietly ignored would take a real SQLite result code out of the engine.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// One `[[array]]` table: its ordered key/value pairs as written in the file.
type Table = BTreeMap<String, Scalar>;

/// The three scalar kinds the manifest uses.
#[derive(Clone, Debug, PartialEq)]
enum Scalar {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl Scalar {
    /// Returns the string body, or panics with the offending value; a manifest
    /// with the wrong type for a column is a build-stopping mistake.
    fn as_str(&self) -> &str {
        match self {
            Scalar::Str(text) => text,
            other => panic!("expected a string in compat/errors.toml, found {other:?}"),
        }
    }

    /// Returns the integer body, or panics with the offending value.
    fn as_int(&self) -> i64 {
        match self {
            Scalar::Int(value) => *value,
            other => panic!("expected an integer in compat/errors.toml, found {other:?}"),
        }
    }

    /// Returns the boolean body, or panics with the offending value.
    fn as_bool(&self) -> bool {
        match self {
            Scalar::Bool(value) => *value,
            other => panic!("expected a boolean in compat/errors.toml, found {other:?}"),
        }
    }
}

/// Parses one scalar literal, rejecting every TOML form the manifest does not
/// use so an unnoticed type change cannot reach the generated table.
fn parse_scalar(raw: &str) -> Scalar {
    let trimmed = raw.trim();
    if trimmed == "true" {
        return Scalar::Bool(true);
    }
    if trimmed == "false" {
        return Scalar::Bool(false);
    }
    if let Some(body) = trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return Scalar::Str(body.replace("\\\"", "\"").replace("\\\\", "\\"));
    }
    match trimmed.parse::<i64>() {
        Ok(value) => Scalar::Int(value),
        Err(_) => panic!("unsupported literal in compat/errors.toml: {trimmed}"),
    }
}

/// Splits the manifest into top-level scalars and the named `[[array]]` tables,
/// preserving file order inside each array.
fn parse_manifest(text: &str) -> (Table, BTreeMap<String, Vec<Table>>) {
    let mut top = Table::new();
    let mut arrays: BTreeMap<String, Vec<Table>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix("[[").and_then(|s| s.strip_suffix("]]")) {
            let name = name.trim().to_string();
            arrays.entry(name.clone()).or_default().push(Table::new());
            current = Some(name);
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            panic!("unsupported line in compat/errors.toml: {line}");
        };
        let (key, scalar) = (key.trim().to_string(), parse_scalar(value));
        match current.as_ref() {
            None => {
                top.insert(key, scalar);
            }
            Some(name) => {
                let rows = arrays.get_mut(name).expect("array was created above");
                let row = rows.last_mut().expect("array has at least one table");
                row.insert(key, scalar);
            }
        }
    }
    (top, arrays)
}

/// Writes the `PrimaryCode` enum plus its numeric conversions.
fn emit_primary_enum(out: &mut String, rows: &[Table]) {
    out.push_str("/// A SQLite primary result code.\n");
    out.push_str("///\n/// Generated from `compat/errors.toml`; do not edit by hand.\n");
    out.push_str("#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]\n");
    out.push_str("#[non_exhaustive]\npub enum PrimaryCode {\n");
    for row in rows {
        let name = row["name"].as_str();
        let c_name = row["c_name"].as_str();
        let code = row["code"].as_int();
        let _ = writeln!(out, "    /// `{c_name}` ({code}).\n    {name},");
    }
    out.push_str("}\n\n");
}

/// Writes the primary-code lookup table used for numeric conversion.
fn emit_primary_table(out: &mut String, rows: &[Table]) {
    out.push_str("/// One row of the generated primary-code table.\n");
    out.push_str("#[derive(Clone, Copy, Debug)]\npub struct PrimaryRow {\n");
    out.push_str("    /// The Rust enum variant this row describes.\n    pub code: PrimaryCode,\n");
    out.push_str("    /// The numeric value SQLite returns.\n    pub value: i32,\n");
    out.push_str("    /// The C macro name, used by the ABI surface and the oracle.\n    pub c_name: &'static str,\n");
    out.push_str(
        "    /// The default English message for the code.\n    pub message: &'static str,\n",
    );
    out.push_str(
        "    /// Whether the connection may still be used.\n    pub connection_usable: bool,\n",
    );
    out.push_str("    /// Whether the statement may be reset and re-stepped.\n    pub statement_resettable: bool,\n");
    out.push_str("    /// Whether the error implicitly rolled the transaction back.\n    pub transaction_rolled_back: bool,\n}\n\n");
    out.push_str("/// Every primary result code, in manifest order.\n");
    let _ = writeln!(
        out,
        "pub const PRIMARY_ROWS: [PrimaryRow; {}] = [",
        rows.len()
    );
    for row in rows {
        let _ = writeln!(
            out,
            "    PrimaryRow {{ code: PrimaryCode::{}, value: {}, c_name: {:?}, message: {:?}, connection_usable: {}, statement_resettable: {}, transaction_rolled_back: {} }},",
            row["name"].as_str(),
            row["code"].as_int(),
            row["c_name"].as_str(),
            row["message"].as_str(),
            row["connection_usable"].as_bool(),
            row["statement_resettable"].as_bool(),
            row["transaction_rolled_back"].as_bool(),
        );
    }
    out.push_str("];\n\n");
}

/// Writes the extended-code constants and their lookup table.
fn emit_extended_table(out: &mut String, rows: &[Table]) {
    out.push_str("/// One row of the generated extended-code table.\n");
    out.push_str("#[derive(Clone, Copy, Debug)]\npub struct ExtendedRow {\n");
    out.push_str("    /// The numeric value SQLite returns.\n    pub value: i32,\n");
    out.push_str(
        "    /// The primary code this extended code refines.\n    pub primary: PrimaryCode,\n",
    );
    out.push_str("    /// The C macro name.\n    pub c_name: &'static str,\n");
    out.push_str(
        "    /// The default English message for the code.\n    pub message: &'static str,\n",
    );
    out.push_str(
        "    /// Whether the connection may still be used.\n    pub connection_usable: bool,\n",
    );
    out.push_str("    /// Whether the statement may be reset and re-stepped.\n    pub statement_resettable: bool,\n");
    out.push_str("    /// Whether the error implicitly rolled the transaction back.\n    pub transaction_rolled_back: bool,\n}\n\n");
    out.push_str("/// Every extended result code, in manifest order.\n");
    let _ = writeln!(
        out,
        "pub const EXTENDED_ROWS: [ExtendedRow; {}] = [",
        rows.len()
    );
    for row in rows {
        let _ = writeln!(
            out,
            "    ExtendedRow {{ value: {}, primary: PrimaryCode::{}, c_name: {:?}, message: {:?}, connection_usable: {}, statement_resettable: {}, transaction_rolled_back: {} }},",
            row["code"].as_int(),
            row["primary"].as_str(),
            row["c_name"].as_str(),
            row["message"].as_str(),
            row["connection_usable"].as_bool(),
            row["statement_resettable"].as_bool(),
            row["transaction_rolled_back"].as_bool(),
        );
    }
    out.push_str("];\n\n");
    out.push_str("impl ExtendedCode {\n");
    for row in rows {
        let rust_name = screaming_snake(row["name"].as_str());
        let c_name = row["c_name"].as_str();
        let code = row["code"].as_int();
        let _ = writeln!(
            out,
            "    /// `{c_name}` ({code}).\n    pub const {rust_name}: ExtendedCode = ExtendedCode({code});"
        );
    }
    out.push_str("}\n");
}

/// Converts a manifest `CamelCase` row name to the `SCREAMING_SNAKE` constant
/// name the engine uses, so `IoErrShortRead` becomes `IO_ERR_SHORT_READ`.
fn screaming_snake(name: &str) -> String {
    let mut out = String::new();
    for (index, ch) in name.char_indices() {
        if ch.is_ascii_uppercase() && index > 0 {
            out.push('_');
        }
        out.push(ch.to_ascii_uppercase());
    }
    out
}

/// Writes the run-time limit table read from `compat/limits.toml`.
fn emit_limit_table(out: &mut String, rows: &[Table]) {
    out.push_str("/// One run-time limit, as `sqlite3_limit` exposes it.\n");
    out.push_str("#[derive(Clone, Copy, Debug)]\npub struct LimitRow {\n");
    out.push_str("    /// The Rust enum variant this row describes.\n    pub limit: Limit,\n");
    out.push_str("    /// The C macro name.\n    pub c_name: &'static str,\n");
    out.push_str("    /// The value a fresh connection starts with.\n    pub default: i64,\n");
    out.push_str(
        "    /// The ceiling a caller may not raise the limit past.\n    pub hard_max: i64,\n",
    );
    out.push_str(
        "    /// The floor a caller may not lower the limit past.\n    pub minimum: i64,\n",
    );
    out.push_str("    /// What the limit constrains.\n    pub description: &'static str,\n}\n\n");
    out.push_str("/// Every run-time limit, in manifest order.\n");
    out.push_str("#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]\n");
    out.push_str("#[non_exhaustive]\npub enum Limit {\n");
    for row in rows {
        let _ = writeln!(
            out,
            "    /// `{}`: {}\n    {},",
            row["c_name"].as_str(),
            row["description"].as_str(),
            row["name"].as_str()
        );
    }
    out.push_str("}\n\n");
    let _ = writeln!(
        out,
        "/// The generated limit table.\npub const LIMIT_ROWS: [LimitRow; {}] = [",
        rows.len()
    );
    for row in rows {
        let _ = writeln!(
            out,
            "    LimitRow {{ limit: Limit::{}, c_name: {:?}, default: {}, hard_max: {}, minimum: {}, description: {:?} }},",
            row["name"].as_str(),
            row["c_name"].as_str(),
            row["default"].as_int(),
            row["hard_max"].as_int(),
            row["minimum"].as_int(),
            row["description"].as_str(),
        );
    }
    out.push_str("];\n");
}

/// Reads one of the two manifests, from the workspace or from the vendored copy.
///
/// **Two locations, and the second one exists because of `cargo publish`.** The
/// manifests live in `compat/`, which is above this crate's directory, and a
/// published `.crate` tarball holds nothing above the crate directory - so a
/// build script that could only read `../../compat/errors.toml` made this crate
/// unpublishable, and everything above it in the graph with it - found the
/// first time this ran `cargo publish --dry-run`.
///
/// So the manifests are also vendored into `manifests/`, which travels in the
/// tarball, and the workspace copy wins whenever it is there.
///
/// **And the two are compared, every build, whenever both exist.** A vendored
/// copy that nothing checks is a copy that goes stale, and this one generates
/// the engine's error table - a stale one would silently give a released build
/// a different set of result codes from the one the compatibility harness
/// tests. So a difference is a build failure that names the file, which is the
/// same shape as every other invariant in this repository: checked, not
/// documented.
///
/// @param manifest_dir - this crate's directory, as cargo sets it
/// @param relative - the workspace path, relative to this crate
/// @param vendored - the file name under `manifests/`
fn read_manifest(manifest_dir: &Path, relative: &str, vendored: &str) -> String {
    let workspace = manifest_dir.join(relative);
    let carried = manifest_dir.join("manifests").join(vendored);
    println!("cargo:rerun-if-changed={}", workspace.display());
    println!("cargo:rerun-if-changed={}", carried.display());

    let from_workspace = std::fs::read_to_string(&workspace).ok();
    let from_package = std::fs::read_to_string(&carried).ok();

    match (from_workspace, from_package) {
        (Some(workspace_text), Some(package_text)) => {
            // Compared as lines rather than as bytes, because git's autocrlf
            // rewrites the line endings of one copy and not the other on this
            // platform, and a difference in line endings is not a difference in
            // the table. Everything else is.
            let same = workspace_text.lines().eq(package_text.lines());
            assert!(
                same,
                "{} and {} have drifted apart.\n  \
                 The workspace copy is the source of truth: copy it over the vendored one.\n  \
                 The vendored copy exists so this crate can be published; a stale one would \
                 give a released build a different error table from the one the harness tests.",
                workspace.display(),
                carried.display()
            );
            workspace_text
        }
        (Some(workspace_text), None) => workspace_text,
        (None, Some(package_text)) => package_text,
        (None, None) => panic!(
            "cannot read {} or {}",
            workspace.display(),
            carried.display()
        ),
    }
}

/// Reads the manifests, generates the error and limit tables, and tells cargo
/// to rerun when either manifest changes.
fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets this"));
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets this"));
    generate_limits(&manifest_dir, &out_dir);
    let text = read_manifest(&manifest_dir, "../../compat/errors.toml", "errors.toml");
    let (top, arrays) = parse_manifest(&text);
    let primary = arrays
        .get("primary")
        .expect("compat/errors.toml has [[primary]] rows");
    let extended = arrays
        .get("extended")
        .expect("compat/errors.toml has [[extended]] rows");

    let mut out = String::new();
    out.push_str("// @generated from compat/errors.toml by crates/inillucent-base/build.rs.\n");
    out.push_str("// Edit the manifest, not this file.\n\n");
    let _ = writeln!(
        out,
        "/// The SQLite release the generated table was taken from.\npub const ERROR_TABLE_REFERENCE: &str = {:?};\n",
        top["reference"].as_str()
    );
    let _ = writeln!(
        out,
        "/// The documentation page the generated table was taken from.\npub const ERROR_TABLE_SOURCE: &str = {:?};\n",
        top["source"].as_str()
    );
    emit_primary_enum(&mut out, primary);
    emit_primary_table(&mut out, primary);
    emit_extended_table(&mut out, extended);

    std::fs::write(out_dir.join("errors_generated.rs"), out)
        .expect("the build directory is writable");
}

/// Generates the limit table into `OUT_DIR`.
fn generate_limits(manifest_dir: &Path, out_dir: &Path) {
    let text = read_manifest(manifest_dir, "../../compat/limits.toml", "limits.toml");
    let (_, arrays) = parse_manifest(&text);
    let rows = arrays
        .get("limit")
        .expect("compat/limits.toml has [[limit]] rows");
    let mut out = String::new();
    out.push_str("// @generated from compat/limits.toml by crates/inillucent-base/build.rs.\n");
    out.push_str("// Edit the manifest, not this file.\n\n");
    emit_limit_table(&mut out, rows);
    std::fs::write(out_dir.join("limits_generated.rs"), out)
        .expect("the build directory is writable");
}
