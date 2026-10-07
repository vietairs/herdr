import { afterEach, expect, test } from "bun:test";
import { rm } from "node:fs/promises";
import net, { createServer, type Server } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

const originalPlatform = process.platform;
const originalArgv = process.argv;
const originalCreateConnection = net.createConnection;
const originalEnvironment = {
  HERDR_ENV: process.env.HERDR_ENV,
  HERDR_OMP_IDLE_DEBOUNCE_MS: process.env.HERDR_OMP_IDLE_DEBOUNCE_MS,
  HERDR_PANE_ID: process.env.HERDR_PANE_ID,
  HERDR_SOCKET_PATH: process.env.HERDR_SOCKET_PATH,
  OMPCODE: process.env.OMPCODE,
};

let server: Server | undefined;
let socketPath: string | undefined;
let importCounter = 0;

afterEach(async () => {
  await new Promise<void>((resolve, reject) => {
    if (!server) {
      resolve();
      return;
    }
    server.close((error) => (error ? reject(error) : resolve()));
  });
  server = undefined;

  if (socketPath) {
    await rm(socketPath, { force: true });
    socketPath = undefined;
  }

  Object.defineProperty(process, "platform", { value: originalPlatform });
  net.createConnection = originalCreateConnection;
  process.argv = originalArgv;
  for (const [name, value] of Object.entries(originalEnvironment)) {
    if (value === undefined) {
      delete process.env[name];
    } else {
      process.env[name] = value;
    }
  }
});

const integrations = [
  { name: "Pi", modulePath: "./pi/herdr-agent-state.ts" },
  { name: "Oh My Pi", modulePath: "./omp/herdr-agent-state.ts" },
] as const;

const socketPlugins = [
  {
    name: "OpenCode",
    modulePath: "./opencode/herdr-agent-state.js",
    sessionID: "opencode-session",
  },
  { name: "Kilo", modulePath: "./kilo/herdr-agent-state.js", sessionID: "kilo-session" },
] as const;

function importFresh(modulePath: string) {
  importCounter += 1;
  return import(`${modulePath}?test=${importCounter}`);
}

type Handler = (event: unknown, context: unknown) => unknown;

function createExtensionHarness() {
  const handlers = new Map<string, Handler>();
  const eventHandlers = new Map<string, Handler>();
  return {
    handlers,
    eventHandlers,
    pi: {
      on(event: string, handler: Handler) {
        handlers.set(event, handler);
      },
      events: {
        on(event: string, handler: Handler) {
          eventHandlers.set(event, handler);
          return () => {};
        },
      },
    },
  };
}

function configureIntegrationEnvironment(recordingSocketPath: string) {
  // Tests may run inside an OMP shell; nested-session cases opt in explicitly.
  delete process.env.OMPCODE;
  process.env.HERDR_ENV = "1";
  process.env.HERDR_SOCKET_PATH = recordingSocketPath;
  process.env.HERDR_PANE_ID = "test:p1";
}

function captureConnectionEndpoint() {
  let connectedEndpoint: unknown;
  net.createConnection = ((...args: unknown[]) => {
    connectedEndpoint = args[0];
    return Reflect.apply(originalCreateConnection, net, args);
  }) as typeof net.createConnection;
  return () => connectedEndpoint;
}

async function startRecordingServer(name: string): Promise<unknown[]> {
  const recordingSocketPath = join(tmpdir(), `herdr-${name}-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });

  const requests: unknown[] = [];
  const recordingServer = createServer((socket) => {
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline === -1) {
        return;
      }
      requests.push(JSON.parse(input.slice(0, newline)));
      socket.end("{}\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(originalPlatform === "win32" ? `\\\\.\\pipe\\${recordingSocketPath}` : recordingSocketPath, resolve);
  });
  configureIntegrationEnvironment(recordingSocketPath);
  return requests;
}

for (const socketPlugin of socketPlugins) {
  test(`${socketPlugin.name} maps the Windows socket marker path to a named pipe endpoint`, async () => {
    const markerPath = `herdr-${socketPlugin.name.toLowerCase()}-${process.pid}.sock`;
    configureIntegrationEnvironment(markerPath);
    Object.defineProperty(process, "platform", { value: "win32" });
    const connectedEndpoint = captureConnectionEndpoint();

    process.argv = ["bun", "/$bunfs/root/src/index.js", "run"];
    const { HerdrAgentStatePlugin } = await importFresh(socketPlugin.modulePath);
    const plugin = await HerdrAgentStatePlugin();
    await plugin.event({
      event: {
        type: "session.updated",
        properties: { sessionID: socketPlugin.sessionID },
      },
    });

    expect(connectedEndpoint()).toBe(`\\\\.\\pipe\\${markerPath}`);
  });
}

test("OpenCode stays disabled without the Herdr socket environment", async () => {
  process.env.HERDR_ENV = "1";
  process.env.HERDR_PANE_ID = "test:p1";
  delete process.env.HERDR_SOCKET_PATH;

  const { HerdrAgentStatePlugin } = await importFresh("./opencode/herdr-agent-state.js");

  expect(await HerdrAgentStatePlugin()).toEqual({});
});

for (const integration of integrations) {
  test(`${integration.name} maps the Windows socket marker path to a named pipe endpoint`, async () => {
    const markerPath = `herdr-${integration.name.toLowerCase().replaceAll(" ", "-")}-${process.pid}.sock`;
    configureIntegrationEnvironment(markerPath);
    Object.defineProperty(process, "platform", { value: "win32" });
    const connectedEndpoint = captureConnectionEndpoint();
    const { handlers, pi } = createExtensionHarness();

    const { default: install } = await importFresh(integration.modulePath);
    install(pi);
    await handlers.get("session_start")?.(
      { reason: "startup" },
      {
        hasUI: true,
        mode: "tui",
        isIdle: () => true,
        sessionManager: {
          getSessionFile: () => undefined,
          getSessionId: () => "test-session",
        },
      },
    );

    expect(connectedEndpoint()).toBe(`\\\\.\\pipe\\${markerPath}`);
  });

  test(`${integration.name} reload preserves working state when the agent is active`, async () => {
    const requests = await startRecordingServer(
      integration.name.toLowerCase().replaceAll(" ", "-"),
    );
    const { handlers, pi } = createExtensionHarness();

    const { default: install } = await importFresh(integration.modulePath);
    install(pi);

    const sessionStart = handlers.get("session_start");
    expect(sessionStart).toBeDefined();
    await sessionStart?.(
      { reason: "reload" },
      {
        hasUI: true,
        mode: "tui",
        isIdle: () => false,
        sessionManager: {
          getSessionFile: () => undefined,
          getSessionId: () => undefined,
        },
      },
    );

    const reportedState = () => {
      for (const request of requests) {
        if (!isRecord(request) || request.method !== "pane.report_agent") {
          continue;
        }
        const params = request.params;
        if (isRecord(params) && typeof params.state === "string") {
          return params.state;
        }
      }
      return undefined;
    };

    const deadline = Date.now() + 1_000;
    while (Date.now() < deadline && reportedState() === undefined) {
      await Bun.sleep(5);
    }

    expect(reportedState()).toBe("working");
  });
}

test("OMP ignores nested sessions launched inside another OMP shell", async () => {
  const requests = await startRecordingServer("omp-nested");
  process.env.OMPCODE = "1";
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./omp/herdr-agent-state.ts");
  install(pi);

  // OMP sets OMPCODE on every shell it spawns. A nested `omp` inherits it and
  // must not claim the pane's session for its short-lived conversation.
  expect(handlers.size).toBe(0);
  await handlers.get("session_start")?.(
    { reason: "startup" },
    {
      hasUI: true,
      isIdle: () => true,
      sessionManager: {
        getSessionFile: () => "/tmp/omp-nested.jsonl",
        getSessionId: () => "omp-nested",
      },
    },
  );
  await Bun.sleep(25);

  expect(requests).toEqual([]);
});

test("OMP accepts POSIX and Windows session paths", async () => {
  const { isAbsoluteSessionPath } = await importFresh("./omp/herdr-agent-state.ts");

  expect(isAbsoluteSessionPath("/tmp/omp-session.jsonl")).toBe(true);
  expect(isAbsoluteSessionPath("C:\\Users\\User\\.omp\\agent\\sessions\\omp-session.jsonl")).toBe(
    true,
  );
  expect(isAbsoluteSessionPath("C:/Users/User/.omp/agent/sessions/omp-session.jsonl")).toBe(true);
  expect(isAbsoluteSessionPath("relative/omp-session.jsonl")).toBe(false);
});

test("Pi reports a Windows session path", async () => {
  const requests = await startRecordingServer("pi-windows-session-path");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const sessionPath = "C:\\Users\\User\\.pi\\agent\\sessions\\pi-session.jsonl";
  await handlers.get("session_start")?.(
    { reason: "startup" },
    {
      ...piContext(() => true),
      sessionManager: {
        getSessionFile: () => sessionPath,
        getSessionId: () => "pi-session",
      },
    },
  );
  await waitFor(() => requests.length === 2);

  expect(requests.map(requestSessionPath)).toEqual([sessionPath, sessionPath]);
});

test("Pi reports idle only after the agent settles", async () => {
  const requests = await startRecordingServer("pi-settled");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  expect(completionHandlers(handlers)).toEqual(["agent_settled"]);
  let idle = true;
  const context = piContext(() => idle);
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => requestStates(requests).length === 1);

  idle = false;
  handlers.get("agent_start")?.({}, context);
  await waitFor(() => requestStates(requests).length === 2);
  expect(requestStates(requests)).toEqual(["idle", "working"]);
  expect(handlers.has("agent_end")).toBe(false);

  const requestCountBeforeStaleSettlement = requests.length;
  handlers.get("agent_settled")?.({}, context);
  await Bun.sleep(25);
  expect(requests).toHaveLength(requestCountBeforeStaleSettlement);
  expect(requestStates(requests)).toEqual(["idle", "working"]);

  idle = true;
  handlers.get("agent_settled")?.({}, context);
  await waitFor(() => requestStates(requests).length === 3);
  expect(requestStates(requests)).toEqual(["idle", "working", "idle"]);
});

test("Pi ignores RPC sessions even when UI APIs are available", async () => {
  const requests = await startRecordingServer("pi-rpc");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const context = {
    ...piContext(() => true),
    hasUI: true,
    mode: "rpc",
  };
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  handlers.get("agent_start")?.({}, context);
  handlers.get("agent_settled")?.({}, context);
  await Bun.sleep(25);

  expect(requests).toEqual([]);
});

test("Pi settlement preserves explicit blocked-state precedence", async () => {
  const requests = await startRecordingServer("pi-settled-blocked");
  const { eventHandlers, handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  let idle = true;
  const context = piContext(() => idle);
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => requestStates(requests).length === 1);
  idle = false;
  handlers.get("agent_start")?.({}, context);
  await waitFor(() => requestStates(requests).length === 2);
  eventHandlers.get("herdr:blocked")?.({ active: true, label: "approval" }, context);
  await waitFor(() => requestStates(requests).length === 3);

  idle = true;
  handlers.get("agent_settled")?.({}, context);
  await Bun.sleep(25);
  expect(requestStates(requests)).toEqual(["idle", "working", "blocked"]);

  eventHandlers.get("herdr:blocked")?.({ active: false }, context);
  await waitFor(() => requestStates(requests).length === 4);
  expect(requestStates(requests)).toEqual(["idle", "working", "blocked", "idle"]);
});

test("Pi reports the session replacement source", async () => {
  const requests = await startRecordingServer("pi-session-source");
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const sessionStart = handlers.get("session_start");
  expect(sessionStart).toBeDefined();
  await sessionStart?.(
    { reason: "new" },
    {
      hasUI: true,
      mode: "tui",
      isIdle: () => true,
      sessionManager: {
        getSessionFile: () => "/tmp/pi-new.jsonl",
        getSessionId: () => "pi-new",
      },
    },
  );

  const reportedSession = () =>
    requests.find((request) => isRecord(request) && request.method === "pane.report_agent_session");
  const deadline = Date.now() + 1_000;
  while (Date.now() < deadline && reportedSession() === undefined) {
    await Bun.sleep(5);
  }

  const request = reportedSession();
  expect(request).toBeDefined();
  expect(isRecord(request) && isRecord(request.params) ? request.params.session_start_source : null)
    .toBe("new");
});

test("Pi waits for a replacement session report before publishing state", async () => {
  const recordingSocketPath = join(tmpdir(), `herdr-pi-session-order-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });

  const requests: unknown[] = [];
  let acknowledgeSessionReport: (() => void) | undefined;
  const recordingServer = createServer((socket) => {
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline === -1) {
        return;
      }
      const request = JSON.parse(input.slice(0, newline));
      requests.push(request);
      if (isRecord(request) && request.method === "pane.report_agent_session") {
        acknowledgeSessionReport = () => socket.end("{}\n");
        return;
      }
      socket.end("{}\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(originalPlatform === "win32" ? `\\\\.\\pipe\\${recordingSocketPath}` : recordingSocketPath, resolve);
  });

  configureIntegrationEnvironment(recordingSocketPath);
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const sessionStart = handlers.get("session_start");
  expect(sessionStart).toBeDefined();
  const sessionStartResult = sessionStart?.(
    { reason: "new" },
    {
      hasUI: true,
      mode: "tui",
      isIdle: () => false,
      sessionManager: {
        getSessionFile: () => "/tmp/pi-new.jsonl",
        getSessionId: () => "pi-new",
      },
    },
  );

  const deadline = Date.now() + 1_000;
  while (Date.now() < deadline && acknowledgeSessionReport === undefined) {
    await Bun.sleep(5);
  }
  expect(acknowledgeSessionReport).toBeDefined();
  expect(
    requests.some((request) => isRecord(request) && request.method === "pane.report_agent"),
  ).toBe(false);

  acknowledgeSessionReport?.();
  await sessionStartResult;

  const stateDeadline = Date.now() + 1_000;
  while (
    Date.now() < stateDeadline &&
    !requests.some((request) => isRecord(request) && request.method === "pane.report_agent")
  ) {
    await Bun.sleep(5);
  }
  expect(requests.map((request) => (isRecord(request) ? request.method : undefined))).toEqual([
    "pane.report_agent_session",
    "pane.report_agent",
  ]);
});

async function startDroppedFirstResponseServer(name: string) {
  const recordingSocketPath = join(tmpdir(), `herdr-${name}-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });

  let connectionCount = 0;
  const attemptedRequests: unknown[] = [];
  const deliveredRequests: unknown[] = [];
  const recordingServer = createServer((socket) => {
    connectionCount += 1;
    const connectionNumber = connectionCount;
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline === -1) {
        return;
      }
      const request = JSON.parse(input.slice(0, newline));
      attemptedRequests.push(request);
      if (connectionNumber === 1) {
        return;
      }
      deliveredRequests.push(request);
      socket.end("{}\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(originalPlatform === "win32" ? `\\\\.\\pipe\\${recordingSocketPath}` : recordingSocketPath, resolve);
  });

  configureIntegrationEnvironment(recordingSocketPath);
  return {
    attemptedRequests,
    deliveredRequests,
    connectionCount: () => connectionCount,
  };
}

test("Oh My Pi retries working before a queued idle state", async () => {
  const { attemptedRequests } = await startDroppedFirstResponseServer("omp-retry");
  process.env.HERDR_OMP_IDLE_DEBOUNCE_MS = "0";
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./omp/herdr-agent-state.ts");
  install(pi);

  const context = {
    hasUI: true,
    isIdle: () => false,
    sessionManager: {
      getSessionFile: () => undefined,
      getSessionId: () => undefined,
    },
  };
  handlers.get("session_start")?.({ reason: "startup" }, context);
  handlers.get("agent_end")?.({ messages: [] }, context);

  const deadline = Date.now() + 2_500;
  while (Date.now() < deadline && attemptedRequests.length < 3) {
    await Bun.sleep(5);
  }

  expect(attemptedRequests).toHaveLength(3);
  expect(attemptedRequests[1]).toEqual(attemptedRequests[0]);
  expect(requestState(attemptedRequests[0])).toBe("working");
  expect(requestState(attemptedRequests[2])).toBe("idle");
});

test("Oh My Pi keeps working when a turn ends with a scheduled continuation", async () => {
  const requests = await startRecordingServer("omp-will-continue");
  process.env.HERDR_OMP_IDLE_DEBOUNCE_MS = "0";
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./omp/herdr-agent-state.ts");
  install(pi);

  let idle = true;
  const context = {
    hasUI: true,
    isIdle: () => idle,
    sessionManager: {
      getSessionFile: () => undefined,
      getSessionId: () => undefined,
    },
  };

  handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => requestStates(requests).length === 1);

  idle = false;
  handlers.get("agent_start")?.({}, context);
  await waitFor(() => requestStates(requests).length === 2);
  expect(requestStates(requests)).toEqual(["idle", "working"]);

  // OMP already scheduled an automatic continuation, so this loop end is not a
  // user-visible settle and must not publish idle. See issue #2851.
  handlers.get("agent_end")?.({ messages: [], willContinue: true }, context);
  await Bun.sleep(50);
  expect(requestStates(requests)).toEqual(["idle", "working"]);

  // The real terminal end still settles the pane.
  idle = true;
  handlers.get("agent_end")?.({ messages: [] }, context);
  await waitFor(() => requestStates(requests).length === 3);
  expect(requestStates(requests)).toEqual(["idle", "working", "idle"]);
});

test("Pi retries working state after an unanswered socket attempt", async () => {
  const { attemptedRequests, deliveredRequests, connectionCount } =
    await startDroppedFirstResponseServer("pi-retry");
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const sessionStart = handlers.get("session_start");
  expect(sessionStart).toBeDefined();
  await sessionStart?.(
    { reason: "startup" },
    {
      hasUI: true,
      mode: "tui",
      isIdle: () => false,
      sessionManager: {
        getSessionFile: () => undefined,
        getSessionId: () => undefined,
      },
    },
  );

  const reportedWorking = () =>
    deliveredRequests.some((request) => {
      if (!isRecord(request) || request.method !== "pane.report_agent") {
        return false;
      }
      const params = request.params;
      return isRecord(params) && params.state === "working";
    });

  const deadline = Date.now() + 2_500;
  while (Date.now() < deadline && !reportedWorking()) {
    await Bun.sleep(5);
  }

  expect(connectionCount()).toBeGreaterThanOrEqual(2);
  expect(attemptedRequests.length).toBeGreaterThanOrEqual(2);
  expect(attemptedRequests[1]).toEqual(attemptedRequests[0]);
  expect(reportedWorking()).toBe(true);
});

function completionHandlers(handlers: Map<string, Handler>): string[] {
  return ["agent_end", "agent_settled"].filter((event) => handlers.has(event));
}

function piContext(isIdle: () => boolean) {
  return {
    hasUI: true,
    mode: "tui",
    isIdle,
    sessionManager: {
      getSessionFile: () => undefined,
      getSessionId: () => undefined,
    },
  };
}

function requestStates(requests: unknown[]): unknown[] {
  return requests
    .filter((request) => isRecord(request) && request.method === "pane.report_agent")
    .map(requestState);
}

async function waitFor(predicate: () => boolean, timeoutMs = 1_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline && !predicate()) {
    await Bun.sleep(5);
  }
  expect(predicate()).toBe(true);
}

function requestState(request: unknown): unknown {
  if (!isRecord(request) || !isRecord(request.params)) {
    return undefined;
  }
  return request.params.state;
}

function requestSessionPath(request: unknown): unknown {
  if (!isRecord(request) || !isRecord(request.params)) {
    return undefined;
  }
  return request.params.agent_session_path;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function requestsFor(requests: unknown[], method: string): Record<string, unknown>[] {
  return requests
    .filter((request) => isRecord(request) && request.method === method)
    .map((request) => (request as Record<string, unknown>).params as Record<string, unknown>);
}

function usageContext(
  usage: unknown,
  promptCache?: Record<string, number>,
  isIdle: () => boolean = () => true,
) {
  return {
    ...piContext(isIdle),
    getContextUsage: () => usage,
    model: promptCache ? { promptCache } : {},
  };
}

function assistantMessage(usage: Record<string, number>) {
  return { role: "assistant", timestamp: 1000, durationMs: 500, usage };
}

test("Pi reports exact context usage and the prompt cache on turn end", async () => {
  const requests = await startRecordingServer("pi-usage-turn-end");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const context = usageContext(
    { tokens: 84000, contextWindow: 200000, percent: 42 },
    { short: 300, long: 3600 },
  );
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  handlers.get("turn_end")?.(
    { message: assistantMessage({ cacheRead: 10, cacheWrite: 5, cacheWrite1h: 5 }) },
    context,
  );
  await waitFor(
    () =>
      requestsFor(requests, "pane.report_context_usage").length === 1 &&
      requestsFor(requests, "pane.report_prompt_cache").length === 1,
  );

  const [usage] = requestsFor(requests, "pane.report_context_usage");
  expect(usage.pane_id).toBe("test:p1");
  expect(usage.source).toBe("herdr:pi");
  expect(usage.used_tokens).toBe(84000);
  expect(usage.window_tokens).toBe(200000);
  expect(typeof usage.observed_at_ms).toBe("number");
  const [cache] = requestsFor(requests, "pane.report_prompt_cache");
  expect(cache.pane_id).toBe("test:p1");
  expect(cache.source).toBe("herdr:pi");
  // The request start, not the response end (timestamp + durationMs).
  expect(cache.last_request_at_ms).toBe(1000);
  expect(cache.ttl_secs).toBe(3600);
});

test("Pi read-only cache hit reuses the last written tier and skips without one", async () => {
  const requests = await startRecordingServer("pi-cache-tier");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const context = usageContext(undefined, { short: 300, long: 3600 });
  await handlers.get("session_start")?.({ reason: "startup" }, context);

  // A read-only hit before any write has no tier to borrow.
  handlers.get("turn_end")?.({ message: assistantMessage({ cacheRead: 10, cacheWrite: 0 }) }, context);
  await Bun.sleep(50);
  expect(requestsFor(requests, "pane.report_prompt_cache")).toHaveLength(0);

  // A short write, then a read-only hit reuses the short tier.
  handlers.get("turn_end")?.({ message: assistantMessage({ cacheRead: 0, cacheWrite: 7 }) }, context);
  await waitFor(() => requestsFor(requests, "pane.report_prompt_cache").length === 1);
  handlers.get("turn_end")?.({ message: assistantMessage({ cacheRead: 7, cacheWrite: 0 }) }, context);
  await waitFor(() => requestsFor(requests, "pane.report_prompt_cache").length === 2);
  expect(requestsFor(requests, "pane.report_prompt_cache").map((cache) => cache.ttl_secs)).toEqual([
    300, 300,
  ]);

  // A new session forgets the remembered tier.
  await handlers.get("session_start")?.({ reason: "new" }, context);
  handlers.get("turn_end")?.({ message: assistantMessage({ cacheRead: 7, cacheWrite: 0 }) }, context);
  await Bun.sleep(50);
  expect(requestsFor(requests, "pane.report_prompt_cache")).toHaveLength(2);

  // Messages that are not assistant replies or touch no cache report nothing.
  handlers.get("turn_end")?.({ message: { role: "user", usage: { cacheWrite: 5 } } }, context);
  handlers.get("turn_end")?.({ message: assistantMessage({ cacheRead: 0, cacheWrite: 0 }) }, context);
  await Bun.sleep(50);
  expect(requestsFor(requests, "pane.report_prompt_cache")).toHaveLength(2);
});

test("Pi skips the prompt cache when the model has no lifetime for the tier", async () => {
  const requests = await startRecordingServer("pi-cache-no-ttl");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const message = assistantMessage({ cacheRead: 0, cacheWrite: 5, cacheWrite1h: 5 });
  for (const promptCache of [undefined, { short: 300 }, { long: 0 }, { long: 90000 }, { long: 1.5 }]) {
    const context = usageContext(undefined, promptCache as Record<string, number> | undefined);
    await handlers.get("session_start")?.({ reason: "startup" }, context);
    handlers.get("turn_end")?.({ message }, context);
  }
  await Bun.sleep(50);

  expect(requestsFor(requests, "pane.report_prompt_cache")).toHaveLength(0);
});

test("Pi clears context usage when tokens are null after compaction", async () => {
  const requests = await startRecordingServer("pi-context-clear");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const context = usageContext({ tokens: null, contextWindow: 200000, percent: null });
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  handlers.get("turn_end")?.({}, context);
  await waitFor(() => requestsFor(requests, "pane.report_context_usage").length === 1);

  const [usage] = requestsFor(requests, "pane.report_context_usage");
  expect(usage.clear).toBe(true);
  expect(usage.used_tokens).toBeUndefined();
  expect(usage.window_tokens).toBeUndefined();
  expect(usage.observed_at_ms).toBeUndefined();
});

test("Pi omits an unusable window and rejects unusable token counts", async () => {
  const requests = await startRecordingServer("pi-context-shape");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const readings: unknown[] = [
    { tokens: 5000, contextWindow: 0 },
    { tokens: -1, contextWindow: 200000 },
    { tokens: 1.5, contextWindow: 200000 },
    { tokens: "5", contextWindow: 200000 },
    null,
  ];
  let reading: unknown = readings[0];
  const context = { ...piContext(() => true), getContextUsage: () => reading };
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  for (reading of readings) {
    handlers.get("turn_end")?.({}, context);
  }
  await waitFor(() => requestsFor(requests, "pane.report_context_usage").length === 1);
  await Bun.sleep(50);

  const sent = requestsFor(requests, "pane.report_context_usage");
  expect(sent).toHaveLength(1);
  expect(sent[0].used_tokens).toBe(5000);
  expect("window_tokens" in sent[0]).toBe(false);
});

test("Pi reports context usage when the agent settles", async () => {
  const requests = await startRecordingServer("pi-context-settled");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const context = usageContext({ tokens: 1200, contextWindow: 100000, percent: 1 });
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  handlers.get("agent_settled")?.({}, context);
  await waitFor(() => requestsFor(requests, "pane.report_context_usage").length === 1);

  expect(requestsFor(requests, "pane.report_context_usage")[0].used_tokens).toBe(1200);
});

test("Oh My Pi reports exact context usage after a tool and at agent end, and never a prompt cache", async () => {
  const requests = await startRecordingServer("omp-context-usage");
  process.env.HERDR_OMP_IDLE_DEBOUNCE_MS = "0";
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./omp/herdr-agent-state.ts");
  install(pi);

  let idle = true;
  const context = {
    hasUI: true,
    isIdle: () => idle,
    getContextUsage: () => ({ tokens: 42000, contextWindow: 1000000, percent: 4.2 }),
    model: { promptCache: { short: 300, long: 3600 } },
    sessionManager: {
      getSessionFile: () => undefined,
      getSessionId: () => undefined,
    },
  };
  handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => requestStates(requests).length === 1);

  idle = false;
  handlers.get("agent_start")?.({}, context);
  handlers.get("tool_execution_end")?.({ toolName: "read" }, context);
  await waitFor(() => requestsFor(requests, "pane.report_context_usage").length === 1);

  handlers.get("agent_end")?.({ messages: [assistantMessage({ cacheRead: 3, cacheWrite: 4 })] }, context);
  await waitFor(() => requestsFor(requests, "pane.report_context_usage").length === 2);

  for (const usage of requestsFor(requests, "pane.report_context_usage")) {
    expect(usage.source).toBe("herdr:omp");
    expect(usage.used_tokens).toBe(42000);
    expect(usage.window_tokens).toBe(1000000);
  }
  expect(requestsFor(requests, "pane.report_prompt_cache")).toHaveLength(0);
  expect(handlers.has("turn_end")).toBe(false);
});

test("Oh My Pi reports nothing when the extension API has no context usage", async () => {
  const requests = await startRecordingServer("omp-context-missing");
  process.env.HERDR_OMP_IDLE_DEBOUNCE_MS = "0";
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./omp/herdr-agent-state.ts");
  install(pi);

  const context = {
    hasUI: true,
    isIdle: () => true,
    getContextUsage: () => undefined,
    sessionManager: { getSessionFile: () => undefined, getSessionId: () => undefined },
  };
  handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => requestStates(requests).length === 1);
  handlers.get("tool_execution_end")?.({ toolName: "read" }, context);
  await Bun.sleep(50);

  expect(requestsFor(requests, "pane.report_context_usage")).toHaveLength(0);
});

test("context reports are skipped outside herdr", async () => {
  const requests = await startRecordingServer("context-outside-herdr");
  delete process.env.HERDR_ENV;
  const usage = { tokens: 100, contextWindow: 1000, percent: 10 };

  for (const integration of integrations) {
    const { handlers, pi } = createExtensionHarness();
    const { default: install } = await importFresh(integration.modulePath);
    install(pi);
    const context = usageContext(usage, { short: 300, long: 3600 });
    handlers.get("turn_end")?.({ message: assistantMessage({ cacheWrite: 1 }) }, context);
    handlers.get("agent_settled")?.({}, context);
    handlers.get("tool_execution_end")?.({ toolName: "read" }, context);
    handlers.get("agent_end")?.({}, context);
  }
  await Bun.sleep(50);

  expect(requests).toEqual([]);
});

// OpenCode and Kilo report the context fact for each finished assistant
// message, with the window taken from the plugin client's provider catalog.

function providerCatalogClient(contextLimit: number | undefined, calls: { count: number }) {
  return {
    config: {
      async providers() {
        calls.count += 1;
        return {
          data: {
            providers: [
              {
                id: "anthropic",
                models: {
                  "claude-opus": contextLimit === undefined ? {} : { limit: { context: contextLimit, output: 32000 } },
                },
              },
            ],
          },
        };
      },
    },
  };
}

function finishedAssistantMessage(overrides: Record<string, unknown> = {}) {
  return {
    id: "message-1",
    sessionID: "root-session",
    role: "assistant",
    providerID: "anthropic",
    modelID: "claude-opus",
    time: { created: 1_700_000_000_000, completed: 1_700_000_005_000 },
    tokens: { input: 4000, output: 900, reasoning: 0, cache: { read: 80000, write: 0 } },
    ...overrides,
  };
}

function messageUpdated(info: Record<string, unknown>) {
  return { event: { type: "message.updated", properties: { info } } };
}

async function startContextPlugin(
  socketPlugin: (typeof socketPlugins)[number],
  name: string,
  client: unknown,
) {
  const requests = await startRecordingServer(name);
  process.argv = ["bun", "/$bunfs/root/src/index.js", "run"];
  const { HerdrAgentStatePlugin } = await importFresh(socketPlugin.modulePath);
  const plugin = await HerdrAgentStatePlugin({ client });
  return { requests, plugin };
}

for (const socketPlugin of socketPlugins) {
  const slug = socketPlugin.name.toLowerCase();

  test(`${socketPlugin.name} reports exact context usage for a finished assistant message`, async () => {
    const calls = { count: 0 };
    const { requests, plugin } = await startContextPlugin(
      socketPlugin,
      `${slug}-context-exact`,
      providerCatalogClient(200000, calls),
    );

    await plugin.event(messageUpdated(finishedAssistantMessage()));
    await waitFor(() => requestsFor(requests, "pane.report_context_usage").length === 1);

    const [usage] = requestsFor(requests, "pane.report_context_usage");
    expect(usage.pane_id).toBe("test:p1");
    expect(usage.source).toBe(`herdr:${slug}`);
    expect(usage.used_tokens).toBe(84000);
    expect(usage.window_tokens).toBe(200000);
    expect(usage.observed_at_ms).toBe(1_700_000_005_000);
    expect(requestsFor(requests, "pane.report_prompt_cache")).toHaveLength(0);
  });

  test(`${socketPlugin.name} sums input, cache read and cache write`, async () => {
    const { requests, plugin } = await startContextPlugin(
      socketPlugin,
      `${slug}-context-sum`,
      providerCatalogClient(200000, { count: 0 }),
    );

    await plugin.event(
      messageUpdated(
        finishedAssistantMessage({ tokens: { input: 10, output: 5, reasoning: 0, cache: { read: 200, write: 3000 } } }),
      ),
    );
    await waitFor(() => requestsFor(requests, "pane.report_context_usage").length === 1);

    expect(requestsFor(requests, "pane.report_context_usage")[0].used_tokens).toBe(3210);
  });

  test(`${socketPlugin.name} skips streaming and user messages`, async () => {
    const { requests, plugin } = await startContextPlugin(
      socketPlugin,
      `${slug}-context-skips`,
      providerCatalogClient(200000, { count: 0 }),
    );

    await plugin.event(messageUpdated(finishedAssistantMessage({ id: "streaming", time: { created: 1 } })));
    await plugin.event(messageUpdated(finishedAssistantMessage({ id: "user-turn", role: "user" })));
    await plugin.event(messageUpdated(finishedAssistantMessage({ id: "empty", tokens: undefined })));
    await Bun.sleep(50);

    expect(requestsFor(requests, "pane.report_context_usage")).toHaveLength(0);
  });

  test(`${socketPlugin.name} reports tokens only when the provider lookup fails`, async () => {
    const failing = {
      config: {
        async providers() {
          throw new Error("catalog unavailable");
        },
      },
    };
    const { requests, plugin } = await startContextPlugin(socketPlugin, `${slug}-context-lookup-fails`, failing);

    await plugin.event(messageUpdated(finishedAssistantMessage()));
    await waitFor(() => requestsFor(requests, "pane.report_context_usage").length === 1);

    const [usage] = requestsFor(requests, "pane.report_context_usage");
    expect(usage.used_tokens).toBe(84000);
    expect("window_tokens" in usage).toBe(false);
  });

  test(`${socketPlugin.name} reports tokens only when the model has no catalog limit`, async () => {
    const { requests, plugin } = await startContextPlugin(
      socketPlugin,
      `${slug}-context-no-limit`,
      providerCatalogClient(undefined, { count: 0 }),
    );

    await plugin.event(messageUpdated(finishedAssistantMessage()));
    await waitFor(() => requestsFor(requests, "pane.report_context_usage").length === 1);

    expect("window_tokens" in requestsFor(requests, "pane.report_context_usage")[0]).toBe(false);
  });

  test(`${socketPlugin.name} looks a model up once`, async () => {
    const calls = { count: 0 };
    const { requests, plugin } = await startContextPlugin(
      socketPlugin,
      `${slug}-context-lookup-once`,
      providerCatalogClient(200000, calls),
    );

    await Promise.all([
      plugin.event(messageUpdated(finishedAssistantMessage({ id: "message-1" }))),
      plugin.event(messageUpdated(finishedAssistantMessage({ id: "message-2" }))),
    ]);
    await waitFor(() => requestsFor(requests, "pane.report_context_usage").length === 2);
    await plugin.event(messageUpdated(finishedAssistantMessage({ id: "message-3" })));
    await waitFor(() => requestsFor(requests, "pane.report_context_usage").length === 3);

    expect(calls.count).toBe(1);
  });

  test(`${socketPlugin.name} reports each finished message once`, async () => {
    const { requests, plugin } = await startContextPlugin(
      socketPlugin,
      `${slug}-context-once`,
      providerCatalogClient(200000, { count: 0 }),
    );

    await plugin.event(messageUpdated(finishedAssistantMessage()));
    await plugin.event(messageUpdated(finishedAssistantMessage()));
    await waitFor(() => requestsFor(requests, "pane.report_context_usage").length === 1);
    await Bun.sleep(50);

    expect(requestsFor(requests, "pane.report_context_usage")).toHaveLength(1);
  });

  test(`${socketPlugin.name} does not delay state reports behind the provider lookup`, async () => {
    let releaseCatalog: () => void = () => {};
    const slowCatalog = {
      config: {
        providers: () =>
          new Promise((resolve) => {
            releaseCatalog = () => resolve({ data: { providers: [] } });
          }),
      },
    };
    const { requests, plugin } = await startContextPlugin(socketPlugin, `${slug}-context-slow-catalog`, slowCatalog);

    await plugin.event(messageUpdated(finishedAssistantMessage()));
    await plugin.event({
      event: { type: "session.idle", properties: { sessionID: "root-session" } },
    });
    await waitFor(() => requestsFor(requests, "pane.report_agent").length === 1);
    expect(requestsFor(requests, "pane.report_context_usage")).toHaveLength(0);

    releaseCatalog();
    await waitFor(() => requestsFor(requests, "pane.report_context_usage").length === 1);
  });

  test(`${socketPlugin.name} reports tokens only without a client and stays disabled outside herdr`, async () => {
    const requests = await startRecordingServer(`${slug}-context-no-client`);
    process.argv = ["bun", "/$bunfs/root/src/index.js", "run"];
    const { HerdrAgentStatePlugin } = await importFresh(socketPlugin.modulePath);
    const plugin = await HerdrAgentStatePlugin();

    await plugin.event(messageUpdated(finishedAssistantMessage()));
    await waitFor(() => requestsFor(requests, "pane.report_context_usage").length === 1);
    expect("window_tokens" in requestsFor(requests, "pane.report_context_usage")[0]).toBe(false);

    delete process.env.HERDR_ENV;
    expect(await HerdrAgentStatePlugin({ client: providerCatalogClient(1, { count: 0 }) })).toEqual({});
  });
}

for (const socketPlugin of socketPlugins) {
  test(`${socketPlugin.name} skips child-session messages`, async () => {
    const { requests, plugin } = await startContextPlugin(
      socketPlugin,
      `${socketPlugin.name.toLowerCase()}-context-child`,
      providerCatalogClient(200000, { count: 0 }),
    );

    await plugin.event({
      event: {
        type: "session.created",
        properties: { sessionID: "child-session", info: { id: "child-session", parentID: "root-session" } },
      },
    });
    await plugin.event(messageUpdated(finishedAssistantMessage({ id: "child-message", sessionID: "child-session" })));
    await plugin.event(messageUpdated(finishedAssistantMessage({ id: "root-message" })));
    await waitFor(() => requestsFor(requests, "pane.report_context_usage").length === 1);
    await Bun.sleep(50);

    const usages = requestsFor(requests, "pane.report_context_usage");
    expect(usages).toHaveLength(1);
    expect(usages[0].used_tokens).toBe(84000);
  });
}
