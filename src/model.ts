export interface SessionRow {
  sessionId: string;
  cwd: string;
  title: string;
  updatedAt: string;
}

export interface ConfigChoice {
  value: string;
  name: string;
}

export interface ConfigOption {
  id: string;
  name: string;
  type: string;
  currentValue: string;
  choices: ConfigChoice[];
}

export interface ModeChoice {
  id: string;
  label: string;
}

export interface Usage {
  used: number;
  size: number;
  inputTokens?: number;
  outputTokens?: number;
  totalTokens?: number;
  cachedReadTokens?: number;
  reasoningTokens?: number;
}

export interface PlanEntry {
  content: string;
  status: string;
  priority: string;
}

export interface ToolPayload {
  toolCallId?: string;
  title?: string;
  name?: string;
  kind?: string;
  status?: string;
  content?: unknown[];
  locations?: { path?: string; line?: number }[];
  rawInput?: unknown;
  rawOutput?: unknown;
}

export interface PermissionOption {
  optionId: string;
  name: string;
  kind: string;
}

export interface PermissionRequest {
  id: number;
  tool: ToolPayload;
  options: PermissionOption[];
}

export type TranscriptItem =
  | {
      id: string;
      kind: "text";
      role: "user" | "assistant" | "thought";
      text: string;
      messageId?: string;
      images?: { id: string; name: string; preview: string }[];
    }
  | { id: string; kind: "tool"; toolId: string; tool: ToolPayload }
  | { id: string; kind: "plan"; entries: PlanEntry[] }
  | { id: string; kind: "notice"; title: string; body: string }
  | { id: string; kind: "compaction"; text: string }
  | { id: string; kind: "unknown"; text: string };

export interface DeskState {
  ready: boolean;
  agentName: string;
  agentVersion: string;
  model: string;
  contextTokens: number;
  sessions: SessionRow[];
  sessionId: string | null;
  ignoreSessionId: string | null;
  warning: string;
  items: TranscriptItem[];
  busy: boolean;
  loading: boolean;
  error: string;
  auth: boolean;
  modeId: string;
  modeConfirmed: boolean;
  advertisedModes: { id: string; name: string }[];
  configOptions: ConfigOption[];
  usage: Usage | null;
  permission: PermissionRequest | null;
  commands: { name: string; description: string }[];
  banner: string;
  promptImage: boolean;
}

export const initialDesk: DeskState = {
  ready: false,
  agentName: "",
  agentVersion: "",
  model: "",
  contextTokens: 0,
  sessions: [],
  sessionId: null,
  ignoreSessionId: null,
  warning: "",
  items: [],
  busy: false,
  loading: false,
  error: "",
  auth: false,
  modeId: "ask",
  modeConfirmed: false,
  advertisedModes: [],
  configOptions: [],
  usage: null,
  permission: null,
  commands: [],
  banner: "",
  promptImage: false,
};

const MODE_LABELS: Record<string, string> = {
  ask: "確認する",
  acceptEdits: "編集は自動",
  plan: "プラン",
  auto: "自動レビュー",
  bypassPermissions: "常時承認",
};

const FALLBACK_MODES = ["ask", "acceptEdits", "plan", "bypassPermissions"];

export function modeChoices(state: DeskState): ModeChoice[] {
  const advertised = state.advertisedModes.map((mode) => mode.id);
  const ids = advertised.length
    ? advertised.filter((id) => id !== "auto" || advertised.includes("auto"))
    : [...FALLBACK_MODES, ...(state.modeId === "auto" ? ["auto"] : [])];
  const unique = [...new Set(ids)];
  return unique.map((id) => ({
    id,
    label: MODE_LABELS[id] ?? state.advertisedModes.find((mode) => mode.id === id)?.name ?? id,
  }));
}

export function mergeSnapshot(state: DeskState, snapshot: Record<string, unknown>): DeskState {
  let next = state;
  const apply = (key: string, missing: boolean) => {
    const event = snapshot[key];
    if (missing && event && typeof event === "object") {
      next = reduceDesk(next, event as Record<string, unknown>);
    }
  };
  apply("ready", !next.ready);
  apply("sessions", next.sessions.length === 0);
  apply("session", !next.sessionId);
  apply("config", next.configOptions.length === 0);
  apply("mode", !next.modeConfirmed);
  apply("usage", !next.usage);
  return next;
}

export function reduceDesk(state: DeskState, event: Record<string, unknown>): DeskState {
  const type = String(event.type ?? "");
  const sessionId = typeof event.sessionId === "string" ? event.sessionId : "";
  if (sessionId && !acceptsSession(state, sessionId) && type !== "session" && type !== "session-loading") {
    return state;
  }
  switch (type) {
    case "ready":
      return {
        ...state,
        ready: true,
        agentName: text(event.name),
        agentVersion: text(event.version),
        model: text(event.model) || state.model,
        contextTokens: number(event.contextTokens) || state.contextTokens,
        auth: false,
        promptImage: Boolean(event.promptImage),
      };
    case "error":
      return {
        ...state,
        error: text(event.message),
        auth: Boolean(event.auth),
        busy: false,
        loading: false,
      };
    case "busy":
      return { ...state, busy: Boolean(event.busy) };
    case "session-loading":
      return {
        ...state,
        items: [],
        permission: null,
        warning: "",
        banner: "",
        loading: true,
        error: "",
        ignoreSessionId: state.sessionId,
        sessionId: typeof event.id === "string" ? event.id : null,
      };
    case "session":
      return {
        ...state,
        loading: false,
        sessionId: text(event.id),
        ignoreSessionId: null,
        warning: text(event.warning),
        modeId: text(event.modeId) || state.modeId,
        modeConfirmed: Boolean(text(event.modeId)),
        advertisedModes: parseModes(event.modes),
        configOptions: parseOptions(event.configOptions),
      };
    case "session-closed":
      return {
        ...state,
        sessionId: null,
        items: [],
        permission: null,
        warning: "",
        loading: false,
        busy: false,
      };
    case "sessions":
      return { ...state, sessions: parseSessions(event.sessions) };
    case "chunk":
      return appendChunk(state, event);
    case "tool":
      return upsertTool(state, event.tool);
    case "plan":
      return upsertPlan(state, event.entries);
    case "notice":
      return {
        ...state,
        banner: text(event.title),
        items: [
          ...state.items,
          {
            id: nextId(state, "notice"),
            kind: "notice",
            title: text(event.title),
            body: text(event.description),
          },
        ],
      };
    case "compaction":
      return {
        ...state,
        items: [
          ...state.items,
          {
            id: nextId(state, "compaction"),
            kind: "compaction",
            text: "Grok が会話を圧縮しました。",
          },
        ],
      };
    case "unknown":
      return {
        ...state,
        items: [
          ...state.items,
          {
            id: nextId(state, "unknown"),
            kind: "unknown",
            text: JSON.stringify(event.update ?? event, null, 2),
          },
        ],
      };
    case "mode":
      return { ...state, modeId: text(event.modeId) || state.modeId, modeConfirmed: true };
    case "config":
      return { ...state, configOptions: parseOptions(event.options) };
    case "title": {
      const title = text(event.title);
      return {
        ...state,
        sessions: state.sessions.map((session) =>
          session.sessionId === (sessionId || state.sessionId) ? { ...session, title } : session,
        ),
      };
    }
    case "usage":
      return { ...state, usage: parseUsage(event, state.usage) };
    case "permission":
      return { ...state, permission: parsePermission(event) };
    case "permission-clear":
      return state.permission?.id === number(event.id) ? { ...state, permission: null } : state;
    case "turn":
      return { ...state, busy: false, loading: false };
    case "commands":
      return { ...state, commands: parseCommands(event.commands) };
    default:
      return state;
  }
}

function acceptsSession(state: DeskState, sessionId: string) {
  if (state.sessionId) return state.sessionId === sessionId;
  return sessionId !== state.ignoreSessionId;
}

function appendChunk(state: DeskState, event: Record<string, unknown>): DeskState {
  const role = text(event.role);
  if (role !== "user" && role !== "assistant" && role !== "thought") return state;
  if (role === "user" && !event.replay) return state;
  const piece = text(event.text);
  if (!piece) return state;
  const messageId = text(event.messageId);
  const items = [...state.items];
  const last = items[items.length - 1];
  if (
    last &&
    last.kind === "text" &&
    last.role === role &&
    (last.messageId ?? "") === messageId
  ) {
    items[items.length - 1] = { ...last, text: last.text + piece };
    return { ...state, items };
  }
  items.push({
    id: nextId(state, role),
    kind: "text",
    role,
    text: piece,
    messageId: messageId || undefined,
  });
  return { ...state, items };
}

function upsertTool(state: DeskState, raw: unknown): DeskState {
  if (!raw || typeof raw !== "object") return state;
  const tool = raw as ToolPayload;
  const toolId = String(tool.toolCallId ?? "");
  if (!toolId) {
    return {
      ...state,
      items: [...state.items, { id: nextId(state, "tool"), kind: "tool", toolId: nextId(state, "loose"), tool }],
    };
  }
  const index = state.items.findIndex((item) => item.kind === "tool" && item.toolId === toolId);
  if (index < 0) {
    return {
      ...state,
      items: [...state.items, { id: nextId(state, "tool"), kind: "tool", toolId, tool }],
    };
  }
  const items = [...state.items];
  const current = items[index];
  if (current.kind !== "tool") return state;
  items[index] = { ...current, tool: mergeTool(current.tool, tool) };
  return { ...state, items };
}

function mergeTool(prev: ToolPayload, next: ToolPayload): ToolPayload {
  const merged: ToolPayload = { ...prev };
  for (const [key, value] of Object.entries(next) as [keyof ToolPayload, unknown][]) {
    if (value !== undefined && value !== null) {
      merged[key] = value as never;
    }
  }
  return merged;
}

function upsertPlan(state: DeskState, raw: unknown): DeskState {
  const entries = Array.isArray(raw)
    ? raw.map((entry) => {
        const row = entry as Record<string, unknown>;
        return {
          content: text(row.content),
          status: text(row.status),
          priority: text(row.priority),
        };
      })
    : [];
  const index = state.items.findIndex((item) => item.kind === "plan");
  if (index < 0) {
    return { ...state, items: [...state.items, { id: nextId(state, "plan"), kind: "plan", entries }] };
  }
  const items = [...state.items];
  items[index] = { id: items[index].id, kind: "plan", entries };
  return { ...state, items };
}

function parseSessions(raw: unknown): SessionRow[] {
  if (!Array.isArray(raw)) return [];
  return raw
    .map((item) => {
      const row = item as Record<string, unknown>;
      return {
        sessionId: text(row.sessionId),
        cwd: text(row.cwd),
        title: text(row.title),
        updatedAt: text(row.updatedAt),
      };
    })
    .filter((session) => session.sessionId);
}

function parseModes(raw: unknown): { id: string; name: string }[] {
  if (!Array.isArray(raw)) return [];
  return raw
    .map((item) => {
      const row = item as Record<string, unknown>;
      return { id: text(row.id), name: text(row.name) };
    })
    .filter((mode) => mode.id);
}

function parseOptions(raw: unknown): ConfigOption[] {
  if (!Array.isArray(raw)) return [];
  return raw
    .map((item) => {
      const row = item as Record<string, unknown>;
      return {
        id: text(row.id),
        name: text(row.name) || text(row.id),
        type: text(row.type),
        currentValue: text(row.currentValue),
        choices: flattenChoices(row.options),
      };
    })
    .filter((option) => option.id);
}

function flattenChoices(raw: unknown): ConfigChoice[] {
  if (!Array.isArray(raw)) return [];
  const choices: ConfigChoice[] = [];
  for (const item of raw) {
    if (!item || typeof item !== "object") continue;
    const row = item as Record<string, unknown>;
    if (typeof row.value === "string") {
      choices.push({ value: row.value, name: text(row.name) || row.value });
      continue;
    }
    if (Array.isArray(row.options)) choices.push(...flattenChoices(row.options));
  }
  return choices;
}

function parseCommands(raw: unknown): { name: string; description: string }[] {
  if (!Array.isArray(raw)) return [];
  return raw
    .map((item) => {
      const row = item as Record<string, unknown>;
      return { name: text(row.name), description: text(row.description) };
    })
    .filter((command) => command.name);
}

function parseUsage(event: Record<string, unknown>, previous: Usage | null): Usage {
  const input = optionalNumber(event.inputTokens);
  const size = number(event.size) || previous?.size || 0;
  const used = input ?? number(event.used);
  return {
    used,
    size,
    inputTokens: input ?? previous?.inputTokens,
    outputTokens: optionalNumber(event.outputTokens) ?? previous?.outputTokens,
    totalTokens: optionalNumber(event.totalTokens) ?? previous?.totalTokens,
    cachedReadTokens: optionalNumber(event.cachedReadTokens) ?? previous?.cachedReadTokens,
    reasoningTokens: optionalNumber(event.reasoningTokens) ?? previous?.reasoningTokens,
  };
}

function parsePermission(event: Record<string, unknown>): PermissionRequest {
  const tool = (event.tool ?? {}) as ToolPayload;
  const options = Array.isArray(event.options)
    ? event.options.map((item) => {
        const row = item as Record<string, unknown>;
        return { optionId: text(row.optionId), name: text(row.name), kind: text(row.kind) };
      })
    : [];
  return { id: number(event.id), tool, options };
}

function nextId(state: DeskState, prefix: string) {
  return `${prefix}-${state.items.length}-${Date.now()}`;
}

function text(value: unknown) {
  return typeof value === "string" ? value : "";
}

function number(value: unknown) {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function optionalNumber(value: unknown) {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

export function freePercent(usage: Usage | null, fallbackSize: number) {
  const size = usage?.size || fallbackSize;
  if (!usage || size <= 0) return null;
  const used = usage.inputTokens ?? usage.used;
  const free = Math.max(0, size - used);
  return { used, size, free, percent: Math.round((free * 100) / size) };
}

export function toolLooksLikePlan(tool: ToolPayload) {
  const blob = `${tool.title ?? ""} ${tool.name ?? ""} ${tool.kind ?? ""}`.toLowerCase();
  return blob.includes("plan") || blob.includes("プラン");
}

export function toolLabel(tool: ToolPayload) {
  const kind = tool.kind || tool.name || "tool";
  const labels: Record<string, string> = {
    read: "読み取り",
    edit: "編集",
    delete: "削除",
    move: "移動",
    search: "検索",
    execute: "実行",
    think: "思考",
    fetch: "取得",
    other: "ツール",
  };
  return labels[kind] ?? kind;
}

export function statusLabel(status: string | undefined) {
  const labels: Record<string, string> = {
    pending: "待機",
    in_progress: "実行中",
    inProgress: "実行中",
    completed: "完了",
    failed: "失敗",
  };
  return labels[status ?? ""] ?? (status || "待機");
}

export interface DiffLine {
  kind: "same" | "add" | "del";
  text: string;
}

export function lineDiff(oldText: string, newText: string): DiffLine[] {
  const oldLines = oldText.split("\n");
  const newLines = newText.split("\n");
  if (oldLines.length > 400 || newLines.length > 400) {
    return newLines.slice(0, 400).map((text) => ({ kind: "add", text }));
  }
  const dp: number[][] = Array.from({ length: oldLines.length + 1 }, () =>
    Array(newLines.length + 1).fill(0),
  );
  for (let i = oldLines.length - 1; i >= 0; i -= 1) {
    for (let j = newLines.length - 1; j >= 0; j -= 1) {
      dp[i][j] = oldLines[i] === newLines[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }
  const lines: DiffLine[] = [];
  let i = 0;
  let j = 0;
  while (i < oldLines.length && j < newLines.length) {
    if (oldLines[i] === newLines[j]) {
      lines.push({ kind: "same", text: oldLines[i] });
      i += 1;
      j += 1;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      lines.push({ kind: "del", text: oldLines[i] });
      i += 1;
    } else {
      lines.push({ kind: "add", text: newLines[j] });
      j += 1;
    }
  }
  while (i < oldLines.length) {
    lines.push({ kind: "del", text: oldLines[i] });
    i += 1;
  }
  while (j < newLines.length) {
    lines.push({ kind: "add", text: newLines[j] });
    j += 1;
  }
  return lines;
}
