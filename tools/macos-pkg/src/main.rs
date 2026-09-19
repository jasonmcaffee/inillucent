//! `macos-pkg` — writes a macOS flat package on a machine that is not a Mac.
//!
//! `pkgbuild` and `productbuild` are the two programs in a macOS release with no
//! equivalent outside macOS. Everything else the release needs has one:
//! `cargo-zigbuild` links the Mach-O, and `rcodesign` replaces `lipo`,
//! `codesign`, `notarytool` and `stapler`. This program is the remaining gap,
//! and closing it is what lets the whole release be cut on the Windows machine
//! (task-1995).
//!
//! It writes the product archive directly rather than writing a component
//! package and then wrapping it, because the wrapper step only unpacks the
//! component into a directory inside the same kind of archive.
//!
//! ```text
//! macos-pkg --root dist/pkgroot --identifier com.blackrainbowlabs.inillucent \
//!   --version 0.1.4 --install-location / --executable-dir usr/local/bin \
//!   --executable-dir usr/local/lib --distribution packaging/macos/Distribution.xml \
//!   --resource packaging/macos/welcome.txt --resource LICENSE \
//!   --output dist/inillucent-0.1.4-unsigned.pkg
//! ```
//!
//! The result is unsigned. `rcodesign sign` signs it with the Developer ID
//! Installer certificate, which it can do because a flat package is a XAR
//! archive and `rcodesign` signs those.

mod bom;
mod cpio;
mod pkg;
mod vars;
mod xar;

use {
    anyhow::{Context, Result},
    clap::Parser,
    std::path::PathBuf,
};

/// The command line, which mirrors the `pkgbuild` and `productbuild` arguments
/// the macOS release script passes, so that the two scripts can be read against
/// each other.
#[derive(Parser)]
#[command(about = "Builds a macOS flat package (.pkg) on any platform")]
struct Args {
    /// The directory whose contents become the payload, laid out as it installs.
    #[arg(long)]
    root: PathBuf,

    /// The reverse-DNS identifier the install receipt is recorded under.
    #[arg(long)]
    identifier: String,

    /// The release version.
    #[arg(long)]
    version: String,

    /// Where the payload is laid down.
    #[arg(long, default_value = "/")]
    install_location: String,

    /// A directory inside the payload root whose files install as mode 0755.
    #[arg(long = "executable-dir")]
    executable_dirs: Vec<String>,

    /// The `Distribution` document. `@VERSION@` in it is replaced.
    #[arg(long)]
    distribution: PathBuf,

    /// A file the distribution names, such as a welcome or licence text.
    #[arg(long = "resource")]
    resources: Vec<PathBuf>,

    /// The component directory name the distribution's pkg-ref refers to.
    #[arg(long)]
    component_name: Option<String>,

    /// Where to write the package.
    #[arg(long)]
    output: PathBuf,
}

/// Reads the distribution template and substitutes the version into it.
///
/// @param path - the template
/// @param version - the release version
fn distribution_for(path: &PathBuf, version: &str) -> Result<String> {
    let template =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(template.replace("@VERSION@", version))
}

/// Builds the package described by the command line.
fn main() -> Result<()> {
    let args = Args::parse();

    // **A shell can rewrite this argument, and the result installs somewhere nobody meant.**
    // `--install-location /` typed in Git Bash arrives as `C:/Program Files/Git/`, because MSYS
    // rewrites a lone slash into its own root. The package still builds, still signs and is still
    // notarised - Apple accepted one - and it would lay the payload down under a path that does not
    // exist on a Mac. An install location that is not absolute in the POSIX sense is always this.
    if !args.install_location.starts_with('/') {
        anyhow::bail!(
            "--install-location is {:?}, which is not an absolute macOS path. A shell has rewritten \
             it: Git Bash turns a lone `/` into its own installation directory. Pass it from \
             PowerShell, or as `//` which MSYS leaves alone.",
            args.install_location
        );
    }
    let component_name = args
        .component_name
        .clone()
        .unwrap_or_else(|| format!("inillucent-component-{}.pkg", args.version));

    let spec = pkg::PackageSpec {
        identifier: args.identifier.clone(),
        version: args.version.clone(),
        install_location: args.install_location.clone(),
        executable_dirs: args.executable_dirs.clone(),
        timestamp: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    };

    let distribution = distribution_for(&args.distribution, &args.version)?;
    let entries = pkg::assemble(
        &spec,
        &args.root,
        &distribution,
        &args.resources,
        &component_name,
    )?;
    let archive = xar::build(&entries, &spec.timestamp)?;

    std::fs::write(&args.output, &archive)
        .with_context(|| format!("writing {}", args.output.display()))?;
    println!(
        "{} ({} bytes, {} entries, component {component_name})",
        args.output.display(),
        archive.len(),
        entries.len()
    );
    Ok(())
}
