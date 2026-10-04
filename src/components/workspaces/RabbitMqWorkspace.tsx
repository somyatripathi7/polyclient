import { useCallback, useEffect, useState } from "react";
import { api, errString } from "../../api/api.ts";
import type { ConnectionId } from "../../api/api.ts";
import { OverviewTab } from "./rabbitmq/OverviewTab";
import { QueuesTab } from "./rabbitmq/QueuesTab";
import { ExchangesTab } from "./rabbitmq/ExchangesTab";
import { ConnectionsTab } from "./rabbitmq/ConnectionsTab";
import { ConnScope, useConnectionState } from "../../connectionState";
import { MqChannel, MqConnection, MqExchange, MqOverview, MqQueue } from "../../api/rabbitmq.ts";

interface Props {
  connectionId: ConnectionId;
  /** Stable saved-profile id; scopes session-preserved workspace state. */
  savedId: string;
}

type Tab = "overview" | "queues" | "exchanges" | "connections";

const TABS: { id: Tab; label: string }[] = [
  { id: "overview", label: "Overview" },
  { id: "queues", label: "Queues" },
  { id: "exchanges", label: "Exchanges" },
  { id: "connections", label: "Connections" },
];

const REFRESH_OPTIONS = [
  { label: "Off", ms: 0 },
  { label: "2s", ms: 2000 },
  { label: "5s", ms: 5000 },
  { label: "10s", ms: 10000 },
];

export function RabbitMqWorkspace({ connectionId, savedId }: Props) {
  // The active tab + refresh interval persist across connection switches (tab
  // data itself is refetched on tab change, so it isn't stored).
  const scope = ConnScope(savedId, "rabbitmq");
  const [tab, setTab] = useConnectionState<Tab>(scope, "tab", "overview");
  const [refreshMs, setRefreshMs] = useConnectionState(scope, "refreshMs", 0);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const [overview, setOverview] = useState<MqOverview | null>(null);
  const [queues, setQueues] = useState<MqQueue[]>([]);
  const [exchanges, setExchanges] = useState<MqExchange[]>([]);
  const [connections, setConnections] = useState<MqConnection[]>([]);
  const [channels, setChannels] = useState<MqChannel[]>([]);

  const load = useCallback(
    async (active: Tab) => {
      setLoading(true);
      setError(null);
      try {
        switch (active) {
          case "overview":
            setOverview(await api.mqOverview(connectionId));
            break;
          case "queues":
            setQueues(await api.mqListQueues(connectionId));
            break;
          case "exchanges":
            setExchanges(await api.mqListExchanges(connectionId));
            break;
          case "connections":
            setConnections(await api.mqListConnections(connectionId));
            setChannels(await api.mqListChannels(connectionId));
            break;
        }
      } catch (e) {
        setError(errString(e));
      } finally {
        setLoading(false);
      }
    },
    [connectionId],
  );

  // Reload whenever the active tab changes.
  useEffect(() => {
    load(tab);
  }, [tab, load]);

  // Auto-refresh the active tab on the chosen interval.
  useEffect(() => {
    if (!refreshMs) return;
    const h = setInterval(() => load(tab), refreshMs);
    return () => clearInterval(h);
  }, [refreshMs, tab, load]);

  return (
    <div className="mq">
      <div className="mq-tabbar">
        {TABS.map((t) => (
          <button
            key={t.id}
            className={"mq-tab" + (tab === t.id ? " active" : "")}
            onClick={() => setTab(t.id)}
          >
            {t.label}
          </button>
        ))}
        <span className="spacer" />
        {loading && <span className="mq-spin">…</span>}
        <label className="mq-refresh">
          refresh
          <select value={refreshMs} onChange={(e) => setRefreshMs(Number(e.target.value))}>
            {REFRESH_OPTIONS.map((o) => (
              <option key={o.ms} value={o.ms}>
                {o.label}
              </option>
            ))}
          </select>
        </label>
        <button onClick={() => load(tab)} disabled={loading}>
          Refresh
        </button>
      </div>

      {error && <div className="status-line error">{error}</div>}

      <div className="mq-body">
        {tab === "overview" && <OverviewTab overview={overview} />}
        {tab === "queues" && (
          <QueuesTab
            connectionId={connectionId}
            queues={queues}
            onChanged={() => load("queues")}
            onError={setError}
          />
        )}
        {tab === "exchanges" && (
          <ExchangesTab connectionId={connectionId} exchanges={exchanges} onError={setError} />
        )}
        {tab === "connections" && <ConnectionsTab connections={connections} channels={channels} />}
      </div>
    </div>
  );
}
