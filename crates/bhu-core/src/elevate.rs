//! Removing things the user cannot write to.
//!
//! Launch daemons, privileged helpers and installer receipts live outside the
//! user's home directory, so a normal process cannot move them. These need an
//! administrator password.
//!
//! Rather than deleting them outright as root, they are moved — still as root —
//! into a timestamped folder inside the user's Trash, which is then handed back
//! to the user. Nothing is destroyed, the user can look inside and put anything
//! back by hand, and emptying the Trash disposes of it normally.
//!
//! Finder's "Put Back" does not work for these, because the move is a plain
//! `mv` rather than a Finder trash operation. The undo journal records where
//! everything came from, and the UI must say so rather than implying otherwise.

use std::path::{Path, PathBuf};

/// Ask for an administrator password once and move every path into a quarantine
/// folder in the user's Trash. Returns the folder it created.
#[cfg(target_os = "macos")]
pub fn trash_elevated(paths: &[PathBuf], stamp: &str) -> Result<PathBuf, String> {
    use std::time::{SystemTime, UNIX_EPOCH};

    if paths.is_empty() {
        return Err("nothing to remove".into());
    }
    let home = dirs::home_dir().ok_or("no home directory")?;
    let trash = home.join(".Trash");
    std::fs::create_dir_all(&trash)
        .map_err(|e| format!("could not prepare {}: {e}", trash.display()))?;

    // The destination has to be a directory this call has just created, not one
    // that was already sitting there. A predictable name that something else
    // could pre-place — as a symlink, say — would have `mv`, running as root,
    // write through it to wherever that link pointed.
    //
    // `create_dir` fails outright if the path exists at all, symlink included,
    // so a collision is refused rather than followed; the suffix makes
    // arranging one impractical in the first place.
    let dest = trash.join(format!(
        "BHUninstaller {stamp} ({}-{})",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir(&dest).map_err(|e| format!("could not prepare {}: {e}", dest.display()))?;

    // Confirm what we just made is a real directory and not something
    // substituted between the two calls.
    let meta = std::fs::symlink_metadata(&dest)
        .map_err(|e| format!("could not verify {}: {e}", dest.display()))?;
    if meta.is_symlink() || !meta.is_dir() {
        return Err("the quarantine folder was not what we created — refusing".into());
    }

    // Every path is passed as a separate argument and quoted by AppleScript's
    // `quoted form of`. Nothing is ever interpolated into the shell string —
    // a single quote in a filename would otherwise be arbitrary code as root.
    //
    // WARNING: the `chown` is deliberately NOT part of the `&&` chain's
    // success. It exists only so the quarantined items end up belonging to the
    // user rather than to root, which is a convenience — the move is the
    // operation. And it fails: `~/.Trash` is TCC-protected, and the root shell
    // this spawns is still subject to that, so `chown` returns "Operation not
    // permitted" even as root.
    //
    // While it was chained with `&&`, that failure made the whole script exit
    // non-zero **after the move had already happened** — so the app moved two
    // applications into the Trash and told the user "Nothing was removed".
    // Reporting a completed removal as a failure is worse than the failure it
    // was describing, because the user then goes looking for apps that are
    // gone. `|| true` keeps a `chown` failure from speaking for the `mv`.
    let script = r#"on run argv
    set destPath to item 1 of argv
    set uidgid to item 2 of argv
    set cmd to "/bin/mv -f"
    repeat with i from 3 to count of argv
        set cmd to cmd & " " & quoted form of (item i of argv)
    end repeat
    set cmd to cmd & " " & quoted form of destPath
    set cmd to cmd & " && { /usr/sbin/chown -R " & uidgid & " " & quoted form of destPath & " || true; }"
    do shell script cmd with administrator privileges
end run"#;

    // SAFETY: getuid/getgid cannot fail and take no arguments.
    let uidgid = unsafe { format!("{}:{}", libc::getuid(), libc::getgid()) };

    let mut cmd = crate::proc::command("/usr/bin/osascript");
    cmd.arg("-e").arg(script).arg("--").arg(&dest).arg(uidgid);
    for p in paths {
        cmd.arg(p);
    }

    let out = cmd
        .output()
        .map_err(|e| format!("could not run osascript: {e}"))?;
    if out.status.success() {
        return Ok(dest);
    }
    let err = String::from_utf8_lossy(&out.stderr);
    // Cancelling the password prompt is a choice, not a failure to report as one.
    if err.contains("-128") || err.to_lowercase().contains("user canceled") {
        let _ = std::fs::remove_dir(&dest);
        return Err("cancelled".into());
    }
    let _ = std::fs::remove_dir(&dest);
    Err(err.trim().to_string())
}

/// Ask for elevation once, and move every path into a quarantine folder.
///
/// NOT YET RUN ON WINDOWS. The shape mirrors the macOS path: one prompt, a
/// move rather than a delete, and a folder the user can open and pick through.
///
/// Windows has no supported way to put a file in the Recycle Bin as another
/// user, so the quarantine lives under `%LOCALAPPDATA%\BHUninstaller\Quarantine`
/// instead — still the user's own space, still recoverable, and the removal
/// history records where everything came from.
///
/// The paths are written to a file and the elevated script reads them from
/// there. Building a command line out of them would mean quoting user-supplied
/// paths for PowerShell, which is exactly the mistake the macOS path avoids by
/// passing argv.
#[cfg(target_os = "windows")]
pub fn trash_elevated(paths: &[PathBuf], stamp: &str) -> Result<PathBuf, String> {
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    if paths.is_empty() {
        return Err("nothing to remove".into());
    }
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or("no local application data directory")?;

    let root = local.join("BHUninstaller").join("Quarantine");
    std::fs::create_dir_all(&root)
        .map_err(|e| format!("could not prepare {}: {e}", root.display()))?;

    let dest = root.join(format!(
        "{stamp} ({}-{})",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    // Refuses an existing path, symlink included — see the macOS notes.
    std::fs::create_dir(&dest).map_err(|e| format!("could not prepare {}: {e}", dest.display()))?;

    // The script below is run as Administrator, so where it is written matters
    // as much as what it says. See `proc::private_temp_dir`.
    let work = crate::proc::private_temp_dir("BHUninstaller-elevate")?;

    let list = work.join("paths.txt");
    {
        let mut f = std::fs::File::create(&list).map_err(|e| e.to_string())?;
        for p in paths {
            writeln!(f, "{}", p.display()).map_err(|e| e.to_string())?;
        }
    }

    let script = work.join("move.ps1");
    std::fs::write(
        &script,
        r#"param([string]$PathsFile, [string]$Dest)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path $Dest | Out-Null
foreach ($p in Get-Content -LiteralPath $PathsFile -Encoding UTF8) {
    if ([string]::IsNullOrWhiteSpace($p)) { continue }
    if (-not (Test-Path -LiteralPath $p)) { continue }
    $name   = Split-Path -Leaf $p
    $target = Join-Path $Dest $name
    $i = 1
    while (Test-Path -LiteralPath $target) {
        $target = Join-Path $Dest ("{0} ({1})" -f $name, $i)
        $i++
    }
    Move-Item -LiteralPath $p -Destination $target -Force
}
"#,
    )
    .map_err(|e| e.to_string())?;

    // Single-quoted PowerShell strings; a quote inside a path is escaped by
    // doubling it, which is the only escape that form has.
    let q = |p: &std::path::Path| p.display().to_string().replace('\'', "''");
    let inner = format!(
        "Start-Process powershell -Verb RunAs -Wait -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File','{}','-PathsFile','{}','-Dest','{}')",
        q(&script),
        q(&list),
        q(&dest)
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
        .map_err(|e| format!("could not start PowerShell: {e}"))?;

    if out.status.success() {
        return Ok(dest);
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir(&dest);
    // Declining the UAC prompt is a choice, not a fault to report as one.
    if err.contains("canceled")
        || err.contains("cancelled")
        || err.contains("The operation was canceled by the user")
    {
        return Err("cancelled".into());
    }
    Err(if err.trim().is_empty() {
        "the elevated removal did not complete".into()
    } else {
        err.trim().to_string()
    })
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
pub fn trash_elevated(_paths: &[PathBuf], _stamp: &str) -> Result<PathBuf, String> {
    // Linux: pkexec, or leave it to the package manager, which handles its own
    // privilege escalation as part of the uninstall.
    Err("elevated removal is not implemented on this platform yet".into())
}

/// True when this path needs the elevated route.
pub fn needs_elevation(path: &Path) -> bool {
    crate::safety::requires_admin(path)
}

/// Export and delete machine-wide registry keys, with one prompt for the lot.
///
/// Same shape as the elevated file move: the keys are written to a list rather
/// than interpolated into a command line, and the elevated script reads that
/// list. A key that fails to export is skipped rather than deleted — the export
/// is its only undo.
#[cfg(target_os = "windows")]
pub fn registry_remove_elevated(keys: &[(String, PathBuf)]) -> Result<(), String> {
    use std::io::Write;

    if keys.is_empty() {
        return Err("nothing to remove".into());
    }
    // Refuse the whole batch if any single key would not pass the rules. An
    // elevated operation is the wrong place to be lenient.
    for (key, _) in keys {
        crate::safety::check_registry_removable(key).map_err(|e| e.to_string())?;
    }

    // Run as Administrator — see `proc::private_temp_dir`.
    let work = crate::proc::private_temp_dir("BHUninstaller-registry")?;

    // key<TAB>backup-file, one per line, UTF-8.
    let list = work.join("keys.txt");
    {
        let mut f = std::fs::File::create(&list).map_err(|e| e.to_string())?;
        for (key, backup) in keys {
            writeln!(f, "{}\t{}", key, backup.display()).map_err(|e| e.to_string())?;
        }
    }

    let done = work.join("done.txt");
    let script = work.join("registry.ps1");
    std::fs::write(
        &script,
        r#"$ErrorActionPreference='SilentlyContinue'
$list = $args[0]
$done = $args[1]
$ok = 0
foreach ($line in Get-Content -LiteralPath $list -Encoding UTF8) {
  if ($line -notmatch "`t") { continue }
  $parts = $line -split "`t", 2
  $key = $parts[0]
  $backup = $parts[1]
  # Export first. Without a usable backup the key is left exactly as it is.
  & reg.exe export "$key" "$backup" /y | Out-Null
  if (-not (Test-Path -LiteralPath $backup)) { continue }
  if ((Get-Item -LiteralPath $backup).Length -le 0) { continue }
  & reg.exe delete "$key" /f | Out-Null
  if ($LASTEXITCODE -eq 0) { $ok++ }
}
Set-Content -LiteralPath $done -Value $ok -Encoding UTF8
"#,
    )
    .map_err(|e| e.to_string())?;

    // Single-quoted PowerShell strings; a quote inside a path is escaped by
    // doubling it, which is the only escape that form has. The temporary
    // directory comes from the environment, so it is not assumed to be tame.
    let q = |p: &std::path::Path| p.display().to_string().replace('\'', "''");
    let status = crate::proc::command("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
        ])
        .arg(format!(
            "$p = Start-Process powershell -Verb RunAs -Wait -PassThru -ArgumentList \
             '-NoProfile','-ExecutionPolicy','Bypass','-File','{}','{}','{}'; exit $p.ExitCode",
            q(&script),
            q(&list),
            q(&done)
        ))
        .output()
        .map_err(|e| format!("could not start the elevated helper: {e}"))?;

    let removed = std::fs::read_to_string(&done)
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let _ = std::fs::remove_dir_all(&work);

    if removed > 0 {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&status.stderr);
    if err.contains("canceled") || err.contains("cancelled") || !status.status.success() {
        return Err("cancelled".into());
    }
    Err("no keys could be removed".into())
}

#[cfg(not(target_os = "windows"))]
pub fn registry_remove_elevated(_keys: &[(String, PathBuf)]) -> Result<(), String> {
    Err("registry keys only exist on Windows".into())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    /// The shell composition the elevated script builds, exercised directly.
    ///
    /// WARNING: this is here because the `&&` version of it moved two of the
    /// user's applications into the Trash and then reported "Nothing was
    /// removed", because the `chown` that follows the move failed on its own.
    /// The three cases below are the whole contract: a failing `chown` must
    /// not speak for a successful move, and a failing move must still fail.
    fn run(mv_src: &str, dest: &str, chown_clause: &str) -> i32 {
        let cmd = format!("/bin/mv -f '{mv_src}' '{dest}' {chown_clause}",);
        crate::proc::command("/bin/sh")
            .arg("-c")
            .arg(cmd)
            .status()
            .expect("sh")
            .code()
            .unwrap_or(-1)
    }

    /// `nosuchuser:nosuchgroup` makes `chown` fail the way the real one does
    /// in `~/.Trash`, without needing root or a protected location.
    const FAILING_CHOWN: &str = "/usr/sbin/chown -R nosuchuser:nosuchgroup";

    #[test]
    fn a_failing_chown_does_not_fail_a_successful_move() {
        let dir = crate::proc::private_temp_dir("bhu-elev").unwrap();
        let dest = dir.join("dest");
        std::fs::create_dir(&dest).unwrap();
        let src = dir.join("payload.txt");
        std::fs::write(&src, b"hello").unwrap();

        let code = run(
            &src.to_string_lossy(),
            &dest.to_string_lossy(),
            &format!("&& {{ {FAILING_CHOWN} '{}' || true; }}", dest.display()),
        );

        assert_eq!(code, 0, "a chown failure must not be reported as failure");
        assert!(
            dest.join("payload.txt").exists(),
            "the move itself must still have happened"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_move_is_still_reported() {
        let dir = crate::proc::private_temp_dir("bhu-elev").unwrap();
        let dest = dir.join("dest");
        std::fs::create_dir(&dest).unwrap();

        let code = run(
            &dir.join("does-not-exist").to_string_lossy(),
            &dest.to_string_lossy(),
            &format!("&& {{ {FAILING_CHOWN} '{}' || true; }}", dest.display()),
        );

        assert_ne!(code, 0, "tolerating chown must not mask a failed move");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
