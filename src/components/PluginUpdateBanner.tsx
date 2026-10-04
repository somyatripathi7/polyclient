interface PluginUpdateBannerProps {
  /** How many installed plugins have a newer release available. */
  count: number;
  /** Open the install/update dialog so the user can apply the updates. */
  onReview: () => void;
  onDismiss: () => void;
  /** When true, sit above the app-update banner so the two don't overlap. */
  stacked?: boolean;
}

/** Non-blocking toast shown on launch when one or more installed plugins have
 *  a newer release available. "Update plugins" opens the install dialog (where
 *  each outdated plugin shows an "Update" action); "Later" dismisses it. */
export function PluginUpdateBanner({
  count,
  onReview,
  onDismiss,
  stacked,
}: PluginUpdateBannerProps) {
  return (
    <div className={"update-banner" + (stacked ? " update-banner-stacked" : "")}>
      <div className="update-banner-main">
        <strong>Plugin updates available</strong>
        <span className="muted small">
          {count === 1 ? "1 plugin can be updated" : `${count} plugins can be updated`}
        </span>
      </div>
      <div className="update-banner-actions">
        <button className="primary" onClick={onReview}>
          Update plugins
        </button>
        <button onClick={onDismiss}>Later</button>
      </div>
    </div>
  );
}
