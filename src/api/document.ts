import { ConnectionId, pluginCall } from "./api.ts";

// --- Document (MongoDB) ---------------------------------------------------

export interface MongoCollection {
  database: string;
  name: string;
}

/** Reply from a `db.runCommand`. `result` is the raw command reply as extended
 * JSON; for find/aggregate/listIndexes the documents are at
 * `result.cursor.firstBatch`. */
export interface RunCommandResult {
  result: unknown;
  elapsed_ms: number;
}

export const document_api = {
  // Document
  docListDatabases: (connectionId: ConnectionId) =>
    pluginCall<string[]>(connectionId, "document.list_databases", {}),

  docListCollections: (connectionId: ConnectionId, database: string) =>
    pluginCall<MongoCollection[]>(connectionId, "document.list_collections", { database }),

  /** Run an arbitrary database command. `command` is the command document as
   * a (extended) JSON string; the Documents query, the aggregation pipeline,
   * and index listing are all expressed as commands. */
  docRunCommand: (connectionId: ConnectionId, database: string, command: string) =>
    pluginCall<RunCommandResult>(connectionId, "document.run_command", {
      database,
      command,
    }),
};
