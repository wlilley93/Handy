import React, { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { commands, type ServerStatus } from "@/bindings";
import { SettingContainer } from "../ui/SettingContainer";
import { ToggleSwitch } from "../ui/ToggleSwitch";

interface LocalApiServerProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

/**
 * Controls for the local transcription API.
 *
 * The server's real state is read back from the backend rather than mirrored
 * from the settings: a port collision leaves the toggle on and the socket
 * down, and the user needs to see which of the two is true.
 */
export const LocalApiServer: React.FC<LocalApiServerProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const [status, setStatus] = useState<ServerStatus | null>(null);
  const [port, setPort] = useState("");
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    const next = await commands.getServerStatus();
    setStatus(next);
    setPort(String(next.port));
    return next;
  }, []);

  useEffect(() => {
    refresh().catch((e) => setError(String(e)));
  }, [refresh]);

  const apply = useCallback(
    async (
      patch: Partial<{ enabled: boolean; port: number; allowLan: boolean }>,
    ) => {
      if (!status) return;
      setBusy(true);
      setError(null);
      try {
        // An empty token box means "no token" only when the user has cleared a
        // token they typed. Leaving it blank while one is already stored keeps
        // the stored one — the box never displays the secret, so a blank box is
        // not evidence that the user wants it gone.
        const nextToken = token.trim().length > 0 ? token.trim() : null;
        const result = await commands.setServerSettings(
          patch.enabled ?? status.enabled,
          patch.port ?? Number(port) ?? status.port,
          nextToken,
          patch.allowLan ?? status.allow_lan,
        );
        if (result.status === "error") {
          setError(result.error);
        } else {
          setToken("");
        }
      } catch (e) {
        setError(String(e));
      } finally {
        setBusy(false);
        await refresh().catch(() => undefined);
      }
    },
    [status, token, port, refresh],
  );

  if (!status) return null;

  const url = `http://${status.address ?? `127.0.0.1:${status.port}`}`;

  return (
    <>
      <ToggleSwitch
        checked={status.enabled}
        onChange={(enabled) => apply({ enabled })}
        isUpdating={busy}
        label={t("settings.advanced.localApi.label")}
        description={t("settings.advanced.localApi.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />

      {status.enabled && (
        <>
          <SettingContainer
            title={t("settings.advanced.localApi.port.label")}
            description={t("settings.advanced.localApi.port.description")}
            descriptionMode={descriptionMode}
            grouped={grouped}
          >
            <input
              type="number"
              min={1}
              max={65535}
              value={port}
              disabled={busy}
              onChange={(e) => setPort(e.target.value)}
              onBlur={() => {
                const parsed = Number(port);
                if (parsed >= 1 && parsed <= 65535 && parsed !== status.port) {
                  apply({ port: parsed });
                }
              }}
              className="w-24 px-2 py-1 rounded border border-mid-gray/30 bg-background text-right"
            />
          </SettingContainer>

          <SettingContainer
            title={t("settings.advanced.localApi.token.label")}
            description={t("settings.advanced.localApi.token.description")}
            descriptionMode={descriptionMode}
            grouped={grouped}
          >
            <div className="flex items-center gap-2">
              <input
                type="password"
                value={token}
                disabled={busy}
                placeholder={
                  status.has_token
                    ? t("settings.advanced.localApi.token.setPlaceholder")
                    : t("settings.advanced.localApi.token.unsetPlaceholder")
                }
                onChange={(e) => setToken(e.target.value)}
                onBlur={() => token.trim().length > 0 && apply({})}
                className="w-48 px-2 py-1 rounded border border-mid-gray/30 bg-background"
              />
              {status.has_token && (
                <button
                  type="button"
                  disabled={busy || status.allow_lan}
                  title={
                    status.allow_lan
                      ? t("settings.advanced.localApi.token.lanNeedsToken")
                      : undefined
                  }
                  onClick={async () => {
                    setToken("");
                    setBusy(true);
                    try {
                      const result = await commands.setServerSettings(
                        status.enabled,
                        status.port,
                        null,
                        status.allow_lan,
                      );
                      if (result.status === "error") setError(result.error);
                    } finally {
                      setBusy(false);
                      await refresh().catch(() => undefined);
                    }
                  }}
                  className="text-xs underline opacity-70 hover:opacity-100 disabled:opacity-30"
                >
                  {t("settings.advanced.localApi.token.clear")}
                </button>
              )}
            </div>
          </SettingContainer>

          <ToggleSwitch
            checked={status.allow_lan}
            disabled={busy || !status.has_token}
            onChange={(allowLan) => apply({ allowLan })}
            label={t("settings.advanced.localApi.allowLan.label")}
            description={t("settings.advanced.localApi.allowLan.description")}
            descriptionMode={descriptionMode}
            grouped={grouped}
          />

          <SettingContainer
            title={t("settings.advanced.localApi.status.label")}
            description={t("settings.advanced.localApi.status.description")}
            descriptionMode={descriptionMode}
            grouped={grouped}
          >
            <div className="text-xs text-right">
              {error ? (
                <span className="text-red-500">{error}</span>
              ) : status.running ? (
                <code className="opacity-80">{url}</code>
              ) : (
                <span className="opacity-60">
                  {t("settings.advanced.localApi.status.stopped")}
                </span>
              )}
            </div>
          </SettingContainer>
        </>
      )}
    </>
  );
};
