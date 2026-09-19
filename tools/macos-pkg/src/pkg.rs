//! Assembling a macOS product archive out of its four documented parts.
//!
//! `productbuild` writes an archive holding a `Distribution` document, a
//! `Resources` directory, and one directory per component package containing
//! `PackageInfo`, `Payload` and `Bom`. Nothing in that description is secret and
//! none of it needs Apple's tools; what it needs is the three writers beside
//! this file and `apple-bom` for the fourth.
//!
//! **The mode of every file is decided here, not read off the disk.** NTFS has
//! no executable bit, so a payload built on Windows from what the file system
//! says would install four programs that cannot be run. The caller names the
//! directories whose contents are executable and everything else is 0644, which
//! is the same rule `packaging/stage-layout.ps1` applies when it writes the
//! `.tar.gz` — one rule, stated twice, rather than a guess made twice.

use {
    crate::{
        bom,
        cpio::{self, CpioEntry},
        xar::XarEntry,
    },
    anyhow::{bail, Context, Result},
    std::{
        collections::BTreeSet,
        path::{Path, PathBuf},
    },
};

/// Everything the assembly needs that is not a file on disk.
pub struct PackageSpec {
    /// The reverse-DNS identifier the receipt is recorded under.
    pub identifier: String,
    /// The release version.
    pub version: String,
    /// Where the payload is laid down, almost always `/`.
    pub install_location: String,
    /// Directories inside the payload root whose files are mode 0755.
    pub executable_dirs: Vec<String>,
    /// The ISO 8601 timestamp written into the archive.
    pub timestamp: String,
}

/// One file found under the payload root.
struct PayloadFile {
    /// The path relative to the payload root, with `/` separators.
    relative: String,
    /// The bytes.
    data: Vec<u8>,
    /// The permission bits this file installs with.
    mode: u32,
}

/// Reads every file under the payload root and decides its mode.
///
/// @param root - the directory whose contents become the payload
/// @param executable_dirs - path prefixes whose files are executable
fn read_payload(root: &Path, executable_dirs: &[String]) -> Result<Vec<PayloadFile>> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(root).sort_by_file_name() {
        let entry = entry.with_context(|| format!("walking {}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .with_context(|| "a walked path left its own root")?
            .to_string_lossy()
            .replace('\\', "/");
        let executable = executable_dirs
            .iter()
            .any(|dir| relative.starts_with(&format!("{dir}/")));
        files.push(PayloadFile {
            data: std::fs::read(entry.path())
                .with_context(|| format!("reading {}", entry.path().display()))?,
            mode: if executable { 0o755 } else { 0o644 },
            relative,
        });
    }
    if files.is_empty() {
        bail!(
            "{} holds no files, so the package would install nothing",
            root.display()
        );
    }
    Ok(files)
}

/// Lists every directory that has to exist for a set of files, parents first.
///
/// @param files - the payload's files
fn directories_for(files: &[PayloadFile]) -> Vec<String> {
    let mut directories = BTreeSet::new();
    for file in files {
        let mut parts: Vec<&str> = file.relative.split('/').collect();
        parts.pop();
        let mut prefix = String::new();
        for part in parts {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            directories.insert(prefix.clone());
        }
    }
    // A BTreeSet of paths orders a parent before its children because the
    // parent is a prefix of the child and prefixes sort first.
    directories.into_iter().collect()
}

/// Builds the gzipped cpio archive that is the package's `Payload`.
///
/// @param files - the payload's files
/// @param directories - the directories they live in
fn build_payload(files: &[PayloadFile], directories: &[String]) -> Result<Vec<u8>> {
    let mut entries = vec![CpioEntry {
        path: String::new(),
        is_directory: true,
        mode: 0o755,
        data: Vec::new(),
    }];
    for directory in directories {
        entries.push(CpioEntry {
            path: directory.clone(),
            is_directory: true,
            mode: 0o755,
            data: Vec::new(),
        });
    }
    for file in files {
        entries.push(CpioEntry {
            path: file.relative.clone(),
            is_directory: false,
            mode: file.mode,
            data: file.data.clone(),
        });
    }
    Ok(cpio::gzip(&cpio::build(&entries, 0))?)
}

/// Builds the `Bom`, the list the Installer records the receipt from.
///
/// Only files are passed on. The BOM writer derives each directory from the
/// paths it is given and emits it before its children, so naming them here as
/// well would put two records in the BOM for one directory.
///
/// @param files - the payload's files
fn build_bom(files: &[PayloadFile]) -> Result<Vec<u8>> {
    let entries = files
        .iter()
        .map(|file| bom::BomFile {
            path: file.relative.clone(),
            mode: file.mode,
            size: file.data.len() as u32,
            crc32: crc32fast::hash(&file.data),
        })
        .collect::<Vec<_>>();
    bom::build(&entries, 0o755, 0)
}

/// Builds the `PackageInfo` document for the component.
///
/// `installKBytes` and `numberOfFiles` are what the Installer shows as the
/// download size and what it uses to check there is room, so they are computed
/// rather than left at zero.
///
/// @param spec - the identifiers and the install location
/// @param files - the payload's files
/// @param directories - the directories they live in
fn build_package_info(spec: &PackageSpec, files: &[PayloadFile], directories: &[String]) -> String {
    let bytes: usize = files.iter().map(|file| file.data.len()).sum();
    let kilobytes = bytes.div_ceil(1024);
    let count = files.len() + directories.len() + 1;

    // `install-location` is omitted when the payload installs at the root, which is what
    // `node-v26.9.0.pkg` does and what `pkgbuild` writes. The empty elements below are Apple's too:
    // none is needed for a payload with no bundles in it, and all of them appear in every
    // PackageInfo Apple's tooling produces.
    let location = if spec.install_location == "/" {
        String::new()
    } else {
        format!(" install-location=\"{}\"", spec.install_location)
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <pkg-info overwrite-permissions=\"true\" relocatable=\"false\" identifier=\"{}\" \
         postinstall-action=\"none\" version=\"{}\" format-version=\"2\"{} auth=\"root\">\n\
         \x20   <payload numberOfFiles=\"{}\" installKBytes=\"{}\"/>\n\
         \x20   <bundle-version/>\n\
         \x20   <upgrade-bundle/>\n\
         \x20   <update-bundle/>\n\
         \x20   <atomic-update-bundle/>\n\
         \x20   <strict-identifier/>\n\
         \x20   <relocate/>\n\
         </pkg-info>\n",
        spec.identifier, spec.version, location, count, kilobytes
    )
}

/// Assembles the whole product archive.
///
/// The returned entries are in the order a reader walks them: the distribution
/// document, the resources it names, then the component directory.
///
/// @param spec - the identifiers, the install location and the mode rule
/// @param root - the directory whose contents become the payload
/// @param distribution - the `Distribution` document, already substituted
/// @param resources - files the distribution refers to by name
/// @param component_name - the directory name the distribution's pkg-ref names
pub fn assemble(
    spec: &PackageSpec,
    root: &Path,
    distribution: &str,
    resources: &[PathBuf],
    component_name: &str,
) -> Result<Vec<XarEntry>> {
    let files = read_payload(root, &spec.executable_dirs)?;
    let directories = directories_for(&files);

    let payload = build_payload(&files, &directories)?;
    let bom = build_bom(&files)?;
    let package_info = build_package_info(spec, &files, &directories);

    let mut entries = vec![XarEntry {
        path: "Distribution".to_string(),
        is_directory: false,
        mode: 0o644,
        data: distribution.as_bytes().to_vec(),
        compress: true,
    }];

    if !resources.is_empty() {
        entries.push(XarEntry {
            path: "Resources".to_string(),
            is_directory: true,
            mode: 0o755,
            data: Vec::new(),
            compress: false,
        });
        for resource in resources {
            let name = resource
                .file_name()
                .with_context(|| format!("{} has no file name", resource.display()))?
                .to_string_lossy()
                .to_string();
            entries.push(XarEntry {
                path: format!("Resources/{name}"),
                is_directory: false,
                mode: 0o644,
                data: std::fs::read(resource)
                    .with_context(|| format!("reading {}", resource.display()))?,
                compress: true,
            });
        }
    }

    entries.push(XarEntry {
        path: component_name.to_string(),
        is_directory: true,
        mode: 0o755,
        data: Vec::new(),
        compress: false,
    });
    // The payload is already a gzip stream, so it goes into the heap as it is. Apple stores it the
    // same way - `application/octet-stream`, with the archived and extracted lengths equal - and
    // deflating it a second time saves nothing.
    for (name, data, compress) in [
        ("PackageInfo", package_info.into_bytes(), true),
        ("Bom", bom, true),
        ("Payload", payload, false),
    ] {
        entries.push(XarEntry {
            path: format!("{component_name}/{name}"),
            is_directory: false,
            mode: 0o644,
            data,
            compress,
        });
    }

    Ok(entries)
}
