interface NavTreeItem {
  name: string;
  /** Optional badge shown after the name (e.g. a non-"table" relation kind). */
  badge?: string | null;
}

interface NavTreeProps {
  /** Top-level groups: schemas (RDBMS) or databases (MongoDB). */
  groups: string[];
  /** Leaf items keyed by group name: tables (RDBMS) or collections (MongoDB). */
  items: Record<string, NavTreeItem[]>;
  /** The expanded group, or null. */
  openGroup: string | null;
  /** The selected leaf as `group.name`, or null. */
  activeKey: string | null;
  onToggleGroup: (name: string) => void;
  onPickItem: (group: string, name: string) => void;
  /** Text shown when there are no groups. */
  emptyText?: string;
}

/** The collapsible group → leaf navigation tree shared by the RDBMS and MongoDB
 * workspaces: schema → table and database → collection have the same shape. */
export function NavTree({
  groups,
  items,
  openGroup,
  activeKey,
  onToggleGroup,
  onPickItem,
  emptyText,
}: NavTreeProps) {
  return (
    <div className="tree-schemas">
      {groups.length === 0 && <p className="muted">{emptyText ?? "Nothing here."}</p>}
      {groups.map((g) => (
        <div key={g} className="tree-group">
          <div
            className={"tree-node" + (openGroup === g ? " active" : "")}
            onClick={() => onToggleGroup(g)}
          >
            <span className="tree-caret">{openGroup === g ? "▾" : "▸"}</span>
            <span>{g}</span>
          </div>
          {openGroup === g &&
            (items[g] ?? []).map((it) => (
              <div
                key={it.name}
                className={"tree-node leaf" + (activeKey === g + "." + it.name ? " active" : "")}
                onClick={() => onPickItem(g, it.name)}
              >
                <span>{it.name}</span>
                {it.badge && <span className="tree-kind">{it.badge}</span>}
              </div>
            ))}
        </div>
      ))}
    </div>
  );
}
