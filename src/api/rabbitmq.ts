import { ConnectionId, pluginCall } from "./api.ts";

// --- RabbitMQ (Management API) --------------------------------------------
// Mirrors the subset of the HTTP Management API payloads the plugin returns.

export interface ObjectTotals {
  queues: number;
  exchanges: number;
  connections: number;
  channels: number;
  consumers: number;
}

export interface QueueTotals {
  messages: number;
  messages_ready: number;
  messages_unacknowledged: number;
}

export interface RateDetails {
  rate: number;
}

export interface MessageStats {
  publish_details: RateDetails;
  deliver_get_details: RateDetails;
  ack_details: RateDetails;
}

export interface MqOverview {
  rabbitmq_version: string;
  erlang_version: string;
  cluster_name: string;
  node: string;
  object_totals: ObjectTotals;
  queue_totals: QueueTotals;
  message_stats: MessageStats;
}

export interface MqQueue {
  name: string;
  vhost: string;
  state: string;
  type: string;
  durable: boolean;
  auto_delete: boolean;
  messages: number;
  messages_ready: number;
  messages_unacknowledged: number;
  consumers: number;
  memory: number;
  message_stats: MessageStats;
}

export interface MqExchange {
  name: string;
  vhost: string;
  type: string;
  durable: boolean;
  auto_delete: boolean;
  internal: boolean;
}

export interface MqConnection {
  name: string;
  user: string;
  state: string;
  channels: number;
  protocol: string;
  peer_host: string;
  peer_port: number;
  vhost: string;
  ssl: boolean;
}

export interface MqChannel {
  name: string;
  user: string;
  state: string;
  vhost: string;
  number: number;
  consumer_count: number;
  messages_unacknowledged: number;
  prefetch_count: number;
}

export interface MqMessage {
  payload: string;
  payload_bytes: number;
  redelivered: boolean;
  routing_key: string;
  exchange: string;
  message_count: number;
}

export interface MqPublishResult {
  routed: boolean;
}

export interface MqPurgeResult {
  queue: string;
}

export const rabbitmq_api = {
  // RabbitMQ (Management API)
  mqOverview: (connectionId: ConnectionId) =>
    pluginCall<MqOverview>(connectionId, "rabbitmq.overview", {}),

  mqListQueues: (connectionId: ConnectionId) =>
    pluginCall<MqQueue[]>(connectionId, "rabbitmq.list_queues", {}),

  mqListExchanges: (connectionId: ConnectionId) =>
    pluginCall<MqExchange[]>(connectionId, "rabbitmq.list_exchanges", {}),

  mqListConnections: (connectionId: ConnectionId) =>
    pluginCall<MqConnection[]>(connectionId, "rabbitmq.list_connections", {}),

  mqListChannels: (connectionId: ConnectionId) =>
    pluginCall<MqChannel[]>(connectionId, "rabbitmq.list_channels", {}),

  /** Fetch up to `count` messages. `ackmode` is the Management API verb:
   *  `ack_requeue_true` peeks (requeues), `ack_requeue_false` removes them. */
  mqGetMessages: (
    connectionId: ConnectionId,
    vhost: string,
    queue: string,
    count: number,
    ackmode: string,
  ) =>
    pluginCall<MqMessage[]>(connectionId, "rabbitmq.get_messages", {
      vhost,
      queue,
      count,
      ackmode,
    }),

  /** Empty `exchange` targets the queue named by `routingKey` (default exchange). */
  mqPublish: (
    connectionId: ConnectionId,
    vhost: string,
    exchange: string,
    routingKey: string,
    payload: string,
  ) =>
    pluginCall<MqPublishResult>(connectionId, "rabbitmq.publish", {
      vhost,
      exchange,
      routing_key: routingKey,
      payload,
    }),

  mqPurgeQueue: (connectionId: ConnectionId, vhost: string, queue: string) =>
    pluginCall<MqPurgeResult>(connectionId, "rabbitmq.purge_queue", { vhost, queue }),

  mqDeclareQueue: (connectionId: ConnectionId, vhost: string, queue: string, durable: boolean) =>
    pluginCall<MqQueue>(connectionId, "rabbitmq.declare_queue", {
      vhost,
      queue,
      durable,
    }),

  mqDeleteQueue: (connectionId: ConnectionId, vhost: string, queue: string) =>
    pluginCall<void>(connectionId, "rabbitmq.delete_queue", { vhost, queue }),
};
