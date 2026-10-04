import { useState } from "react";
import { installUpdate, type UpdateInfo } from "../api/updater.ts";
import { errString } from "../api/api.ts";

interface UpdateBannerProps {
  update: UpdateInfo;
  onDismiss: () => void;
}

/** Non-blocking toast shown when a newer signed build is available. Install
 *  downloads + verifies + installs the update and relaunches the app. */
export function UpdateBanner({ update, onDismiss }: UpdateBannerProps) {
  const [busy, setBusy] = useState(false);
  const [pct, setPct] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function onInstall() {
    setError(null);
    setBusy(true);
    try {
      await installUpdate((downloaded, total) => {
        setPct(total ? Math.round((downloaded / total) * 100) : null);
      });
      // On success the app relaunches inside installUpdate; we won't get here.
    } catch (e) {
      setError(errString(e));
      setBusy(false);
    }
  }

  return (
    <div className="update-banner">
      <div className="update-banner-main">
        <strong>Update available</strong>
        <span className="muted small">
          v{update.currentVersion} → v{update.version}
        </span>
        {error && <span className="msg error small">{error}</span>}
      </div>
      <div className="update-banner-actions">
        {busy ? (
          <span className="muted small">
            {pct != null ? `Downloading ${pct}%…` : "Installing…"}
          </span>
        ) : (
          <>
            <button className="primary" onClick={onInstall}>
              Install &amp; restart
            </button>
            <button onClick={onDismiss}>Later</button>
          </>
        )}
      </div>
    </div>
  );
}
