import type { MqChannel, MqConnection } from "../../../api/rabbitmq.ts";
import { DataTable, type Col } from "../../DataTable";
import { vhostLabel } from "./format";

export function ConnectionsTab({
  connections,
  channels,
}: {
  connections: MqConnection[];
  channels: MqChannel[];
}) {
  const connCols: Col<MqConnection>[] = [
    { key: "name", label: "Name", render: (r) => r.name, sortVal: (r) => r.name },
    { key: "user", label: "User", render: (r) => r.user, sortVal: (r) => r.user },
    {
      key: "vhost",
      label: "Vhost",
      render: (r) => vhostLabel(r.vhost),
      sortVal: (r) => r.vhost,
    },
    {
      key: "state",
      label: "State",
      render: (r) => r.state,
      sortVal: (r) => r.state,
    },
    {
      key: "protocol",
      label: "Protocol",
      render: (r) => r.protocol,
      sortVal: (r) => r.protocol,
    },
    {
      key: "peer",
      label: "Peer",
      render: (r) => `${r.peer_host}:${r.peer_port}`,
    },
    {
      key: "ssl",
      label: "TLS",
      render: (r) => (r.ssl ? "yes" : "no"),
    },
    {
      key: "channels",
      label: "Channels",
      align: "right",
      render: (r) => r.channels,
      sortVal: (r) => r.channels,
    },
  ];

  const chanCols: Col<MqChannel>[] = [
    { key: "name", label: "Name", render: (r) => r.name, sortVal: (r) => r.name },
    { key: "user", label: "User", render: (r) => r.user, sortVal: (r) => r.user },
    {
      key: "vhost",
      label: "Vhost",
      render: (r) => vhostLabel(r.vhost),
      sortVal: (r) => r.vhost,
    },
    {
      key: "state",
      label: "State",
      render: (r) => r.state,
      sortVal: (r) => r.state,
    },
    {
      key: "consumers",
      label: "Consumers",
      align: "right",
      render: (r) => r.consumer_count,
      sortVal: (r) => r.consumer_count,
    },
    {
      key: "unacked",
      label: "Unacked",
      align: "right",
      render: (r) => r.messages_unacknowledged,
      sortVal: (r) => r.messages_unacknowledged,
    },
    {
      key: "prefetch",
      label: "Prefetch",
      align: "right",
      render: (r) => r.prefetch_count,
      sortVal: (r) => r.prefetch_count,
    },
  ];

  return (
    <div className="mq-main">
      <h3>Connections</h3>
      <DataTable
        cols={connCols}
        rows={connections}
        rowKey={(r) => r.name}
        empty="No open connections."
      />
      <h3>Channels</h3>
      <DataTable cols={chanCols} rows={channels} rowKey={(r) => r.name} empty="No open channels." />
    </div>
  );
}
