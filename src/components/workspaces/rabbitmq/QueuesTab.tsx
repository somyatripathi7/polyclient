import { useState } from "react";
import { api, errString } from "../../../api/api.ts";
import type { ConnectionId } from "../../../api/api.ts";
import { DataTable, Stat, type Col } from "../../DataTable";
import { fmtBytes, fmtRate, vhostLabel } from "./format";
import { MqMessage, MqQueue } from "../../../api/rabbitmq.ts";

export function QueuesTab({
  connectionId,
  queues,
  onChanged,
  onError,
}: {
  connectionId: ConnectionId;
  queues: MqQueue[];
  onChanged: () => void;
  onError: (e: string | null) => void;
}) {
  const [selected, setSelected] = useState<MqQueue | null>(null);
  const [newName, setNewName] = useState("");
  const [newDurable, setNewDurable] = useState(true);
  const [busy, setBusy] = useState(false);

  // Keep the open detail panel in sync with refreshed data.
  const live = selected
    ? (queues.find((q) => q.name === selected.name && q.vhost === selected.vhost) ?? selected)
    : null;

  async function guard(fn: () => Promise<void>) {
    setBusy(true);
    onError(null);
    try {
      await fn();
    } catch (e) {
      onError(errString(e));
    } finally {
      setBusy(false);
    }
  }

  const declare = () =>
    guard(async () => {
      await api.mqDeclareQueue(connectionId, "/", newName.trim(), newDurable);
      setNewName("");
      onChanged();
    });

  const cols: Col<MqQueue>[] = [
    {
      key: "name",
      label: "Name",
      render: (r) => r.name,
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
      key: "state",
      label: "State",
      render: (r) => r.state,
      sortVal: (r) => r.state,
    },
    {
      key: "ready",
      label: "Ready",
      align: "right",
      render: (r) => r.messages_ready,
      sortVal: (r) => r.messages_ready,
    },
    {
      key: "unacked",
      label: "Unacked",
      align: "right",
      render: (r) => r.messages_unacknowledged,
      sortVal: (r) => r.messages_unacknowledged,
    },
    {
      key: "total",
      label: "Total",
      align: "right",
      render: (r) => r.messages,
      sortVal: (r) => r.messages,
    },
    {
      key: "consumers",
      label: "Consumers",
      align: "right",
      render: (r) => r.consumers,
      sortVal: (r) => r.consumers,
    },
    {
      key: "incoming",
      label: "Incoming",
      align: "right",
      render: (r) => fmtRate(r.message_stats?.publish_details?.rate),
      sortVal: (r) => r.message_stats?.publish_details?.rate ?? 0,
    },
  ];

  return (
    <div className="mq-split">
      <div className="mq-main">
        <div className="section">
          <h3>Add queue</h3>
          <div className="row">
            <input
              className="grow"
              placeholder="queue name"
              value={newName}
              onChange={(e) => setNewName(e.target.value)}
            />
            <label className="row">
              <input
                type="checkbox"
                checked={newDurable}
                onChange={(e) => setNewDurable(e.target.checked)}
              />{" "}
              durable
            </label>
            <button disabled={busy || !newName.trim()} onClick={declare}>
              Declare
            </button>
          </div>
        </div>
        <DataTable
          cols={cols}
          rows={queues}
          rowKey={(r) => r.vhost + "/" + r.name}
          onRowClick={(r) => setSelected(r)}
          selectedKey={live ? live.vhost + "/" + live.name : null}
          empty="No queues."
        />
      </div>
      {live && (
        <QueueDetail
          connectionId={connectionId}
          queue={live}
          busy={busy}
          guard={guard}
          onChanged={onChanged}
          onClose={() => setSelected(null)}
        />
      )}
    </div>
  );
}

function QueueDetail({
  connectionId,
  queue,
  busy,
  guard,
  onChanged,
  onClose,
}: {
  connectionId: ConnectionId;
  queue: MqQueue;
  busy: boolean;
  guard: (fn: () => Promise<void>) => Promise<void>;
  onChanged: () => void;
  onClose: () => void;
}) {
  const [payload, setPayload] = useState("");
  const [count, setCount] = useState(1);
  const [remove, setRemove] = useState(false);
  const [messages, setMessages] = useState<MqMessage[]>([]);

  const publish = () =>
    guard(async () => {
      // Empty exchange + queue name as routing key = default exchange.
      await api.mqPublish(connectionId, queue.vhost, "", queue.name, payload);
    });

  const getMessages = () =>
    guard(async () => {
      const ms = await api.mqGetMessages(
        connectionId,
        queue.vhost,
        queue.name,
        count,
        remove ? "ack_requeue_false" : "ack_requeue_true",
      );
      setMessages(ms);
      if (remove) onChanged();
    });

  const purge = () =>
    guard(async () => {
      await api.mqPurgeQueue(connectionId, queue.vhost, queue.name);
      setMessages([]);
      onChanged();
    });

  const del = () =>
    guard(async () => {
      await api.mqDeleteQueue(connectionId, queue.vhost, queue.name);
      onChanged();
      onClose();
    });

  return (
    <div className="mq-detail">
      <div className="mq-detail-head">
        <strong>{queue.name}</strong>
        <button className="icon-btn" onClick={onClose} title="Close">
          ✕
        </button>
      </div>
      <div className="muted small">
        vhost {vhostLabel(queue.vhost)} · {queue.type || "classic"} ·{" "}
        {queue.durable ? "durable" : "transient"} · {fmtBytes(queue.memory)}
      </div>
      <div className="mq-stats">
        <Stat label="Ready" value={queue.messages_ready} />
        <Stat label="Unacked" value={queue.messages_unacknowledged} />
        <Stat label="Consumers" value={queue.consumers} />
      </div>

      <div className="section">
        <h3>Publish message</h3>
        <textarea
          className="code"
          placeholder="message payload"
          value={payload}
          onChange={(e) => setPayload(e.target.value)}
        />
        <div className="row">
          <button disabled={busy} onClick={publish}>
            Publish to this queue
          </button>
        </div>
      </div>

      <div className="section">
        <h3>Get messages</h3>
        <div className="row">
          <label className="row">
            count{" "}
            <input
              type="number"
              min={1}
              value={count}
              style={{ width: 70 }}
              onChange={(e) => setCount(Number(e.target.value) || 1)}
            />
          </label>
          <label className="row">
            <input type="checkbox" checked={remove} onChange={(e) => setRemove(e.target.checked)} />{" "}
            remove from queue
          </label>
          <button disabled={busy} onClick={getMessages}>
            Get
          </button>
        </div>
      </div>

      <div className="row">
        <button disabled={busy} onClick={purge}>
          Purge
        </button>
        <button className="row-del" disabled={busy} onClick={del}>
          Delete queue
        </button>
      </div>

      <div className="msg-list">
        {messages.map((m, i) => (
          <div key={i} className="msg-item">
            <div className="msg-meta">
              {m.payload_bytes} byte(s) · key {m.routing_key || "(none)"}
              {m.redelivered ? " · redelivered" : ""}
            </div>
            {m.payload}
          </div>
        ))}
      </div>
    </div>
  );
}
