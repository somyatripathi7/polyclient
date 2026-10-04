import type { MqOverview } from "../../../api/rabbitmq.ts";
import { Stat } from "../../DataTable";
import { fmtRate } from "./format";

export function OverviewTab({ overview: o }: { overview: MqOverview | null }) {
  if (!o) return <div className="placeholder">No data.</div>;
  const t = o.object_totals;
  const q = o.queue_totals;
  const m = o.message_stats;
  return (
    <div className="mq-overview">
      <div className="mq-banner">
        <strong>{o.cluster_name || "RabbitMQ"}</strong>
        <span className="muted">
          node {o.node || "?"} · RabbitMQ {o.rabbitmq_version || "?"} · Erlang{" "}
          {o.erlang_version || "?"}
        </span>
      </div>

      <h3>Queued messages</h3>
      <div className="mq-stats">
        <Stat label="Ready" value={q.messages_ready} />
        <Stat label="Unacked" value={q.messages_unacknowledged} />
        <Stat label="Total" value={q.messages} />
      </div>

      <h3>Message rates</h3>
      <div className="mq-stats">
        <Stat label="Publish" value={fmtRate(m.publish_details?.rate)} />
        <Stat label="Deliver / get" value={fmtRate(m.deliver_get_details?.rate)} />
        <Stat label="Ack" value={fmtRate(m.ack_details?.rate)} />
      </div>

      <h3>Global counts</h3>
      <div className="mq-stats">
        <Stat label="Connections" value={t.connections} />
        <Stat label="Channels" value={t.channels} />
        <Stat label="Exchanges" value={t.exchanges} />
        <Stat label="Queues" value={t.queues} />
        <Stat label="Consumers" value={t.consumers} />
      </div>
    </div>
  );
}
