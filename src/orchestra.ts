export type TaskStatus = "running" | "waiting" | "paused" | "completed" | "error" | "aborted";
export type NodeStatus = "running" | "succeeded" | "failed" | "killed" | "timedOut";
export type NextAction = "dispatch" | "complete" | "askHuman";
export type LogStream = "stdout" | "stderr" | "system";
export type AgentKind = "claude" | "codex" | "grok" | "antigravity" | "custom";
export type PromptVia = "stdin" | "arg" | "file";
export type OutputFormat = "text" | "claudeStreamJson" | "codexJsonl" | "grokJson";

export interface Decision {
  nextAction: NextAction;
  reasoning: string;
  targetAgent?: string | null;
  convertedPrompt?: string | null;
  stepTitle?: string | null;
  question?: string | null;
  summary?: string | null;
}

export interface RunRecord {
  agentId: string;
  nodeId: string;
  status: NodeStatus;
  startedAt: number;
  endedAt?: number | null;
  exitCode?: number | null;
  summary: string;
  diffStat: string;
  untracked: string[];
}

export interface Step {
  id: string;
  turn: number;
  planNodeId: string;
  decision?: Decision | null;
  error?: string | null;
  run?: RunRecord | null;
  reply?: string | null;
}

export interface TaskRecord {
  id: string;
  project: string;
  goal: string;
  createdAt: number;
  updatedAt: number;
  status: TaskStatus;
  reason?: string | null;
  turn: number;
  maxTurns: number;
  plannerSession: string;
  plannerStarted: boolean;
  steps: Step[];
}

export interface LogLine {
  seq: number;
  stream: LogStream;
  text: string;
}

export interface AgentSpec {
  id: string;
  name: string;
  kind: AgentKind;
  enabled: boolean;
  program: string;
  args: string[];
  promptVia: PromptVia;
  output: OutputFormat;
  role: string;
  allowDangerous: boolean;
}

export interface OrchestratorSettings {
  maxTurns: number;
  sameErrorLimit: number;
  dispatchTimeoutSecs: number;
  diffLimitBytes: number;
  logTailBytes: number;
  plannerProgram: string;
  plannerArgs: string[];
  plannerTimeoutSecs: number;
}

export interface Registry {
  version: number;
  orchestrator: OrchestratorSettings;
  agents: AgentSpec[];
}

export interface ProbeResult {
  id: string;
  resolved?: string | null;
  version?: string | null;
  error?: string | null;
}

export interface Preflight {
  isGit: boolean;
  hasCommit: boolean;
  dirty: boolean;
  branch?: string | null;
}

export interface Planning {
  stepId: string;
  nodeId: string;
  turn: number;
}

export interface OrchestraState {
  tasks: Record<string, TaskRecord>;
  activeTaskId: string | null;
  planning: Record<string, Planning>;
  logs: Record<string, LogLine[]>;
}

export const initialOrchestra: OrchestraState = {
  tasks: {},
  activeTaskId: null,
  planning: {},
  logs: {},
};

const LOG_LIMIT = 2000;

export const TASK_STATUS_LABEL: Record<TaskStatus, string> = {
  running: "自律実行中",
  waiting: "承認待ち",
  paused: "一時停止",
  completed: "完了確認",
  error: "エラー",
  aborted: "停止済み",
};

export const NODE_STATUS_LABEL: Record<NodeStatus, string> = {
  running: "実行中",
  succeeded: "完了",
  failed: "失敗",
  killed: "強制終了",
  timedOut: "時間切れ",
};

export const PROMPT_VIA_LABEL: Record<PromptVia, string> = {
  stdin: "標準入力",
  arg: "引数",
  file: "ファイル",
};

export const OUTPUT_LABEL: Record<OutputFormat, string> = {
  text: "テキスト",
  claudeStreamJson: "Claude stream-json",
  codexJsonl: "Codex JSONL",
  grokJson: "Grok JSON",
};

export type Badge = "spin" | "done" | "error" | "ask" | "stop";

export function isLive(status: TaskStatus) {
  return status === "running" || status === "waiting" || status === "paused";
}

export function taskBadge(status: TaskStatus): Badge {
  switch (status) {
    case "running":
      return "spin";
    case "waiting":
    case "paused":
      return "ask";
    case "completed":
      return "done";
    case "error":
      return "error";
    default:
      return "stop";
  }
}

export function nodeBadge(status: NodeStatus): Badge {
  switch (status) {
    case "running":
      return "spin";
    case "succeeded":
      return "done";
    case "killed":
      return "stop";
    default:
      return "error";
  }
}

export function agentName(agents: AgentSpec[], id: string | null | undefined) {
  if (!id) return "";
  return agents.find((agent) => agent.id === id)?.name || id;
}

export function stepLabel(step: Step, agents: AgentSpec[]): { who: string; what: string; badge: Badge } {
  const decision = step.decision;
  if (!decision) {
    return { who: "Grok", what: step.error ? "判断の失敗" : "判断", badge: step.error ? "error" : "spin" };
  }
  if (decision.nextAction === "dispatch") {
    return {
      who: agentName(agents, decision.targetAgent),
      what: decision.stepTitle || "作業",
      badge: step.run ? nodeBadge(step.run.status) : "spin",
    };
  }
  if (decision.nextAction === "complete") {
    return { who: "Grok", what: decision.stepTitle || "完了", badge: "done" };
  }
  return { who: "Grok", what: decision.stepTitle || "質問", badge: step.reply ? "done" : "ask" };
}

export function sameProject(a: string, b: string) {
  return normalizeProject(a) === normalizeProject(b);
}

function normalizeProject(path: string) {
  return path.replace(/\//g, "\\").replace(/\\+$/, "").toLowerCase();
}

type TaskMap = OrchestraState["tasks"];

export function projectTasks(tasks: TaskMap, project: string) {
  if (!project) return [];
  return Object.values(tasks)
    .filter((task) => sameProject(task.project, project))
    .sort((a, b) => b.createdAt - a.createdAt);
}

export function liveTask(tasks: TaskMap) {
  return Object.values(tasks).find((task) => isLive(task.status)) ?? null;
}

export function plannerSessions(tasks: TaskMap) {
  return new Set(Object.values(tasks).map((task) => task.plannerSession));
}

export type OrchestraAction =
  | { type: "tasks-loaded"; project: string; tasks: TaskRecord[] }
  | { type: "task-removed"; id: string }
  | { type: "select"; id: string | null }
  | { type: "log-batch"; nodeId: string; lines: LogLine[] }
  | { type: "logs-loaded"; nodeId: string; lines: LogLine[] };

export function reduceOrchestra(
  state: OrchestraState,
  event: Record<string, unknown> | OrchestraAction,
): OrchestraState {
  const row = event as Record<string, unknown>;
  switch (String(row.type ?? "")) {
    case "task": {
      const task = row.task as TaskRecord | undefined;
      if (!task || typeof task.id !== "string") return state;
      return settlePlanning({ ...state, tasks: { ...state.tasks, [task.id]: task } }, task.id);
    }
    case "planning": {
      const taskId = text(row.taskId);
      if (!taskId) return state;
      return {
        ...state,
        planning: {
          ...state.planning,
          [taskId]: { stepId: text(row.stepId), nodeId: text(row.nodeId), turn: number(row.turn) },
        },
      };
    }
    case "decision": {
      const taskId = text(row.taskId);
      const step = row.step as Step | undefined;
      if (!taskId || !step) return state;
      return settlePlanning(updateTask(state, taskId, (task) => ({ ...task, steps: upsertStep(task.steps, step) })), taskId);
    }
    case "node-started": {
      const taskId = text(row.taskId);
      const stepId = text(row.stepId);
      return updateTask(state, taskId, (task) => ({
        ...task,
        steps: task.steps.map((step) =>
          step.id === stepId && !step.run
            ? {
                ...step,
                run: {
                  agentId: text(row.agentId),
                  nodeId: text(row.nodeId),
                  status: "running",
                  startedAt: Date.now(),
                  summary: "",
                  diffStat: "",
                  untracked: [],
                },
              }
            : step,
        ),
      }));
    }
    case "node-finished": {
      const taskId = text(row.taskId);
      const stepId = text(row.stepId);
      const run = row.run as RunRecord | undefined;
      if (!run) return state;
      return updateTask(state, taskId, (task) => ({
        ...task,
        steps: task.steps.map((step) => (step.id === stepId ? { ...step, run } : step)),
      }));
    }
    case "state": {
      const taskId = text(row.taskId);
      const next = updateTask(state, taskId, (task) => ({
        ...task,
        status: (text(row.status) as TaskStatus) || task.status,
        turn: number(row.turn) || task.turn,
        maxTurns: number(row.maxTurns) || task.maxTurns,
        reason: typeof row.reason === "string" ? row.reason : null,
        updatedAt: Date.now(),
      }));
      return settlePlanning(next, taskId);
    }
    case "tasks-loaded": {
      const action = event as Extract<OrchestraAction, { type: "tasks-loaded" }>;
      const tasks = { ...state.tasks };
      for (const [id, task] of Object.entries(tasks)) {
        if (sameProject(task.project, action.project) && !isLive(task.status)) delete tasks[id];
      }
      for (const task of action.tasks) {
        const current = tasks[task.id];
        if (!current || current.updatedAt <= task.updatedAt) tasks[task.id] = task;
      }
      return { ...state, tasks };
    }
    case "task-removed": {
      const action = event as Extract<OrchestraAction, { type: "task-removed" }>;
      const tasks = { ...state.tasks };
      delete tasks[action.id];
      return { ...state, tasks, activeTaskId: state.activeTaskId === action.id ? null : state.activeTaskId };
    }
    case "select": {
      const action = event as Extract<OrchestraAction, { type: "select" }>;
      return { ...state, activeTaskId: action.id };
    }
    case "log-batch":
    case "logs-loaded": {
      const action = event as Extract<OrchestraAction, { type: "log-batch" | "logs-loaded" }>;
      if (!action.nodeId || !Array.isArray(action.lines) || action.lines.length === 0) return state;
      return { ...state, logs: { ...state.logs, [action.nodeId]: mergeLines(state.logs[action.nodeId], action.lines) } };
    }
    default:
      return state;
  }
}

function updateTask(state: OrchestraState, taskId: string, change: (task: TaskRecord) => TaskRecord): OrchestraState {
  const task = state.tasks[taskId];
  if (!task) return state;
  return { ...state, tasks: { ...state.tasks, [taskId]: change(task) } };
}

function upsertStep(steps: Step[], step: Step) {
  const index = steps.findIndex((item) => item.id === step.id);
  if (index < 0) return [...steps, step];
  const next = [...steps];
  next[index] = step;
  return next;
}

function settlePlanning(state: OrchestraState, taskId: string): OrchestraState {
  const pending = state.planning[taskId];
  if (!pending) return state;
  const task = state.tasks[taskId];
  const step = task?.steps.find((item) => item.id === pending.stepId);
  const done = !task || task.status !== "running" || Boolean(step && (step.decision || step.error));
  if (!done) return state;
  const planning = { ...state.planning };
  delete planning[taskId];
  return { ...state, planning };
}

function mergeLines(current: LogLine[] | undefined, incoming: LogLine[]) {
  const existing = current ?? [];
  const last = existing.length ? existing[existing.length - 1].seq : 0;
  let merged: LogLine[];
  if (incoming.every((line) => line.seq > last)) {
    merged = existing.concat(incoming);
  } else {
    const bySeq = new Map<number, LogLine>();
    for (const line of existing) bySeq.set(line.seq, line);
    for (const line of incoming) bySeq.set(line.seq, line);
    merged = [...bySeq.values()].sort((a, b) => a.seq - b.seq);
  }
  return merged.length > LOG_LIMIT ? merged.slice(merged.length - LOG_LIMIT) : merged;
}

export function nextCustomId(agents: AgentSpec[]) {
  let index = 1;
  while (agents.some((agent) => agent.id === `custom-${index}`)) index += 1;
  return `custom-${index}`;
}

export function blankCustomAgent(agents: AgentSpec[]): AgentSpec {
  const id = nextCustomId(agents);
  return {
    id,
    name: `カスタム ${id.slice("custom-".length)}`,
    kind: "custom",
    enabled: false,
    program: "",
    args: [],
    promptVia: "stdin",
    output: "text",
    role: "",
    allowDangerous: false,
  };
}

function text(value: unknown) {
  return typeof value === "string" ? value : "";
}

function number(value: unknown) {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}
