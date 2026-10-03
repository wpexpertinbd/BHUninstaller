//! Moving things to the trash.
//!
//! The engine never deletes. Every removal is a move to the OS trash / recycle
//! bin, so that a wrong call by us, or a change of mind by the user, is always
//! recoverable.
//!
//! ## The two routes on macOS, and why the choice is not only cosmetic
//!
//! macOS offers two ways to trash a file:
//!
//! - **Through Finder.** Finder plays its trash sound for every item, and needs
//!   Automation permission. It records the information behind Finder's
//!   "Put Back".
//! - **Through `NSFileManager`.** Silent, faster, no extra permission — but it
//!   records nothing, so "Put Back" is greyed out. Verified on macOS 27: a file
//!   trashed this way has no `kMDItemWhereFroms` and no put-back attribute.
//!
//! So the sound setting is really a choice between Finder's restore affordance
//! and a quiet, permission-free removal. The engine covers the gap by recording
//! where each item came from *and* where it landed in the trash, so it can put
//! things back itself either way — see [`crate::undo`].

use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum TrashError {
    #[error("{0}")]
    Failed(String),
}

impl TrashError {
    /// Whether the operating system refused this on permissions.
    ///
    /// ⚠️ Worth knowing *why* this is a string match. The `trash` crate
    /// flattens every platform failure into prose, so there is no error code
    /// left to test by the time it reaches here — the wording below is what
    /// each platform actually produces:
    ///
    /// - macOS, `NSFileManager`: *"… couldn't be moved to the trash because
    ///   you don't have permission to access it."*
    /// - POSIX: `Permission denied` (EACCES), `Operation not permitted` (EPERM)
    /// - Windows: `Access is denied`
    ///
    /// Matching too widely here would send a genuinely impossible removal to
    /// the elevated path and ask for a password that cannot help, so the
    /// phrases are specific rather than a bare search for "permission".
    pub fn is_permission_denied(&self) -> bool {
        let TrashError::Failed(message) = self;
        let m = message.to_lowercase();
        m.contains("permission denied")
            || m.contains("operation not permitted")
            || m.contains("access is denied")
            || m.contains("don't have permission")
            || m.contains("do not have permission")
    }
}

/// Move a path to the trash, returning where it ended up when that can be
/// determined. Never follows symlinks: a symlink is trashed as the link itself.
pub fn move_to_trash(path: &Path, sound: bool) -> Result<Option<PathBuf>, TrashError> {
    #[cfg(target_os = "macos")]
    {
        use trash::macos::TrashContextExtMacos;
        let mut ctx = trash::TrashContext::default();
        ctx.set_delete_method(if sound {
            trash::macos::DeleteMethod::Finder
        } else {
            trash::macos::DeleteMethod::NsFileManager
        });
        ctx.delete(path)
            .map_err(|e| TrashError::Failed(e.to_string()))?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = sound;
        trash::delete(path).map_err(|e| TrashError::Failed(e.to_string()))?;
    }
    Ok(locate_in_trash(path))
}

/// Work out where an item landed, so it can be put back later.
///
/// The trash APIs do not report the destination, and the name may have been
/// changed to avoid a collision with something already in there. We look for
/// the exact name first, then for the newest entry whose name is that one with
/// a suffix, which is the pattern macOS uses.
#[cfg(target_os = "macos")]
fn locate_in_trash(original: &Path) -> Option<PathBuf> {
    let trash = dirs::home_dir()?.join(".Trash");
    let name = original.file_name()?.to_string_lossy().to_string();

    let exact = trash.join(&name);
    if exact.exists() {
        return Some(exact);
    }

    let stem = Path::new(&name)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| name.clone());

    std::fs::read_dir(&trash)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(&stem))
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((modified, e.path()))
        })
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

#[cfg(not(target_os = "macos"))]
fn locate_in_trash(_original: &Path) -> Option<PathBuf> {
    // The freedesktop and Windows implementations record their own restore
    // information, so the destination is not needed to put an item back.
    None
}

/// Whether this platform can restore from the trash through the file manager
/// the user already knows.
///
/// On macOS this depends on how the item was trashed — see the module docs —
/// so the UI must not promise "Put Back" unconditionally.
pub const fn can_restore_programmatically() -> bool {
    cfg!(any(target_os = "windows", target_os = "linux"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The macOS wording is verbatim from a real failure: an application
    /// installed by a `.pkg` (owned by root) in a `/Applications` that the
    /// admin user *can* write to, which is exactly the case the elevated
    /// retry exists for.
    #[test]
    fn a_permission_refusal_is_recognised_whatever_the_platform_calls_it() {
        for message in [
            "Error during a `trash` operation: Unknown { description: \"While deleting \
             '\\\"/Applications/Display Portal.app\\\"', `trashItemAtURL` failed: \"Display Portal\" \
             couldn't be moved to the trash because you don't have permission to access it.\" }",
            "Permission denied (os error 13)",
            "Operation not permitted (os error 1)",
            "Access is denied. (os error 5)",
        ] {
            assert!(
                TrashError::Failed(message.to_string()).is_permission_denied(),
                "should have been read as a permission refusal: {message}"
            );
        }
    }

    /// Matching too widely would send an impossible removal to the elevated
    /// path and ask for a password that cannot help.
    #[test]
    fn other_failures_are_not_mistaken_for_permission() {
        for message in [
            "No such file or directory (os error 2)",
            "No space left on device (os error 28)",
            "Read-only file system (os error 30)",
            "could not determine the trash directory",
        ] {
            assert!(
                !TrashError::Failed(message.to_string()).is_permission_denied(),
                "should NOT have been read as a permission refusal: {message}"
            );
        }
    }
}
