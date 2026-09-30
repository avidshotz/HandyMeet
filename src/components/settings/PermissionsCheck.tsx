import React, { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { type } from "@tauri-apps/plugin-os";
import {
  checkAccessibilityPermission,
  requestAccessibilityPermission,
  checkMicrophonePermission,
  requestMicrophonePermission,
  checkInputMonitoringPermission,
  requestInputMonitoringPermission,
} from "tauri-plugin-macos-permissions-api";
import { SettingsGroup } from "../ui/SettingsGroup";

/**
 * What macOS currently thinks this app is allowed to do.
 *
 * Deliberately always on screen, including when everything is granted. The older panel hid itself
 * the moment it believed permission was there, which left no way to tell "granted" apart from
 * "the check is wrong" — and the check being wrong is the failure that keeps recurring here.
 *
 * macOS never tells a running app that a permission was granted, so a panel that only looks once
 * at startup reports "not granted" forever after you switch the toggle on. This one re-checks
 * whenever the window comes back to the front, which is exactly the moment you return from
 * System Settings, and offers a manual re-check for every other moment.
 */

type Status = "checking" | "granted" | "denied";

interface Permission {
  key: string;
  labelKey: string;
  whyKey: string;
  check: () => Promise<boolean>;
  request: () => Promise<unknown>;
}

const PERMISSIONS: Permission[] = [
  {
    key: "accessibility",
    labelKey: "permissions.accessibilityLabel",
    whyKey: "permissions.accessibilityWhy",
    check: checkAccessibilityPermission,
    request: requestAccessibilityPermission,
  },
  {
    key: "microphone",
    labelKey: "permissions.microphoneLabel",
    whyKey: "permissions.microphoneWhy",
    check: checkMicrophonePermission,
    request: requestMicrophonePermission,
  },
  {
    key: "inputMonitoring",
    labelKey: "permissions.inputMonitoringLabel",
    whyKey: "permissions.inputMonitoringWhy",
    check: checkInputMonitoringPermission,
    request: requestInputMonitoringPermission,
  },
];

export const PermissionsCheck: React.FC = () => {
  const { t } = useTranslation();
  const isMacOS = type() === "macos";
  const [statuses, setStatuses] = useState<Record<string, Status>>(() =>
    Object.fromEntries(PERMISSIONS.map((p) => [p.key, "checking" as Status])),
  );
  const [busy, setBusy] = useState(false);
  const [checkedAt, setCheckedAt] = useState<Date | null>(null);

  const recheck = useCallback(async () => {
    setBusy(true);
    const results = await Promise.all(
      PERMISSIONS.map(async (permission) => {
        try {
          return [permission.key, (await permission.check()) ? "granted" : "denied"] as const;
        } catch {
          // A check that throws is not a grant.
          return [permission.key, "denied"] as const;
        }
      }),
    );
    setStatuses(Object.fromEntries(results));
    setCheckedAt(new Date());
    setBusy(false);
  }, []);

  useEffect(() => {
    if (!isMacOS) return;
    void recheck();

    // Coming back from System Settings is the one moment the answer is likely to have changed.
    const onFocus = () => void recheck();
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [isMacOS, recheck]);

  if (!isMacOS) return null;

  const outstanding = PERMISSIONS.filter((p) => statuses[p.key] === "denied").length;

  return (
    <SettingsGroup title={t("permissions.title")}>
      <div className="flex flex-col gap-3 p-3">
        <div className="flex items-center justify-between gap-3">
          <p className="text-xs text-text/70">
            {busy
              ? t("permissions.checking")
              : outstanding === 0
                ? t("permissions.allGranted")
                : t("permissions.outstanding", { count: outstanding })}
          </p>
          <button
            type="button"
            onClick={() => void recheck()}
            disabled={busy}
            className="shrink-0 px-2 py-1 text-sm font-semibold bg-mid-gray/10 border border-mid-gray/80 hover:bg-logo-primary/10 hover:border-logo-primary rounded cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
          >
            {busy ? t("permissions.checking") : t("permissions.checkButton")}
          </button>
        </div>

        {PERMISSIONS.map((permission) => {
          const status = statuses[permission.key];
          return (
            <div
              key={permission.key}
              className="flex items-center justify-between gap-3 rounded-md border border-mid-gray/30 px-3 py-2"
            >
              <div className="min-w-0">
                <div className="flex items-center gap-2">
                  <span
                    aria-hidden
                    className={`inline-block w-2 h-2 rounded-full shrink-0 ${
                      status === "granted"
                        ? "bg-emerald-500"
                        : status === "denied"
                          ? "bg-red-500"
                          : "bg-mid-gray/60"
                    }`}
                  />
                  <span className="text-sm font-medium">{t(permission.labelKey)}</span>
                  <span className="text-xs text-text/60">
                    {status === "granted"
                      ? t("permissions.granted")
                      : status === "denied"
                        ? t("permissions.denied")
                        : t("permissions.checking")}
                  </span>
                </div>
                <p className="text-xs text-text/60 mt-0.5">{t(permission.whyKey)}</p>
              </div>
              {status === "denied" && (
                <button
                  type="button"
                  onClick={async () => {
                    try {
                      await permission.request();
                    } catch (error) {
                      console.error(`Could not open settings for ${permission.key}`, error);
                    }
                    // The grant lands while we are in the background; the focus listener catches it.
                  }}
                  className="shrink-0 px-2 py-1 text-sm font-semibold bg-mid-gray/10 border border-mid-gray/80 hover:bg-logo-primary/10 hover:border-logo-primary rounded cursor-pointer"
                >
                  {t("permissions.openSettings")}
                </button>
              )}
            </div>
          );
        })}

        <p className="text-xs text-text/50 leading-relaxed">
          {checkedAt
            ? t("permissions.lastChecked", { time: checkedAt.toLocaleTimeString() }) + " "
            : ""}
          {t("permissions.staleEntryHint")}
        </p>
      </div>
    </SettingsGroup>
  );
};
