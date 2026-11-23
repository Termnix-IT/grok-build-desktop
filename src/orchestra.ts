export type TaskStatus = "open" | "paused" | "closed";
export type RunStatus = "queued" | "running" | "succeeded" | "failed" | "killed" | "timedOut" | "cancelled";
export type Source = "bot" | "manual";
export type ErrorKind = "exit" | "timeout" | "killed" | "launch" | "denied" | "interrupted" | "cancelled";
export type FileChange = "modified" | "added" | "deleted" | "renamed" | "untracked";
export type LogStream = "stdout" | "stderr" | "system";
export type AgentKind = "claude" | "codex" | "grok" | "antigravity" | "custom";
export type PromptVia = "stdin" | "arg" | "file";
export type OutputFormat = "text" | "claudeStreamJson" | "codexJsonl" | "grokJson" | "antigravityJson";

export interface ChangedFile {
  path: string;
  change: FileChange;
  added?: number | null;
  deleted?: number | null;
}

export interface RunError {
  kind: ErrorKind;
  message: string;
}

export interface RunRecord {
  id: string;
  taskId: string;
  agentId: string;
  source: Source;
  title: string;
  prompt: string;
  status: RunStatus;
  queuedAt: number;
  startedAt?: number | null;
  endedAt?: number | null;
  timeoutSecs: number;
  exitCode?: number | null;
  summary: string;
  error?: RunError | null;
  denied: string[];
  diffStat: string;
  changedFiles: ChangedFile[];
  truncated: boolean;
  recoveryPoint?: string | null;
}

export interface TaskRecord {
  id: string;
  project: string;
  title: string;
  source: Source;
  createdAt: number;
  updatedAt: number;
  status: TaskStatus;
  pauseReason?: string | null;
  dispatches: number;
  dispatchLimit: number;
  runs: RunRecord[];
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
  timeoutSecs?: number | null;
}

export interface Limits {
  maxDispatchesPerTask: number;
  maxConsecutivePerProject: number;
  sameErrorLimit: number;
  defaultTimeoutSecs: number;
  maxQueue: number;
  pauseOnDeletion: boolean;
  logTailBytes: number;
}

export interface Registry {
  version: number;
  limits: Limits;
  agents: AgentSpec[];
}

export interface ProbeResult {
  id: string;
  resolved?: string | null;
  version?: string | null;
  error?: string | null;
}

export type GatewayStatus =
  | { state: "starting" }
  | { state: "listening"; pipe: string }
  | { state: "failed"; error: string };

export interface Rejection {
  project: string;
  agent: string;
  code: string;
  at: number;
}

export interface GatewayView {
  accepting: boolean;
  allowedProjects: string[];
  status: GatewayStatus;
  pipe?: string | null;
  cliPath?: string | null;
}

export interface Submitted {
  jobId: string;
  taskId: string;
  position: number;
}

export interface HubSnapshot {
  running?: string | null;
  queued?: string[];
  active?: TaskRecord[];
  gateway?: { status: GatewayStatus; accepting: boolean };
}

export interface GatewayState {
  status: GatewayStatus | null;
  accepting: boolean;
  rejections: Rejection[];
}

export interface OrchestraState {
  tasks: Record<string, TaskRecord>;
  activeTaskId: string | null;
  /** When this screen last heard about each task, so an older list cannot overwrite it. */
  touched: Record<string, number>;
  logs: Record<string, LogLine[]>;
  logOrder: string[];
  gateway: GatewayState;
}

export const initialOrchestra: OrchestraState = {
  tasks: {},
  activeTaskId: null,
  touched: {},
  logs: {},
  logOrder: [],
  gateway: { status: null, accepting: false, rejections: [] },
};

const LOG_LIMIT = 2000;
const LOG_NODES = 50;
const REJECTIONS = 5;

export const TASK_STATUS_LABEL: Record<TaskStatus, string> = {
  open: "進行中",
  paused: "一時停止",
  closed: "終了",
};

export const RUN_STATUS_LABEL: Record<RunStatus, string> = {
  queued: "待機中",
  running: "実行中",
  succeeded: "完了",
  failed: "失敗",
  killed: "強制終了",
  timedOut: "時間切れ",
  cancelled: "取り消し",
};

export const SOURCE_LABEL: Record<Source, string> = {
  bot: "Grok Bot",
  manual: "画面",
};

export const ERROR_KIND_LABEL: Record<ErrorKind, string> = {
  exit: "異常終了",
  timeout: "時間切れ",
  killed: "強制終了",
  launch: "起動の失敗",
  denied: "操作の拒否",
  interrupted: "中断",
  cancelled: "取り消し",
};

export const FILE_CHANGE_MARK: Record<FileChange, { mark: string; label: string }> = {
  modified: { mark: "M", label: "変更" },
  added: { mark: "A", label: "追加" },
  deleted: { mark: "D", label: "削除" },
  renamed: { mark: "R", label: "名前の変更" },
  untracked: { mark: "U", label: "新規" },
};

export const REJECTION_LABEL: Record<string, string> = {
  project_not_allowed: "許可していないプロジェクト",
  project_paused: "連続実行の上限",
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
  antigravityJson: "Antigravity JSON",
};

export type Badge = "spin" | "wait" | "done" | "error" | "ask" | "stop";

export function runBadge(status: RunStatus): Badge {
  switch (status) {
    case "queued":
      return "wait";
    case "running":
      return "spin";
    case "succeeded":
      return "done";
    case "failed":
    case "timedOut":
      return "error";
    default:
      return "stop";
  }
}

export function runFinished(status: RunStatus) {
  return status !== "queued" && status !== "running";
}

export function taskActive(task: TaskRecord) {
  return task.runs.some((run) => !runFinished(run.status));
}

export function taskBadge(task: TaskRecord): { badge: Badge; label: string } {
  if (task.runs.some((run) => run.status === "running")) return { badge: "spin", label: RUN_STATUS_LABEL.running };
  if (task.runs.some((run) => run.status === "queued")) return { badge: "wait", label: RUN_STATUS_LABEL.queued };
  if (task.status === "paused") return { badge: "ask", label: TASK_STATUS_LABEL.paused };
  const last = task.runs[task.runs.length - 1];
  if (task.status === "closed") return { badge: last?.status === "succeeded" ? "done" : "stop", label: TASK_STATUS_LABEL.closed };
  if (!last) return { badge: "wait", label: TASK_STATUS_LABEL.open };
  return { badge: runBadge(last.status), label: RUN_STATUS_LABEL[last.status] };
}

export function agentName(agents: AgentSpec[], id: string | null | undefined) {
  if (!id) return "";
  return agents.find((agent) => agent.id === id)?.name || id;
}

export function agentKind(agents: AgentSpec[], id: string): AgentKind {
  return agents.find((agent) => agent.id === id)?.kind ?? "custom";
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

/** The one job the hub is running now, if this screen knows about it. */
export function liveRun(tasks: TaskMap): { task: TaskRecord; run: RunRecord } | null {
  for (const task of Object.values(tasks)) {
    const run = task.runs.find((item) => item.status === "running");
    if (run) return { task, run };
  }
  return null;
}

export function queuedRuns(tasks: TaskMap) {
  return Object.values(tasks).flatMap((task) => task.runs.filter((run) => run.status === "queued"));
}

export type OrchestraAction =
  | { type: "tasks-loaded"; project: string | null; tasks: TaskRecord[]; requestedAt: number }
  | { type: "task-removed"; id: string }
  | { type: "select"; id: string | null }
  | { type: "log-batch"; nodeId: string; lines: LogLine[] }
  | { type: "logs-loaded"; nodeId: string; lines: LogLine[] };

export function reduceOrchestra(
  state: OrchestraState,
  event: Record<string, unknown> | OrchestraAction,
  at: number = Date.now(),
): OrchestraState {
  const row = event as Record<string, unknown>;
  switch (String(row.type ?? "")) {
    case "task": {
      const task = row.task as TaskRecord | undefined;
      if (!task || typeof task.id !== "string" || !Array.isArray(task.runs)) return state;
      return {
        ...state,
        tasks: { ...state.tasks, [task.id]: task },
        touched: { ...state.touched, [task.id]: at },
      };
    }
    case "run": {
      const run = row.run as RunRecord | undefined;
      const taskId = typeof row.taskId === "string" ? row.taskId : run?.taskId;
      if (!run || !taskId || typeof run.id !== "string") return state;
      const task = state.tasks[taskId];
      // A task not loaded yet still remembers the time, so a list fetched before this job
      // changed cannot put the older state on screen.
      if (!task) return { ...state, touched: { ...state.touched, [taskId]: at } };
      const index = task.runs.findIndex((item) => item.id === run.id);
      const runs = [...task.runs];
      if (index < 0) runs.push(run);
      else runs[index] = run;
      const updatedAt = Math.max(task.updatedAt, run.endedAt ?? 0, run.startedAt ?? 0, run.queuedAt ?? 0);
      return {
        ...state,
        tasks: { ...state.tasks, [taskId]: { ...task, runs, updatedAt } },
        touched: { ...state.touched, [taskId]: at },
      };
    }
    case "gateway": {
      const status = row.status as GatewayStatus | undefined;
      const rejected = row.rejected as Rejection | null | undefined;
      return {
        ...state,
        gateway: {
          status: status && typeof status.state === "string" ? status : state.gateway.status,
          accepting: typeof row.accepting === "boolean" ? row.accepting : state.gateway.accepting,
          rejections: rejected ? [rejected, ...state.gateway.rejections].slice(0, REJECTIONS) : state.gateway.rejections,
        },
      };
    }
    case "tasks-loaded": {
      const action = event as Extract<OrchestraAction, { type: "tasks-loaded" }>;
      const tasks = { ...state.tasks };
      const touched = { ...state.touched };
      const fresher = (id: string) => (state.touched[id] ?? 0) > action.requestedAt;
      if (action.project !== null) {
        const listed = new Set(action.tasks.map((task) => task.id));
        for (const [id, task] of Object.entries(tasks)) {
          if (sameProject(task.project, action.project) && !listed.has(id) && !fresher(id)) {
            delete tasks[id];
            delete touched[id];
          }
        }
      }
      for (const task of action.tasks) {
        if (!fresher(task.id)) tasks[task.id] = task;
      }
      return { ...state, tasks, touched };
    }
    case "task-removed": {
      const action = event as Extract<OrchestraAction, { type: "task-removed" }>;
      const task = state.tasks[action.id];
      const tasks = { ...state.tasks };
      const touched = { ...state.touched };
      delete tasks[action.id];
      delete touched[action.id];
      let logs = state.logs;
      let logOrder = state.logOrder;
      if (task) {
        const nodes = new Set(task.runs.map((run) => run.id));
        logs = Object.fromEntries(Object.entries(state.logs).filter(([id]) => !nodes.has(id)));
        logOrder = state.logOrder.filter((id) => !nodes.has(id));
      }
      return {
        ...state,
        tasks,
        touched,
        logs,
        logOrder,
        activeTaskId: state.activeTaskId === action.id ? null : state.activeTaskId,
      };
    }
    case "select": {
      const action = event as Extract<OrchestraAction, { type: "select" }>;
      return { ...state, activeTaskId: action.id };
    }
    case "log-batch":
    case "logs-loaded": {
      const action = event as Extract<OrchestraAction, { type: "log-batch" | "logs-loaded" }>;
      if (!action.nodeId || !Array.isArray(action.lines) || action.lines.length === 0) return state;
      const logs = { ...state.logs, [action.nodeId]: mergeLines(state.logs[action.nodeId], action.lines) };
      let logOrder = state.logOrder.includes(action.nodeId)
        ? state.logOrder
        : [...state.logOrder, action.nodeId];
      while (logOrder.length > LOG_NODES) {
        const [oldest, ...rest] = logOrder;
        delete logs[oldest];
        logOrder = rest;
      }
      return { ...state, logs, logOrder };
    }
    default:
      return state;
  }
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
    timeoutSecs: null,
  };
}

export function formatSeconds(total: number) {
  if (total < 60) return `${total}秒`;
  const minutes = Math.floor(total / 60);
  const seconds = total % 60;
  return seconds ? `${minutes}分${seconds}秒` : `${minutes}分`;
}
