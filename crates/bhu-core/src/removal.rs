//! Planning and executing removals.
//!
//! Two separate steps on purpose: `build_plan` produces something the user
//! reads and edits, and `execute` acts only on what that plan says is selected.
//! There is no path from "user clicked Uninstall" straight to a filesystem
//! change without a plan in between.

use crate::model::*;
use crate::safety;
use crate::trash_bin;
use crate::undo;
use std::fs;

/// Build the dry run for uninstalling an app.
///
/// The app's own bundle is always the first item and is always pre-selected;
/// leftovers are selected according to their confidence.
pub fn build_plan(app: InstalledApp, leftovers: Vec<Leftover>) -> RemovalPlan {
    let mut items: Vec<RemovalItem> = Vec::with_capacity(leftovers.len() + 1);

    if let Some(path) = app.path.clone() {
        items.push(RemovalItem {
            name: app.name.clone(),
            size_bytes: app.size_bytes,
            size_unknown: false,
            is_directory: path.is_dir(),
            requires_admin: safety::requires_admin(&path),
            path,
            kind: LeftoverKind::Other,
            confidence: Confidence::High,
            reason: "the application itself".into(),
            selected: true,
            registry_key: None,
        });
    }

    items.extend(leftovers.into_iter().map(RemovalItem::from));

    // Highest confidence first, then largest — so the things the user most
    // needs to scrutinise are not buried at the bottom of a long list.
    items.sort_by(|a, b| {
        b.confidence
            .cmp(&a.confidence)
            .then(b.size_bytes.cmp(&a.size_bytes))
    });

    RemovalPlan {
        delegated_command: None,
        app: Some(app),
        items,
    }
}

/// Build a plan for orphaned leftovers — the "Remaining Files" case, where
/// there is no app to uninstall because it is already gone.
pub fn build_orphan_plan(leftovers: Vec<Leftover>) -> RemovalPlan {
    let mut items: Vec<RemovalItem> = leftovers.into_iter().map(RemovalItem::from).collect();
    items.sort_by(|a, b| {
        b.confidence
            .cmp(&a.confidence)
            .then(b.size_bytes.cmp(&a.size_bytes))
    });
    RemovalPlan {
        app: None,
        items,
        delegated_command: None,
    }
}

/// Where exported registry keys are kept, alongside the removal journal.
pub fn registry_backup_dir() -> Option<std::path::PathBuf> {
    let dir = crate::undo::data_dir()?.join("registry-backups");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// A filename that cannot escape the backup directory.
fn backup_name(key: &str) -> String {
    let safe: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}-{stamp}.reg", safe.trim_matches('-'))
}

/// Export a registry key, then delete it.
///
/// The export is not optional and not best-effort: a registry key cannot go to
/// the Recycle Bin, so the `.reg` file *is* the undo. If the export fails, or
/// produces nothing, the key is left exactly where it is.
#[cfg(target_os = "windows")]
fn remove_registry_key(key: &str) -> Result<std::path::PathBuf, String> {
    crate::safety::check_registry_removable(key).map_err(|e| e.to_string())?;

    let dir = registry_backup_dir().ok_or("no application data directory for the backup")?;
    let backup = dir.join(backup_name(key));

    let exported = crate::proc::command("reg")
        .args(["export", key])
        .arg(&backup)
        .arg("/y")
        .output()
        .map_err(|e| format!("could not run reg export: {e}"))?;
    if !exported.status.success() {
        let err = String::from_utf8_lossy(&exported.stderr).trim().to_string();
        return Err(if err.is_empty() {
            "could not export the key, so it was left alone".into()
        } else {
            format!("could not export the key, so it was left alone: {err}")
        });
    }
    let usable = std::fs::metadata(&backup)
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    if !usable {
        return Err("the exported backup was empty, so the key was left alone".into());
    }

    let deleted = crate::proc::command("reg")
        .args(["delete", key, "/f"])
        .output()
        .map_err(|e| format!("could not run reg delete: {e}"))?;
    if !deleted.status.success() {
        let err = String::from_utf8_lossy(&deleted.stderr).trim().to_string();
        return Err(if err.is_empty() {
            "the key could not be removed".into()
        } else {
            err
        });
    }
    Ok(backup)
}

#[cfg(not(target_os = "windows"))]
fn remove_registry_key(_key: &str) -> Result<std::path::PathBuf, String> {
    Err("registry keys only exist on Windows".into())
}

/// Execute the selected items of a plan.
///
/// Each item is re-validated against the safety rules and re-checked on disk
/// immediately before it is touched. A plan can be minutes old by the time the
/// user clicks Remove; the filesystem may have changed under it.
///
/// `opts` carries the trash behaviour — see [`crate::trash_bin`] for why the
/// sound setting also decides whether Finder can put items back.
///
/// Items the user cannot write to are set aside and moved in one privileged
/// batch at the end, so the password is asked for once rather than per file —
/// and only after everything that needs no password has already succeeded.
pub fn execute(plan: &RemovalPlan, opts: RemovalOptions) -> RemovalReport {
    let mut outcomes = Vec::new();
    let mut bytes_freed = 0u64;
    let mut deferred: Vec<(std::path::PathBuf, u64)> = Vec::new();

    // Where the platform owns the uninstall, its own uninstaller runs first and
    // the sweep only happens if it succeeded. Removing files underneath a
    // failed or cancelled uninstaller would leave the system describing
    // software that is half gone — worse than not having started.
    if let Some(command) = &plan.delegated_command {
        let verify = plan.app.as_ref().and_then(|a| a.path.clone());
        if let Err(e) = run_delegated(command, verify.as_deref()) {
            if opts.force {
                // Explicitly asked for: the uninstaller is broken or refuses to
                // run, and clearing the files is all that is left.
            } else {
                return RemovalReport {
                    delegated_failed: Some(e.clone()),
                    delegated_ran: true,
                    outcomes: plan
                        .selected_items()
                        .map(|i| RemovalOutcome {
                            path: i.path.clone(),
                            removed: false,
                            already_gone: false,
                            trashed_to: None,
                            error: Some(e.clone()),
                        })
                        .collect(),
                    bytes_freed: 0,
                    undo_id: None,
                };
            }
        }
    }

    // Key and the file it will be exported to, decided once: the name carries a
    // timestamp, so working it out twice would record a backup path that does
    // not exist.
    let mut deferred_keys: Vec<(String, std::path::PathBuf)> = Vec::new();

    for item in plan.selected_items() {
        // Registry keys are not files: they are exported and deleted, never
        // trashed, and machine-wide ones need the same elevation.
        if let Some(key) = item.registry_key.clone() {
            if item.requires_admin {
                match registry_backup_dir() {
                    Some(dir) => deferred_keys.push((key.clone(), dir.join(backup_name(&key)))),
                    None => outcomes.push(RemovalOutcome {
                        path: item.path.clone(),
                        removed: false,
                        already_gone: false,
                        trashed_to: None,
                        error: Some("no directory to export the key into".into()),
                    }),
                }
            } else {
                match remove_registry_key(&key) {
                    Ok(backup) => outcomes.push(RemovalOutcome {
                        path: item.path.clone(),
                        removed: true,
                        already_gone: false,
                        trashed_to: Some(backup),
                        error: None,
                    }),
                    Err(e) => outcomes.push(RemovalOutcome {
                        path: item.path.clone(),
                        removed: false,
                        already_gone: false,
                        trashed_to: None,
                        error: Some(e),
                    }),
                }
            }
            continue;
        }

        // The safety check runs here, at the point of no return.
        if let Err(e) = safety::check_removable(&item.path) {
            outcomes.push(RemovalOutcome {
                path: item.path.clone(),
                removed: false,
                already_gone: false,
                trashed_to: None,
                error: Some(e.to_string()),
            });
            continue;
        }
        // `symlink_metadata` so a symlink is examined as itself rather than
        // followed to whatever it points at.
        let Ok(meta) = fs::symlink_metadata(&item.path) else {
            // Already gone — almost always because the application's own
            // uninstaller has just removed it. Reporting that as a failure told
            // the user nothing had been removed when in fact the uninstall had
            // worked perfectly.
            outcomes.push(RemovalOutcome {
                path: item.path.clone(),
                removed: true,
                already_gone: true,
                trashed_to: None,
                error: None,
            });
            continue;
        };
        let size = if meta.is_symlink() {
            0
        } else {
            crate::fsutil::size_on_disk(&item.path)
        };

        if crate::elevate::needs_elevation(&item.path) {
            deferred.push((item.path.clone(), size));
            continue;
        }

        match trash_bin::move_to_trash(&item.path, opts.sound) {
            Ok(trashed_to) => {
                bytes_freed += size;
                outcomes.push(RemovalOutcome {
                    path: item.path.clone(),
                    removed: true,
                    already_gone: false,
                    trashed_to,
                    error: None,
                });
            }
            Err(e) if e.is_permission_denied() => {
                // ⚠️ `needs_elevation` is a PREDICTION — it asks whether the
                // parent directory is writable — and it can be wrong.
                // `/Applications` is group-writable by admin, so an app that a
                // `.pkg` installed as root is predicted to need no password,
                // and macOS then refuses the move anyway. (Since macOS 14 one
                // app may not modify another's bundle without App Management,
                // which Full Disk Access does not include.)
                //
                // A permission refusal is not a final answer, so it is not
                // reported as one. The item joins the elevated batch and is
                // retried as root, under the single password prompt the rest
                // of that batch already asks for. Before this, the user was
                // left looking at an application the uninstaller could see and
                // could not remove.
                deferred.push((item.path.clone(), size));
            }
            Err(e) => outcomes.push(RemovalOutcome {
                path: item.path.clone(),
                removed: false,
                already_gone: false,
                trashed_to: None,
                error: Some(e.to_string()),
            }),
        }
    }

    if !deferred_keys.is_empty() {
        match crate::elevate::registry_remove_elevated(&deferred_keys) {
            Ok(()) => {
                for (key, backup) in &deferred_keys {
                    outcomes.push(RemovalOutcome {
                        path: std::path::PathBuf::from(key),
                        removed: true,
                        already_gone: false,
                        trashed_to: Some(backup.clone()),
                        error: None,
                    });
                }
            }
            Err(e) => {
                for (key, _) in &deferred_keys {
                    outcomes.push(RemovalOutcome {
                        path: std::path::PathBuf::from(key),
                        removed: false,
                        already_gone: false,
                        trashed_to: None,
                        error: Some(e.clone()),
                    });
                }
            }
        }
    }

    if !deferred.is_empty() {
        let paths: Vec<std::path::PathBuf> = deferred.iter().map(|(p, _)| p.clone()).collect();
        match crate::elevate::trash_elevated(&paths, &timestamp()) {
            Ok(dest) => {
                for (path, size) in deferred {
                    bytes_freed += size;
                    let landed = path.file_name().map(|n| dest.join(n));
                    outcomes.push(RemovalOutcome {
                        path,
                        removed: true,
                        already_gone: false,
                        trashed_to: landed,
                        error: None,
                    });
                }
            }
            Err(e) => {
                // WARNING: the exit status is a hint, not the outcome. The
                // elevated script moves the items and then tries to hand them
                // back to the user with `chown`; that second step can fail on
                // its own (`~/.Trash` is TCC-protected even from root) long
                // after the move has succeeded. Believing the status reported
                // "Nothing was removed" for two applications that had in fact
                // just been moved into the Trash.
                //
                // So each path is checked on disk. Gone is gone, whatever the
                // script returned — and it is journalled as removed, because a
                // removal the app denies having done is one the user cannot
                // undo from History either.
                let message = elevated_failure_message(&e, &deferred);
                for (path, size) in deferred {
                    let moved = fs::symlink_metadata(&path).is_err();
                    if moved {
                        bytes_freed += size;
                    }
                    outcomes.push(RemovalOutcome {
                        path,
                        removed: moved,
                        already_gone: false,
                        // Where it landed is only known when the script
                        // reported success, so this stays empty — History
                        // shows it as removed but not restorable from here.
                        trashed_to: None,
                        error: if moved { None } else { Some(message.clone()) },
                    });
                }
            }
        }
    }

    let mut report = RemovalReport {
        outcomes,
        bytes_freed,
        undo_id: None,
        delegated_failed: None,
        delegated_ran: plan.delegated_command.is_some(),
    };
    report.undo_id = undo::record(&report, plan.app.as_ref().map(|a| a.name.clone()));
    report
}

/// What to tell the user when even the elevated move failed.
///
/// If root could not move an application bundle, the remaining explanation on
/// macOS is **App Management**: since macOS 14 one application may not modify
/// or delete another's bundle without it, and ⚠️ Full Disk Access does **not**
/// include it — they are separate permissions and granting the first does
/// nothing for the second. Naming it is the difference between a dead end and
/// something the user can actually fix.
///
/// Cancelling the password prompt is a choice and keeps its own wording.
fn elevated_failure_message(error: &str, items: &[(std::path::PathBuf, u64)]) -> String {
    let _ = items;
    #[cfg(target_os = "macos")]
    if error != "cancelled"
        && items
            .iter()
            .any(|(p, _)| p.extension().and_then(|e| e.to_str()) == Some("app"))
    {
        return format!(
            "{error}\n\nmacOS may be withholding App Management, which is the permission \
             that lets one app remove another — Full Disk Access does not include it. \
             Add BHUninstaller in System Settings → Privacy & Security → App Management, \
             then try again."
        );
    }
    error.to_string()
}

/// Run the platform's own uninstaller and wait for it.
///
/// NOT YET EXERCISED — there is no delegated uninstall on macOS, so this path
/// only runs on Windows and Linux and has not been tried on either.
#[cfg(not(target_os = "windows"))]
fn run_delegated(command: &str, verify: Option<&std::path::Path>) -> Result<(), String> {
    let _ = verify;
    let status = crate::proc::command("/bin/sh")
        .args(["-c", command])
        .status()
        .map_err(|e| format!("could not start the uninstaller: {e}"))?;
    if status.success() {
        return Ok(());
    }
    Err(format!(
        "the application's own uninstaller did not finish (exit {}).",
        status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown".into())
    ))
}

/// Run a Windows uninstaller, elevated, and wait for it.
///
/// NOT YET RUN ON WINDOWS.
///
/// Two things this has to get right. An uninstaller in `Program Files` needs
/// administrator rights: started without them it either fails outright, or
/// re-launches itself elevated and returns immediately — so the caller sees an
/// exit code that has nothing to do with the real outcome. That is what the
/// first Windows attempt hit: "exit 1" from an uninstaller that had not
/// actually run. Starting it elevated avoids both.
///
/// And the command comes out of the registry carrying its own quoting, so it is
/// written to a `.cmd` file and that file is run. Re-quoting someone else's
/// command line is how this goes wrong.
#[cfg(target_os = "windows")]
fn run_delegated(command: &str, verify: Option<&std::path::Path>) -> Result<(), String> {
    use std::io::Write;

    // The `.cmd` written here is run elevated, so nothing may be able to put
    // its own file at that path first. See `proc::private_temp_dir`.
    let work = crate::proc::private_temp_dir("BHUninstaller-uninstall")?;
    let script = work.join("run.cmd");
    {
        let mut f = std::fs::File::create(&script).map_err(|e| e.to_string())?;
        // Written verbatim; nothing re-quotes it.
        write!(f, "@echo off\r\n{command}\r\nexit /b %ERRORLEVEL%\r\n")
            .map_err(|e| e.to_string())?;
    }

    let quoted = script.display().to_string().replace('\'', "''");
    let inner = format!(
        "$p = Start-Process -FilePath cmd.exe -ArgumentList '/C','{quoted}' -Verb RunAs -Wait -PassThru; exit $p.ExitCode"
    );
    let out = crate::proc::command("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &inner,
        ])
        .output()
        .map_err(|e| format!("could not start the uninstaller: {e}"))?;

    // The exit code is not trustworthy on its own. Inno Setup's `unins000.exe`
    // — which is what most Windows apps ship — returns 1 both when it hands off
    // to an elevated copy of itself *and* when the user answers "No" to its own
    // confirmation. Reading it as failure told the user the uninstaller was
    // broken when it had simply asked a question.
    //
    // So the code is treated as a hint and the outcome is checked: if the
    // install directory is gone, the uninstall worked, whatever it returned.
    let code = out.status.code();
    if matches!(code, Some(0) | Some(3010)) {
        return Ok(());
    }
    if let Some(path) = verify {
        // Give the uninstaller a moment to finish deleting before looking.
        std::thread::sleep(std::time::Duration::from_millis(1500));
        if !path.exists() {
            return Ok(());
        }
    }
    match code {
        Some(1223) | Some(1602) | Some(1) => {
            Err("the uninstall was cancelled, or the application is still installed.".into())
        }
        Some(code) => Err(format!(
            "the application's own uninstaller did not finish (exit {code})."
        )),
        None => Err("the application's own uninstaller was interrupted.".into()),
    }
}

/// A human-readable stamp for the quarantine folder name.
fn timestamp() -> String {
    #[cfg(unix)]
    {
        if let Ok(out) = crate::proc::command("/bin/date")
            .arg("+%Y-%m-%d %H.%M.%S")
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "removal".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> InstalledApp {
        InstalledApp {
            id: "com.example.app".into(),
            name: "Example".into(),
            path: Some("/Applications/Example.app".into()),
            bundle_id: Some("com.example.app".into()),
            executable: None,
            version: None,
            publisher: None,
            size_bytes: 100,
            source: AppSource::Applications,
            icon_png_base64: None,
            created_at: None,
            modified_at: None,
            last_opened_at: None,
            notarized: None,
            is_running: false,
            is_system: false,
            scope: None,
        }
    }

    fn leftover(name: &str, conf: Confidence, shared: Vec<String>) -> Leftover {
        Leftover {
            path: dirs::home_dir().unwrap().join("Library/Caches").join(name),
            name: name.into(),
            size_bytes: 10,
            size_unknown: false,
            is_directory: true,
            kind: LeftoverKind::Caches,
            confidence: conf,
            reason: "test".into(),
            requires_admin: false,
            shared_with: shared,
            registry_key: None,
        }
    }

    #[test]
    fn only_high_confidence_leftovers_are_preselected() {
        let plan = build_plan(
            app(),
            vec![
                leftover("high", Confidence::High, vec![]),
                leftover("medium", Confidence::Medium, vec![]),
                leftover("low", Confidence::Low, vec![]),
            ],
        );
        // The app bundle plus the one high-confidence leftover.
        assert_eq!(plan.selected_count(), 2);
        let selected: Vec<_> = plan.selected_items().map(|i| i.name.as_str()).collect();
        assert!(selected.contains(&"high"));
        assert!(!selected.contains(&"medium"));
        assert!(!selected.contains(&"low"));
    }

    #[test]
    fn shared_vendor_directories_are_never_preselected() {
        // Even at High confidence: if another installed app also lives in this
        // directory, removing it would take that app's data with it.
        let plan = build_plan(
            app(),
            vec![leftover(
                "Google",
                Confidence::High,
                vec!["Google Chrome".into()],
            )],
        );
        let selected: Vec<_> = plan.selected_items().map(|i| i.name.as_str()).collect();
        assert!(!selected.contains(&"Google"));
    }

    #[test]
    fn execute_refuses_a_plan_that_points_at_a_protected_path() {
        let mut plan = build_orphan_plan(vec![]);
        plan.items.push(RemovalItem {
            path: dirs::home_dir().unwrap().join("Documents"),
            name: "Documents".into(),
            size_bytes: 0,
            size_unknown: false,
            is_directory: true,
            kind: LeftoverKind::Other,
            confidence: Confidence::High,
            reason: "malicious or buggy plan".into(),
            requires_admin: false,
            selected: true,
            registry_key: None,
        });
        let report = execute(&plan, RemovalOptions::default());
        assert_eq!(report.removed_count(), 0);
        assert_eq!(report.failed().count(), 1);
    }
}
