//! The filesystem authorization service: one answer to "may this process open
//! that file".
//!
//! Invariant: **when a root is installed, no path this process opens, creates,
//! deletes or stats resolves outside it.** `--root DIR` on `inillucent-mcp`
//! states that guarantee to whoever hands the server to an agent, and the
//! guarantee has to hold for every file the request causes to be opened, not
//! only for the one the caller named in the `db` argument.
//!
//! ## Why the check resolves the path instead of reading its text
//!
//! The check this replaces compared normalised path *text* against the root
//! with `starts_with`. Text is not where a file is. A Windows junction or a
//! Unix symbolic link placed below the root has a name that passes that test
//! and an inode that is somewhere else entirely, and a database was once
//! opened outside `--root` through exactly that gap. So every component is
//! resolved through the file system as it is appended, and a component that
//! turns out to be a link is replaced by what it points at before the next one
//! is considered. `..` pops the *resolved* path, which is what the kernel does
//! after it has followed a link, rather than the text the caller wrote.
//!
//! A path that does not exist yet cannot be a link, so resolution stops at the
//! deepest ancestor that does exist and the remaining components are appended
//! lexically. That is what makes the service usable for creation - the check
//! this replaces documented that a confinement which has already touched the
//! disk is not a confinement, and it is right about the *final* target, which
//! is why the VFS checks again at the moment the file is opened.
//!
//! ## Why there is a process-wide root as well as a per-call one
//!
//! Enumerating the call sites that open a file is how the hole got there: the
//! CLI checked its `db` argument, and `ATTACH DATABASE 'C:/elsewhere/x.rdb'`
//! never passed through that check because the path arrived inside a SQL
//! statement rather than as an argument. Enumeration also cannot cover the
//! next file operation somebody adds. [`OsVfs`](crate::os::OsVfs) is the only
//! thing in the workspace that opens a file, so the process-wide root is
//! consulted *there*, and a new file operation is confined the day it is
//! written rather than the day somebody remembers it.
//!
//! The per-call [`Root`] is still what the command surface uses, because a
//! refusal a person reads should name the path they typed and the directory
//! they confined to, and by the time a path reaches the VFS both of those are
//! gone.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

use inillucent_base::error::{ExtendedCode, PrimaryCode};

use crate::error::{VfsError, VfsResult};
use crate::path::DbPath;

/// A directory every path this service authorizes must resolve inside.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Root {
    resolved: PathBuf,
}

/// A path that resolved outside the root.
///
/// It carries all three of the things a readable refusal needs: what the caller
/// wrote, where that turned out to point, and what the root is. The last two
/// are what distinguishes "you typed a path outside the root" from "you typed a
/// path inside the root that is a link to somewhere else", and an operator
/// chasing the second one cannot do it from the first message alone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Refused {
    /// The path text the caller named.
    pub named: String,
    /// Where that path resolved to.
    pub resolved: PathBuf,
    /// The root it had to be inside.
    pub root: PathBuf,
}

impl Refused {
    /// Returns the sentence a command surface prints.
    pub fn message(&self) -> String {
        match self.resolved.as_os_str() == Path::new(&self.named).as_os_str() {
            true => format!(
                "\"{}\" is outside {}, which this server is confined to.",
                self.named,
                self.root.display()
            ),
            // The path text was inside and the file is not, which only happens
            // through a link. Saying so is the difference between an operator
            // fixing their command and an operator finding the junction.
            false => format!(
                "\"{}\" resolves to {}, which is outside {}, which this server is confined to.",
                self.named,
                self.resolved.display(),
                self.root.display()
            ),
        }
    }
}

impl std::fmt::Display for Refused {
    /// Writes the refusal sentence.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl std::error::Error for Refused {}

impl From<Refused> for VfsError {
    /// Converts a refusal into the VFS failure a file operation returns.
    ///
    /// `Perm` rather than `CantOpen`, because the file is very often there and
    /// readable and this process is the thing that may not have it. A caller
    /// that retried a `CantOpen` by creating the file would be told to try
    /// again forever.
    fn from(refused: Refused) -> VfsError {
        VfsError::new(
            ExtendedCode::from_primary(PrimaryCode::Perm),
            refused.message(),
        )
    }
}

impl Root {
    /// Builds a root from a directory, resolving it once.
    ///
    /// The root is resolved here so that a root which is itself reached through
    /// a link still compares against the same resolution every candidate gets.
    /// A root that does not exist is a refusal rather than a permitted
    /// everything: `--root /typo` naming nothing must not confine to nothing.
    ///
    /// @param directory - the directory to confine to
    pub fn at(directory: &Path) -> VfsResult<Root> {
        let resolved = resolve_through_links(directory);
        if !resolved.is_dir() {
            return Err(VfsError::new(
                ExtendedCode::from_primary(PrimaryCode::CantOpen),
                format!("--root {} is not a directory", directory.display()),
            ));
        }
        Ok(Root { resolved })
    }

    /// Builds a root from an already-resolved directory, without touching the
    /// file system.
    ///
    /// For tests and for a caller that has resolved the directory itself. A
    /// production caller wants [`Root::at`], which refuses a root that is not
    /// there.
    ///
    /// @param directory - the resolved directory to confine to
    pub fn resolved(directory: PathBuf) -> Root {
        Root {
            resolved: directory,
        }
    }

    /// Returns the resolved directory this root confines to.
    pub fn directory(&self) -> &Path {
        &self.resolved
    }

    /// Resolves a path a caller named and returns it when it is inside.
    ///
    /// A relative path is taken as relative to the root rather than to the
    /// working directory, which is what makes `--db app.rdb` mean the same
    /// thing to an agent as it does to the operator who started the server.
    ///
    /// @param named - the path the caller wrote
    pub fn admit(&self, named: &str) -> Result<PathBuf, Refused> {
        if named == ":memory:" || named.is_empty() {
            return Ok(PathBuf::from(named));
        }
        let joined = match Path::new(named).is_absolute() {
            true => PathBuf::from(named),
            false => self.resolved.join(named),
        };
        self.admit_path(named, &joined)
    }

    /// Resolves a path that is already absolute and returns it when it is
    /// inside.
    ///
    /// This is the form the VFS backstop uses: by then the path has been
    /// through `full_pathname` and joining a relative one against the root
    /// would be wrong, because a relative path arriving there is relative to
    /// the working directory the process was started in.
    ///
    /// @param named - the path text to report in a refusal
    /// @param path - the path to resolve
    pub fn admit_path(&self, named: &str, path: &Path) -> Result<PathBuf, Refused> {
        let resolved = resolve_through_links(path);
        match resolved.starts_with(&self.resolved) {
            true => Ok(resolved),
            false => Err(Refused {
                named: named.to_string(),
                resolved,
                root: self.resolved.clone(),
            }),
        }
    }

    /// Reports whether a path resolves inside this root.
    ///
    /// @param path - the path to resolve
    pub fn admits(&self, path: &Path) -> bool {
        resolve_through_links(path).starts_with(&self.resolved)
    }
}

/// The root this process is confined to, when one was installed.
static PROCESS_ROOT: OnceLock<Arc<Root>> = OnceLock::new();

/// Confines this process to a directory, for the rest of its life.
///
/// **Once, and it cannot be lifted.** A confinement that could be replaced
/// would be a confinement an attached database, an extension or a future
/// command could widen, and there is no caller that needs to: `--root` is read
/// from the command line before anything is opened. A second call with the same
/// directory is accepted so that a binary which sets it on two paths through
/// its startup is not a crash; a second call with a *different* directory is a
/// refusal.
///
/// @param directory - the directory to confine to
pub fn confine_process(directory: &Path) -> VfsResult<()> {
    let root = Root::at(directory)?;
    let installed = PROCESS_ROOT.get_or_init(|| Arc::new(root.clone()));
    match installed.as_ref() == &root {
        true => Ok(()),
        false => Err(VfsError::new(
            ExtendedCode::from_primary(PrimaryCode::Misuse),
            format!(
                "this process is already confined to {}",
                installed.directory().display()
            ),
        )),
    }
}

/// Returns the root this process is confined to, when there is one.
pub fn process_root() -> Option<Arc<Root>> {
    PROCESS_ROOT.get().map(Arc::clone)
}

/// Refuses a path that resolves outside the process root.
///
/// Returns `Ok` when no root is installed, which is every use of the library
/// that is not a confined server. This is the call [`OsVfs`](crate::os::OsVfs)
/// makes before it opens, deletes or stats anything.
///
/// @param path - the path about to be used
pub fn authorize(path: &DbPath) -> VfsResult<()> {
    if path.is_memory() || path.is_anonymous() {
        return Ok(());
    }
    let Some(root) = process_root() else {
        return Ok(());
    };
    let named = path.display();
    root.admit_path(&named, path.as_path())
        .map(|_| ())
        .map_err(VfsError::from)
}

/// Resolves a path through the file system, component by component.
///
/// Every component that exists is resolved with `canonicalize`, so a junction
/// or symbolic link is replaced by what it points at before the next component
/// is appended. `..` pops what has been resolved so far, which is what the
/// kernel does once it has followed a link - popping the *text* instead is the
/// bug that makes `root/link/../secret` look like `root/secret`. Components
/// past the deepest one that exists are appended as written, because a path
/// that is not there cannot be a link.
///
/// Never fails: a path that cannot be resolved at all resolves to itself, and
/// the caller's `starts_with` then refuses it unless it is genuinely inside.
///
/// @param path - the path to resolve
pub fn resolve_through_links(path: &Path) -> PathBuf {
    let absolute = match path.is_absolute() {
        true => path.to_path_buf(),
        false => match std::env::current_dir() {
            Ok(working) => working.join(path),
            Err(_) => path.to_path_buf(),
        },
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => resolved.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                if let Ok(canonical) = std::fs::canonicalize(&resolved) {
                    resolved = strip_verbatim_prefix(canonical);
                }
            }
        }
    }
    resolved
}

/// Removes the verbatim prefix Windows canonicalisation adds.
///
/// The same reason the one in `os` exists: the prefix is correct and it makes
/// two resolutions of the same directory compare unequal when one of them came
/// from a string a caller typed.
///
/// @param path - the canonicalised path
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy().to_string();
    match text.strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped),
        None => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path whose text climbs out of the root is refused.
    #[test]
    fn a_path_that_climbs_out_is_refused() {
        let directory = std::env::temp_dir().join("inillucent-confine-climb");
        std::fs::create_dir_all(directory.join("inner")).unwrap();
        let root = Root::at(&directory).unwrap();
        assert!(root.admit("inner/app.rdb").is_ok());
        assert!(root.admit("../outside.rdb").is_err());
        assert!(root.admit(":memory:").is_ok());
    }

    /// `..` inside the root that stays inside the root is admitted, because a
    /// refusal on the text alone would refuse a path that is genuinely inside.
    #[test]
    fn climbing_back_inside_is_admitted() {
        let directory = std::env::temp_dir().join("inillucent-confine-back");
        std::fs::create_dir_all(directory.join("inner")).unwrap();
        let root = Root::at(&directory).unwrap();
        assert!(root.admit("inner/../app.rdb").is_ok());
    }

    /// The resolver stops at the deepest ancestor that exists, so a file that
    /// is about to be created is still checked against where it would land.
    #[test]
    fn a_path_that_does_not_exist_yet_resolves_to_where_it_would_land() {
        let directory = std::env::temp_dir().join("inillucent-confine-new");
        std::fs::create_dir_all(&directory).unwrap();
        let root = Root::at(&directory).unwrap();
        let admitted = root.admit("not/there/yet.rdb").unwrap();
        assert!(admitted.ends_with("yet.rdb"));
        assert!(admitted.starts_with(root.directory()));
    }

    /// A root that is not a directory is refused, because confining to nothing
    /// would confine to everything.
    #[test]
    fn a_root_that_is_not_there_is_refused() {
        let missing = std::env::temp_dir().join("inillucent-confine-absent-root");
        let _ = std::fs::remove_dir_all(&missing);
        assert!(Root::at(&missing).is_err());
    }

    /// A refusal names the resolved target when it differs from the text,
    /// because the operator otherwise cannot tell a typo from a link.
    #[test]
    fn a_refusal_names_the_resolved_target() {
        let refused = Refused {
            named: "inner/app.rdb".to_string(),
            resolved: PathBuf::from("/elsewhere/app.rdb"),
            root: PathBuf::from("/root"),
        };
        assert!(refused.message().contains("resolves to"));
        assert!(refused.message().contains("elsewhere"));
    }

    /// With no process root installed, every path is authorized: the library
    /// is not a sandbox unless somebody asked for one.
    #[test]
    fn an_unconfined_process_authorizes_everything() {
        if process_root().is_none() {
            assert!(authorize(&DbPath::from("C:/anywhere/app.rdb")).is_ok());
        }
    }
}
