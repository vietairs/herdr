// installed by herdr
// managed by herdr; reinstalling or updating the integration overwrites this file.
// add custom hooks/plugins beside this file instead of editing it.
// HERDR_INTEGRATION_ID=kilo
// HERDR_INTEGRATION_VERSION=5

import net from "node:net";

const SOURCE = "herdr:kilo";
const AGENT = "kilo";
let reportSeq = Date.now() * 1000;

// Child sessions are subagent runs; their messages are not the pane's context.
const childSessions = new Set();

function nextReportSeq() {
  reportSeq += 1;
  return reportSeq;
}

function sessionIDFromProperties(properties) {
  return typeof properties?.sessionID === "string" && properties.sessionID
    ? properties.sessionID
    : undefined;
}

function stateFromSessionStatus(status) {
  if (typeof status !== "string") {
    return undefined;
  }
  switch (status.toLowerCase()) {
    case "idle":
      return "idle";
    case "active":
    case "busy":
    case "pending":
    case "running":
    case "streaming":
    case "working":
      return "working";
    default:
      return undefined;
  }
}

function request(method, params) {
  const paneId = process.env.HERDR_PANE_ID;
  const socketPath = process.env.HERDR_SOCKET_PATH;

  if (!paneId || !socketPath) {
    return Promise.resolve();
  }

  const socketEndpoint =
    process.platform === "win32" ? `\\\\.\\pipe\\${socketPath}` : socketPath;

  const requestId = `${SOURCE}:${Date.now()}:${Math.floor(Math.random() * 1_000_000)
    .toString()
    .padStart(6, "0")}`;
  const request = {
    id: requestId,
    method,
    params: {
      pane_id: paneId,
      source: SOURCE,
      agent: AGENT,
      seq: nextReportSeq(),
      ...params,
    },
  };

  return new Promise((resolve) => {
    const client = net.createConnection(socketEndpoint, () => {
      client.write(`${JSON.stringify(request)}\n`);
    });

    const finish = () => {
      client.destroy();
      resolve();
    };

    client.setTimeout(500, finish);
    client.on("data", finish);
    client.on("error", finish);
    client.on("end", finish);
    client.on("close", resolve);
  });
}

// Context usage. The window comes from the plugin client's provider catalog:
// `client.config.providers()` resolves to `{ data: { providers: Provider[] } }`
// in the OpenCode v1 and v2 SDKs and the Kilo SDK, and the window is
// `providers[i].models[modelID].limit.context` of the provider whose id matches
// the assistant message's `providerID`. Neither agent exposes a prompt-cache
// lifetime, so only the context fact is reported.
const CONTEXT_REPORT_MEMORY = 256;
const reportedContextMessages = new Set();
const modelWindows = new Map();

function tokenCount(value) {
  if (value === undefined || value === null) return 0;
  return Number.isInteger(value) && value >= 0 ? value : undefined;
}

// Builds the report params for a finished assistant message, or null when the
// message is still streaming, is not an assistant turn, or carries no usable
// token counts (a turn that never reached the model leaves no reading).
function contextUsageParams(info, windowTokens) {
  const completed = info?.time?.completed;
  if (info?.role !== "assistant" || !Number.isInteger(completed) || completed <= 0) return null;
  const input = tokenCount(info.tokens?.input);
  const read = tokenCount(info.tokens?.cache?.read);
  const write = tokenCount(info.tokens?.cache?.write);
  if (input === undefined || read === undefined || write === undefined) return null;
  const used = input + read + write;
  if (used === 0) return null;
  const params = {
    pane_id: process.env.HERDR_PANE_ID,
    source: SOURCE,
    used_tokens: used,
    observed_at_ms: completed,
  };
  if (Number.isInteger(windowTokens) && windowTokens > 0) params.window_tokens = windowTokens;
  return params;
}

// Returns true the first time a finished message is seen, so a replayed update
// is not reported twice. Memory is bounded to the most recent messages.
function claimContextReport(info) {
  const key = `${info.id}:${info.time.completed}`;
  if (reportedContextMessages.has(key)) return false;
  reportedContextMessages.add(key);
  if (reportedContextMessages.size > CONTEXT_REPORT_MEMORY) {
    reportedContextMessages.delete(reportedContextMessages.values().next().value);
  }
  return true;
}

// Resolves a model's context window once per provider/model. Never throws: a
// failed lookup is forgotten so a later message retries, and an unknown model
// stays unknown (the reading is then tokens only).
async function modelContextWindow(client, providerID, modelID) {
  if (typeof providerID !== "string" || typeof modelID !== "string" || typeof client?.config?.providers !== "function") {
    return undefined;
  }
  const key = `${providerID}/${modelID}`;
  let lookup = modelWindows.get(key);
  if (!lookup) {
    lookup = (async () => {
      const result = await client.config.providers();
      const provider = result?.data?.providers?.find((candidate) => candidate?.id === providerID);
      const limit = provider?.models?.[modelID]?.limit?.context;
      return Number.isInteger(limit) && limit > 0 ? limit : undefined;
    })();
    modelWindows.set(key, lookup);
  }
  try {
    return await lookup;
  } catch {
    modelWindows.delete(key);
    return undefined;
  }
}

// Reports the context fact without blocking the lifecycle reports: the provider
// lookup is awaited only inside this detached task.
function reportContextUsage(client, info) {
  if (!contextUsageParams(info) || !claimContextReport(info)) return;
  void (async () => {
    const windowTokens = await modelContextWindow(client, info.providerID, info.modelID);
    const params = contextUsageParams(info, windowTokens);
    if (params) await request("pane.report_context_usage", params);
  })().catch(() => {});
}

function reportSession(sessionID) {
  if (!sessionID) {
    return Promise.resolve();
  }
  return request("pane.report_agent_session", {
    agent_session_id: sessionID,
    session_start_source: "startup",
  });
}

function reportState(state, sessionID) {
  const params = { state };
  if (sessionID) {
    params.agent_session_id = sessionID;
  }
  return request("pane.report_agent", params);
}

export const HerdrAgentStatePlugin = async ({ client } = {}) => {
  if (
    process.env.HERDR_ENV !== "1" ||
    !process.env.HERDR_SOCKET_PATH ||
    !process.env.HERDR_PANE_ID
  ) {
    return {};
  }

  return {
    "chat.message": async ({ sessionID }) => {
      await reportState("working", sessionID);
    },
    event: async ({ event }) => {
      const type = event?.type;
      const properties = event?.properties ?? {};
      const sessionID = sessionIDFromProperties(properties);

      // A message's parentID names its parent message, not a parent session.
      if (type !== "message.updated" && properties.info?.id && properties.info.parentID) {
        childSessions.add(properties.info.id);
      }

      switch (type) {
        case "session.created":
        case "session.updated":
          await reportSession(sessionID);
          break;
        case "session.status": {
          const state = stateFromSessionStatus(properties.status);
          if (state) {
            await reportState(state, sessionID);
          } else {
            await reportSession(sessionID);
          }
          break;
        }
        case "tool.execute.before":
        case "tool.execute.after":
        case "permission.replied":
        case "question.replied":
        case "question.rejected":
        case "session.compacted":
          await reportState("working", sessionID);
          break;
        case "permission.asked":
        case "question.asked":
        case "session.error":
          await reportState("blocked", sessionID);
          break;
        case "session.idle":
          await reportState("idle", sessionID);
          break;
        case "message.updated":
          if (!childSessions.has(properties.info?.sessionID)) {
            reportContextUsage(client, properties.info);
          }
          break;
        case "session.deleted":
          break;
        default:
          break;
      }
    },
  };
};
