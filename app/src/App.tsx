import { useCallback, useEffect, useState } from "react";
import type {
  AccessReport, AppUpdateCheck, ExtensionGroup, InstalledApp, JunkGroup,
  OrphanGroup, RemovalPlan, RemovalReport,
  RestoreOutcome, Section, Settings, StartupItem, UndoEntry, UpdateInfo,
} from "./types";
import { api, IS_TAURI } from "./api";
import { AppsView } from "./components/AppsView";
import { RemainingView } from "./components/RemainingView";
import { ReviewSheet } from "./components/ReviewSheet";
import { SettingsSheet } from "./components/SettingsSheet";
import { StartupView } from "./components/StartupView";
import { ExtensionsView } from "./components/ExtensionsView";
import { UpdatesView } from "./components/UpdatesView";
import { HistoryView } from "./components/HistoryView";
import { CleanupView } from "./components/CleanupView";
import { FullDiskAccessSheet } from "./components/FullDiskAccessSheet";
import { AppUpdateSheet } from "./components/AppUpdateSheet";
import {
  IconApps, IconBroom, IconHistory, IconPuzzle, IconRemaining, IconSettings,
  IconStartup, IconUpdates, IconWarn,
} from "./components/icons";

export default function App() {
  const [section, setSection] = useState<Section>("applications");
  const [apps, setApps] = useState<InstalledApp[]>([]);
  const [icons, setIcons] = useState<Record<string, string>>({});
  const [orphans, setOrphans] = useState<OrphanGroup[]>([]);
  const [scanningOrphans, setScanningOrphans] = useState(false);
  const [access, setAccess] = useState<AccessReport>({
    granted: true,
    checked: 0,
    blocked: [],
    applicable: false,
  });
  const [showAccess, setShowAccess] = useState(false);
  const [loading, setLoading] = useState(true);
  const [expert, setExpert] = useState(false);
  const [startup, setStartup] = useState<StartupItem[]>([]);
  const [startupLoading, setStartupLoading] = useState(false);
  const [startupBusyId, setStartupBusyId] = useState<string | null>(null);
  const [startupError, setStartupError] = useState<string | null>(null);
  const [extensions, setExtensions] = useState<ExtensionGroup[]>([]);
  const [extensionsLoading, setExtensionsLoading] = useState(false);
  const [junk, setJunk] = useState<JunkGroup[]>([]);
  const [junkLoading, setJunkLoading] = useState(false);
  const [updates, setUpdates] = useState<UpdateInfo[]>([]);
  const [updatesLoading, setUpdatesLoading] = useState(false);
  const [history, setHistory] = useState<UndoEntry[]>([]);
  const [historyLoading, setHistoryLoading] = useState(false);
  const [restoreBusyId, setRestoreBusyId] = useState<string | null>(null);
  // Keyed by the entry it belongs to. A floating "last result" is too easy for
  // an unrelated re-render to clear, which is exactly what happened when this
  // was a bare piece of state.
  const [restoreResult, setRestoreResult] =
    useState<{ id: string; outcomes: RestoreOutcome[] } | null>(null);
  // Bumped whenever something is actually removed. The views watch it to drop
  // a selection that no longer exists — otherwise the detail pane keeps showing
  // an app that has just been uninstalled, with an Uninstall button on it.
  const [removalNonce, setRemovalNonce] = useState(0);
  const [settings, setSettings] = useState<Settings>({
    removal_sound: false,
    full_disk_prompt_seen: true,
    auto_check_updates: false,
    last_update_check: 0,
  });
  const [showSettings, setShowSettings] = useState(false);
  const [appUpdate, setAppUpdate] = useState<AppUpdateCheck | null>(null);
  const [version, setVersion] = useState("0.1.0");

  const [plan, setPlan] = useState<RemovalPlan | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [report, setReport] = useState<RemovalReport | null>(null);

  useEffect(() => {
    // The permission explanation appears once, on first launch, and only when
    // something is actually being withheld.
    Promise.all([api.accessReport(), api.getSettings()]).then(([report, saved]) => {
      setAccess(report);
      setSettings(saved);
      if (report.applicable && !report.granted && !saved.full_disk_prompt_seen) {
        setShowAccess(true);
        const seen = { ...saved, full_disk_prompt_seen: true };
        setSettings(seen);
        api.setSettings(seen);
      }
    });
    // Honours the setting and the once-a-day throttle; silent when it fails.
    api.checkAppUpdate(false).then((result) => {
      if (result.update) setAppUpdate(result);
      setVersion(result.current);
    });
    api.listApps().then((a) => {
      setApps(a);
      setLoading(false);
      // Icons cost a subprocess each, so they are fetched after the list is on
      // screen — and only once it is, since the engine reads them from the
      // scan it has just cached.
      api.appIcons().then(setIcons);
    });
  }, []);

  useEffect(() => {
    if (section === "startup" && startup.length === 0) {
      setStartupLoading(true);
      api.startupItems().then((i) => {
        setStartup(i);
        setStartupLoading(false);
      });
    }
    if (section === "cleanup" && junk.length === 0) {
      setJunkLoading(true);
      api.junkGroups().then((g) => {
        setJunk(g);
        setJunkLoading(false);
      });
    }
    if (section === "extensions" && extensions.length === 0) {
      setExtensionsLoading(true);
      api.extensionGroups().then((g) => {
        setExtensions(g);
        setExtensionsLoading(false);
      });
    }
  }, [section, startup.length, extensions.length, junk.length]);

  const loadUpdates = useCallback(async (refresh: boolean) => {
    setUpdatesLoading(true);
    try {
      setUpdates(await api.checkUpdates(refresh));
    } finally {
      setUpdatesLoading(false);
    }
  }, []);

  const loadHistory = useCallback(async () => {
    setHistoryLoading(true);
    try {
      setHistory(await api.removalHistory());
    } finally {
      setHistoryLoading(false);
    }
  }, []);

  useEffect(() => {
    if (section === "updates" && updates.length === 0 && !updatesLoading) {
      loadUpdates(false);
    }
    if (section === "history") {
      loadHistory();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [section]);

  const restore = useCallback(
    async (id: string) => {
      setRestoreBusyId(id);
      setRestoreResult(null);
      try {
        setRestoreResult({ id, outcomes: await api.restoreRemoval(id) });
        await loadHistory();
        // What came back is installed again; the other screens must re-read.
        api.listApps(true).then(setApps);
      } catch (e) {
        setRestoreResult({
          id,
          outcomes: [
            {
              original: "",
              restored: false,
              error: e instanceof Error ? e.message : String(e),
            },
          ],
        });
      } finally {
        setRestoreBusyId(null);
      }
    },
    [loadHistory]
  );

  useEffect(() => {
    if (section === "remaining" && orphans.length === 0) {
      setScanningOrphans(true);
      api.orphanGroups().then((g) => {
        setOrphans(g);
        setScanningOrphans(false);
      });
    }
  }, [section, orphans.length]);

  const openPlanFor = useCallback(async (app: InstalledApp) => {
    setError(null);
    setBusy(true);
    try {
      const p = await api.planUninstall(app.id);
      if (p) {
        setPlan(p);
        return;
      }
      // No plan means the engine could not find that application any more —
      // nearly always because it has just been removed and this row is stale.
      // Doing nothing here is what made the Uninstall button look broken:
      // the row was still on screen, the click produced no sheet, no error,
      // nothing, and only quitting and reopening the app brought it back.
      setError(
        `${app.name || "That application"} is no longer installed — the list has been refreshed.`
      );
      setApps(await api.listApps(true));
    } finally {
      setBusy(false);
    }
  }, []);

  const openOrphanPlan = useCallback(async (names: string[]) => {
    setError(null);
    setBusy(true);
    try {
      setPlan(await api.planOrphans(names));
    } finally {
      setBusy(false);
    }
  }, []);

  const toggleStartup = useCallback(async (item: StartupItem, enabled: boolean) => {
    setStartupBusyId(item.id);
    setStartupError(null);
    // Show the new position immediately; put it back if the system refuses.
    setStartup((list) =>
      list.map((i) => (i.id === item.id ? { ...i, enabled } : i))
    );
    try {
      await api.setStartupEnabled(item.id, enabled);
      setStartup(await api.startupItems(true));
    } catch (e) {
      setStartup((list) =>
        list.map((i) => (i.id === item.id ? { ...i, enabled: !enabled } : i))
      );
      const message = e instanceof Error ? e.message : String(e);
      setStartupError(
        message === "cancelled"
          ? `${item.name} was left unchanged.`
          : `Could not change ${item.name}: ${message}`
      );
    } finally {
      setStartupBusyId(null);
    }
  }, []);

  const openCleanupPlan = useCallback(async (ids: string[]) => {
    setError(null);
    setBusy(true);
    try {
      setPlan(await api.planCleanup(ids));
    } finally {
      setBusy(false);
    }
  }, []);

  const openExtensionPlan = useCallback(async (ids: string[]) => {
    setError(null);
    setBusy(true);
    try {
      setPlan(await api.planExtensions(ids));
    } finally {
      setBusy(false);
    }
  }, []);

  const toggleItem = (path: string) =>
    setPlan((p) =>
      p
        ? {
            ...p,
            items: p.items.map((i) =>
              i.path === path ? { ...i, selected: !i.selected } : i
            ),
          }
        : p
    );

  const confirm = async (force = false) => {
    if (!plan) return;
    setBusy(true);
    setError(null);
    try {
      const r = await api.executePlan(plan, force);
      setReport(r);
      // Whatever moved is gone. Re-read the list, and empty every other
      // section's cache so it reloads when next opened rather than showing
      // something that is no longer there.
      if (r.outcomes.some((o) => o.removed)) {
        setRemovalNonce((n) => n + 1);
        setOrphans([]);
        setExtensions([]);
        setJunk([]);
        setStartup([]);
        setUpdates([]);
        api.listApps(true).then(setApps);
        if (section === "remaining") api.orphanGroups(true).then(setOrphans);
        if (section === "extensions") api.extensionGroups(true).then(setExtensions);
        if (section === "cleanup") api.junkGroups(true).then(setJunk);
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const closeSheet = () => {
    setPlan(null);
    setReport(null);
    setError(null);
  };

  const recheckAccess = useCallback(async () => {
    const report = await api.accessReport();
    setAccess(report);
    return report;
  }, []);

  const banner = access.applicable && !access.granted ? (
    <div className="banner">
      <IconWarn />
      <div>
        <strong>Full Disk Access is off</strong>
        {access.blocked.length > 0
          ? `${access.blocked.length} of ${access.checked} locations are hidden, so some leftovers will be missed and some sizes will read as unknown.`
          : "Parts of your Library are hidden, so some leftovers will be missed."}
        <button onClick={() => setShowAccess(true)}>What this means, and how to fix it</button>
      </div>
    </div>
  ) : !IS_TAURI ? (
    <div className="banner">
      <IconWarn />
      <div>
        <strong>Preview mode</strong>
        Showing captured data in a browser. Nothing can be removed here.
      </div>
    </div>
  ) : null;

  return (
    <div className="shell">
      <nav className="rail" data-tauri-drag-region>
        <RailItem
          label="Applications"
          active={section === "applications"}
          onClick={() => setSection("applications")}
          icon={<IconApps />}
        />
        <RailItem
          label={"Startup\nPrograms"}
          active={section === "startup"}
          onClick={() => setSection("startup")}
          icon={<IconStartup />}
        />
        <RailItem
          label="Extensions"
          active={section === "extensions"}
          onClick={() => setSection("extensions")}
          icon={<IconPuzzle />}
        />
        <RailItem
          label={"Remaining\nFiles"}
          active={section === "remaining"}
          onClick={() => setSection("remaining")}
          icon={<IconRemaining />}
        />
        <RailItem
          label="Cleanup"
          active={section === "cleanup"}
          onClick={() => setSection("cleanup")}
          icon={<IconBroom />}
        />
        <RailItem
          label="Updates"
          active={section === "updates"}
          onClick={() => setSection("updates")}
          icon={<IconUpdates />}
        />
        <RailItem
          label="History"
          active={section === "history"}
          onClick={() => setSection("history")}
          icon={<IconHistory />}
        />
        <div style={{ marginTop: "auto", width: "100%", display: "flex", justifyContent: "center" }}>
          <RailItem
            label="Settings"
            active={showSettings}
            onClick={() => setShowSettings(true)}
            icon={<IconSettings />}
          />
        </div>
        <div className="rail-brand" style={{ marginTop: 10 }}>
          BiswasHost
          <span>v{version}</span>
        </div>
      </nav>

      {section === "applications" && (
        <AppsView
          apps={apps}
          icons={icons}
          banner={banner}
          loading={loading}
          removalNonce={removalNonce}
          onUninstall={openPlanFor}
        />
      )}
      {section === "startup" && (
        <StartupView
          items={startup}
          banner={banner}
          loading={startupLoading}
          busyId={startupBusyId}
          error={startupError}
          onToggle={toggleStartup}
        />
      )}
      {section === "extensions" && (
        <ExtensionsView
          groups={extensions}
          banner={banner}
          loading={extensionsLoading}
          onRemove={openExtensionPlan}
        />
      )}
      {section === "cleanup" && (
        <CleanupView
          groups={junk}
          banner={banner}
          loading={junkLoading}
          onClean={openCleanupPlan}
        />
      )}
      {section === "updates" && (
        <UpdatesView
          updates={updates}
          banner={banner}
          loading={updatesLoading}
          checkedApps={Math.max(0, apps.length - updates.length)}
          onRecheck={() => loadUpdates(true)}
        />
      )}
      {section === "history" && (
        <HistoryView
          entries={history}
          loading={historyLoading}
          busyId={restoreBusyId}
          result={restoreResult}
          onRestore={restore}
        />
      )}
      {section === "remaining" && (
        <RemainingView
          groups={orphans}
          banner={banner}
          loading={scanningOrphans}
          onRemove={openOrphanPlan}
        />
      )}

      <label className="toggle" style={{ position: "fixed", top: 16, right: 22 }}>
        <input
          type="checkbox"
          checked={expert}
          onChange={(e) => setExpert(e.target.checked)}
        />
        Expert Mode
      </label>

      {appUpdate?.update && (
        <AppUpdateSheet check={appUpdate} onClose={() => setAppUpdate(null)} />
      )}

      {showAccess && (
        <FullDiskAccessSheet
          report={access}
          onRecheck={recheckAccess}
          onClose={() => setShowAccess(false)}
        />
      )}

      {showSettings && (
        <SettingsSheet
          settings={settings}
          accessGranted={access.granted}
          accessApplicable={access.applicable}
          version={version}
          onUpdateFound={(check) => {
            setShowSettings(false);
            setAppUpdate(check);
          }}
          onOpenAccess={() => {
            setShowSettings(false);
            setShowAccess(true);
          }}
          onChange={(next) => {
            setSettings(next);
            api.setSettings(next);
          }}
          onClose={() => setShowSettings(false)}
        />
      )}

      {plan && (
        <ReviewSheet
          plan={plan}
          busy={busy}
          error={error}
          report={report}
          expert={expert}
          onToggle={toggleItem}
          onCancel={closeSheet}
          onConfirm={() => confirm(false)}
          onForce={() => confirm(true)}
        />
      )}
    </div>
  );
}

function RailItem({
  label, icon, active, onClick,
}: { label: string; icon: React.ReactNode; active: boolean; onClick: () => void }) {
  return (
    <button className="rail-item" aria-current={active} onClick={onClick}>
      {icon}
      <span style={{ whiteSpace: "pre-line" }}>{label}</span>
    </button>
  );
}
