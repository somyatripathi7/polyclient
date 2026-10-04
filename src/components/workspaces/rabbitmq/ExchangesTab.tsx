import { useState } from "react";
import { api, errString } from "../../../api/api.ts";
import type { ConnectionId } from "../../../api/api.ts";
import { DataTable, type Col } from "../../DataTable";
import { vhostLabel } from "./format";
import { MqExchange } from "../../../api/rabbitmq.ts";

export function ExchangesTab({
  connectionId,
  exchanges,
  onError,
}: {
  connectionId: ConnectionId;
  exchanges: MqExchange[];
  onError: (e: string | null) => void;
}) {
  const [selected, setSelected] = useState<MqExchange | null>(null);
  const [routingKey, setRoutingKey] = useState("");
  const [payload, setPayload] = useState("");
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<string | null>(null);

  const live = selected
    ? (exchanges.find((x) => x.name === selected.name && x.vhost === selected.vhost) ?? selected)
    : null;

  const publish = () =>
    (async () => {
      setBusy(true);
      onError(null);
      setStatus(null);
      try {
        const r = await api.mqPublish(connectionId, live!.vhost, live!.name, routingKey, payload);
        setStatus(r.routed ? "Published (routed)." : "Published, but not routed to any queue.");
      } catch (e) {
        onError(errString(e));
      } finally {
        setBusy(false);
      }
    })();

  const cols: Col<MqExchange>[] = [
    {
      key: "name",
      label: "Name",
      render: (r) => r.name || "(AMQP default)",
      sortVal: (r) => r.name,
    },
    {
      key: "vhost",
      label: "Vhost",
      render: (r) => vhostLabel(r.vhost),
      sortVal: (r) => r.vhost,
    },
    { key: "type", label: "Type", render: (r) => r.type, sortVal: (r) => r.type },
    {
      key: "durable",
      label: "Durable",
      render: (r) => (r.durable ? "yes" : "no"),
      sortVal: (r) => String(r.durable),
    },
    {
      key: "auto_delete",
      label: "Auto-delete",
      render: (r) => (r.auto_delete ? "yes" : "no"),
    },
    {
      key: "internal",
      label: "Internal",
      render: (r) => (r.internal ? "yes" : "no"),
    },
  ];

  return (
    <div className="mq-split">
      <div className="mq-main">
        <DataTable
          cols={cols}
          rows={exchanges}
          rowKey={(r) => r.vhost + "/" + r.name}
          onRowClick={(r) => {
            setSelected(r);
            setStatus(null);
          }}
          selectedKey={live ? live.vhost + "/" + live.name : null}
          empty="No exchanges."
        />
      </div>
      {live && (
        <div className="mq-detail">
          <div className="mq-detail-head">
            <strong>{live.name || "(AMQP default)"}</strong>
            <button className="icon-btn" onClick={() => setSelected(null)} title="Close">
              ✕
            </button>
          </div>
          <div className="muted small">
            vhost {vhostLabel(live.vhost)} · {live.type}
          </div>
          <div className="section">
            <h3>Publish message</h3>
            <input
              className="grow"
              placeholder="routing key"
              value={routingKey}
              onChange={(e) => setRoutingKey(e.target.value)}
            />
            <textarea
              className="code"
              placeholder="message payload"
              value={payload}
              onChange={(e) => setPayload(e.target.value)}
            />
            <div className="row">
              <button disabled={busy} onClick={publish}>
                Publish
              </button>
              {status && <span className="status-line">{status}</span>}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
