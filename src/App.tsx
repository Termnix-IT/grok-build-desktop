import { Fragment, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { AgentSettings } from "./AgentSettings";
import { GatewaySettings } from "./GatewaySettings";
import { LogDrawer, StatusBadge, TaskTimeline, type LogTarget, type LogView } from "./OrchestraView";
import {
  type GatewayView,
  type HubSnapshot,
  type LogLine,
  type OrchestraAction,
  type OrchestraState,
  type Registry,
  type TaskRecord,
  FILE_CHANGE_MARK,
  REJECTION_LABEL,
  RUN_STATUS_LABEL,
  SOURCE_LABEL,
  TASK_STATUS_LABEL,
  agentName,
  initialOrchestra,
  liveRun,
  nextStamp,
  projectBadge,
  projectList,
  projectTasks,
  queuedRuns,
  reduceOrchestra,
  runBadge,
  runFinished,
  sameProject,
  taskActive,
  taskBadge,
} from "./orchestra";
import "./App.css";

const LAST_KEY = "gbd-last-folder";
const CONTEXT_KEY = "gbd-context-open";
const MOTION_KEY = "gbd-motion";
const LOG_VIEW_KEY = "gbd-log-view";

type SettingsPage = "display" | "agents" | "gateway" | "app";

const SETTINGS_PAGES: { id: SettingsPage; label: string }[] = [
  { id: "display", label: "表示" },
  { id: "agents", label: "エージェント" },
  { id: "gateway", label: "連携" },
  { id: "app", label: "このアプリ" },
];

interface Preflight {
  isGit: boolean;
  hasCommit: boolean;
  branch?: string | null;
}

interface RestoreFile {
  path: string;
  change: string;
}

interface PendingRestore {
  jobId: string;
  project: string;
  files: RestoreFile[];
  /** The files changed after the first preview, so the user confirms the new list. */
  changed?: boolean;
}

function errorText(error: unknown) {
  if (error instanceof Error && error.message) return error.message;
  if (typeof error === "string") return error;
  // Hub commands reject with { code, message }.
  if (error && typeof error === "object" && typeof (error as { message?: unknown }).message === "string") {
    return (error as { message: string }).message;
  }
  return String(error);
}

function formatUpdateError(error: unknown) {
  const message = errorText(error);
  const lower = message.toLowerCase();
  if (
    lower.includes("pubkey") ||
    lower.includes("public key") ||
    lower.includes("signature") ||
    lower.includes("minisign") ||
    message.includes("署名")
  ) {
    return "署名鍵は次の公開時に設定します。それまでは適用できません。";
  }
  if (
    lower.includes("404") ||
    lower.includes("latest.json") ||
    lower.includes("release json") ||
    lower.includes("release not found") ||
    lower.includes("could not fetch") ||
    lower.includes("network") ||
    lower.includes("fetch") ||
    lower.includes("dns") ||
    lower.includes("connect") ||
    lower.includes("timed out") ||
    lower.includes("timeout")
  ) {
    return "公開された更新情報はまだありません。";
  }
  return message;
}

function windowAct(action: "minimize" | "toggleMaximize" | "close") {
  const win = getCurrentWindow();
  const task =
    action === "minimize" ? win.minimize() : action === "close" ? win.close() : win.toggleMaximize();
  void task.catch(() => {});
}

function baseName(path: string) {
  return path.split(/[\\/]/).filter(Boolean).pop() || path;
}

function readLogView(): LogView {
  return localStorage.getItem(LOG_VIEW_KEY) === "raw" ? "raw" : "brief";
}

export default function App() {
  const [folder, setFolder] = useState(() => localStorage.getItem(LAST_KEY) ?? "");
  const [allowed, setAllowed] = useState<string[]>([]);
  const [taskProjects, setTaskProjects] = useState<string[]>([]);
  const [openFolders, setOpenFolders] = useState<string[]>(() => {
    const last = localStorage.getItem(LAST_KEY);
    return last ? [last] : [];
  });
  const [filter, setFilter] = useState("");
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [contextOpen, setContextOpen] = useState(() => localStorage.getItem(CONTEXT_KEY) !== "0");
  const [motion, setMotion] = useState(() => localStorage.getItem(MOTION_KEY) !== "0");
  const [settingsPage, setSettingsPage] = useState<SettingsPage>("display");
  const [appVersion, setAppVersion] = useState("");
  const [updateNote, setUpdateNote] = useState("");
  const [updateBusy, setUpdateBusy] = useState(false);
  const [hasUpdate, setHasUpdate] = useState(false);
  const [pendingApply, setPendingApply] = useState(false);
  const [orch, setOrch] = useState<OrchestraState>(initialOrchestra);
  const [registry, setRegistry] = useState<Registry | null>(null);
  const [logTarget, setLogTarget] = useState<LogTarget | null>(null);
  const [logView, setLogView] = useState<LogView>(readLogView);
  const [pendingTaskDelete, setPendingTaskDelete] = useState<string | null>(null);
  const [pendingRestore, setPendingRestore] = useState<PendingRestore | null>(null);
  const [pendingInit, setPendingInit] = useState<string | null>(null);
  const [openTasks, setOpenTasks] = useState<string[]>([]);
  const [hubBusy, setHubBusy] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [preflight, setPreflight] = useState<{ project: string; result: Preflight } | null>(null);
  const [preflightTick, setPreflightTick] = useState(0);
  const folderRef = useRef(folder);
  folderRef.current = folder;
  const orchRef = useRef(orch);
  orchRef.current = orch;
  const pushOrchRef = useRef<(event: Record<string, unknown> | OrchestraAction) => void>(() => {});
  const transcriptRef = useRef<HTMLDivElement>(null);
  const stickRef = useRef(true);
  const updateRef = useRef<Update | null>(null);

  useEffect(() => {
    let frame = 0;
    let timer = 0;
    // Every change to the hub state goes through this queue, so a list fetched earlier is
    // applied in order with the events that arrived around it.
    const queue: { event: Record<string, unknown> | OrchestraAction; at: number }[] = [];
    const flush = () => {
      if (frame) window.cancelAnimationFrame(frame);
      if (timer) window.clearTimeout(timer);
      frame = 0;
      timer = 0;
      const batch = queue.splice(0);
      if (batch.length === 0) return;
      setOrch((current) => batch.reduce((state, item) => reduceOrchestra(state, item.event, item.at), current));
    };
    const push = (event: Record<string, unknown> | OrchestraAction) => {
      queue.push({ event, at: nextStamp() });
      if (queue.length >= 500) {
        flush();
        return;
      }
      // A minimized window gets no animation frames, so a timer flushes the queue too.
      if (!frame) frame = window.requestAnimationFrame(flush);
      if (!timer) timer = window.setTimeout(flush, 250);
    };
    pushOrchRef.current = push;
    const known = new Set<string>();
    const fetching = new Set<string>();
    const stale = new Set<string>();
    // A job event that arrives while the task is being fetched may be newer than the answer,
    // and the answer is then dropped as older, so the task is fetched once more.
    const fetchTask = (taskId: string) => {
      fetching.add(taskId);
      stale.delete(taskId);
      const requestedAt = nextStamp();
      void invoke<TaskRecord | null>("orchestrator_task", { taskId })
        .then((task) => {
          if (!task) return;
          known.add(task.id);
          push({ type: "tasks-loaded", project: null, tasks: [task], requestedAt });
        })
        .catch(() => {})
        .finally(() => {
          fetching.delete(taskId);
          if (stale.has(taskId)) fetchTask(taskId);
        });
    };
    const stopEvents = listen<Record<string, unknown>>("orchestrator://event", (event) => {
      const payload = event.payload;
      if (payload.type === "task") {
        const task = payload.task as TaskRecord | undefined;
        if (task?.id) known.add(task.id);
      }
      push(payload);
      // A job of a task this screen has not loaded (a new folder, or a reloaded screen) fetches its task.
      const taskId = payload.type === "run" && typeof payload.taskId === "string" ? payload.taskId : "";
      if (!taskId) return;
      if (fetching.has(taskId)) stale.add(taskId);
      else if (!known.has(taskId) && !orchRef.current.tasks[taskId]) fetchTask(taskId);
    });
    const stopLogs = listen<{ nodeId?: string; lines?: LogLine[]; brief?: LogLine[] }>("agent://log", (event) => {
      const { nodeId, lines, brief } = event.payload;
      if (typeof nodeId !== "string") return;
      push({
        type: "log-batch",
        nodeId,
        lines: Array.isArray(lines) ? lines : [],
        brief: Array.isArray(brief) ? brief : [],
      });
    });
    const requestedAt = nextStamp();
    void invoke<HubSnapshot>("orchestrator_snapshot")
      .then((snapshot) => {
        if (snapshot?.active?.length) push({ type: "tasks-loaded", project: null, tasks: snapshot.active, requestedAt });
        if (snapshot?.gateway) {
          push({
            type: "gateway",
            status: snapshot.gateway.status,
            accepting: snapshot.gateway.accepting,
            rejected: null,
            requestedAt,
          });
        }
      })
      .catch(() => {});
    void invoke<Registry>("agents_config_get").then(setRegistry).catch(() => {});
    void invoke<GatewayView>("gateway_get")
      .then((view) => setAllowed(view.allowedProjects))
      .catch(() => {});
    void invoke<string[]>("orchestrator_projects").then(setTaskProjects).catch(() => {});
    // A folder saved by an older version may be spelled differently from the hub's form.
    const last = localStorage.getItem(LAST_KEY);
    if (last) {
      void invoke<string>("orchestrator_normalize", { project: last })
        .then((normalized) => {
          if (normalized === last || folderRef.current !== last) return;
          setFolder(normalized);
          localStorage.setItem(LAST_KEY, normalized);
          setOpenFolders((current) => current.map((item) => (item === last ? normalized : item)));
        })
        .catch(() => {});
    }
    return () => {
      pushOrchRef.current = () => {};
      if (frame) window.cancelAnimationFrame(frame);
      if (timer) window.clearTimeout(timer);
      void stopEvents.then((stop) => stop());
      void stopLogs.then((stop) => stop());
    };
  }, []);

  const orchTasks = orch.tasks;
  const projects = useMemo(() => {
    const list = projectList([...allowed, ...taskProjects], orchTasks);
    if (folder && !list.some((path) => sameProject(path, folder))) list.push(folder);
    return list;
  }, [allowed, taskProjects, orchTasks, folder]);
  const projectsKey = projects.join("\n");

  // Each listed folder loads its tasks, so a folder that is not selected still shows its marks.
  useEffect(() => {
    if (!projectsKey) return;
    let cancelled = false;
    for (const project of projectsKey.split("\n")) {
      const requestedAt = nextStamp();
      void invoke<TaskRecord[]>("orchestrator_tasks", { project })
        .then((tasks) => {
          if (!cancelled) pushOrchRef.current({ type: "tasks-loaded", project, tasks, requestedAt });
        })
        .catch(() => {});
    }
    return () => {
      cancelled = true;
    };
  }, [projectsKey]);

  useEffect(() => {
    void getVersion().then(setAppVersion).catch(() => setAppVersion("不明"));
    return () => {
      void updateRef.current?.close();
    };
  }, []);

  useEffect(() => {
    if (!settingsOpen) return;
    function onKey(event: KeyboardEvent) {
      if (event.key === "Escape") setSettingsOpen(false);
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [settingsOpen]);

  // A folder made into a repository outside the app is noticed when the window comes back.
  useEffect(() => {
    const onFocus = () => setPreflightTick((tick) => tick + 1);
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, []);

  const viewTask = orch.activeTaskId ? orch.tasks[orch.activeTaskId] ?? null : null;
  const viewProject = viewTask?.project ?? folder;
  // A job of the shown folder can make the first commit or change the branch, so the check reruns.
  const projectRuns = useMemo(
    () => (viewProject ? projectTasks(orch.tasks, viewProject).reduce((sum, task) => sum + task.runs.length, 0) : 0),
    [orch.tasks, viewProject],
  );

  useEffect(() => {
    if (!viewProject) {
      setPreflight(null);
      return;
    }
    let cancelled = false;
    void invoke<Preflight>("orchestrator_preflight", { project: viewProject })
      .then((result) => {
        if (!cancelled) setPreflight({ project: viewProject, result });
      })
      .catch(() => {
        if (!cancelled) setPreflight(null);
      });
    return () => {
      cancelled = true;
    };
  }, [viewProject, preflightTick, projectRuns]);

  useEffect(() => {
    const box = transcriptRef.current;
    if (!box || !stickRef.current) return;
    box.scrollTop = box.scrollHeight;
  }, [viewTask]);

  function chooseFolder(path: string) {
    if (!sameProject(path, folder)) {
      setError("");
      setNotice("");
      setLogTarget(null);
      setOrch((current) => (current.activeTaskId ? reduceOrchestra(current, { type: "select", id: null }) : current));
    }
    setFolder(path);
    localStorage.setItem(LAST_KEY, path);
    setOpenFolders((current) => (current.some((item) => sameProject(item, path)) ? current : [...current, path]));
  }

  function toggleFolder(path: string) {
    setOpenFolders((current) =>
      current.some((item) => sameProject(item, path))
        ? current.filter((item) => !sameProject(item, path))
        : [...current, path],
    );
  }

  /** A new project is a folder Grok Bot may use, so it goes into the allowed list. */
  async function addProject() {
    const picked = await open({ directory: true, multiple: false, title: "プロジェクトフォルダ" });
    if (typeof picked !== "string") return;
    try {
      const next = await invoke<GatewayView>("gateway_add_project", { project: picked });
      setAllowed(next.allowedProjects);
      chooseFolder(await invoke<string>("orchestrator_normalize", { project: picked }));
    } catch (failure) {
      setError(errorText(failure));
    }
  }

  const flowError = useCallback((failure: unknown) => {
    setError(errorText(failure));
  }, []);

  async function hubCall(command: string, args?: Record<string, unknown>) {
    setHubBusy(true);
    try {
      await invoke(command, args);
    } catch (failure) {
      flowError(failure);
    } finally {
      setHubBusy(false);
    }
  }

  const cancelJob = useCallback(
    (jobId: string) => {
      void invoke("orchestrator_cancel", { jobId }).catch(flowError);
    },
    [flowError],
  );

  const killJob = useCallback(
    (jobId: string) => {
      void invoke("orchestrator_kill", { jobId }).catch(flowError);
    },
    [flowError],
  );

  const resumeTask = useCallback((taskId: string) => {
    void hubCall("orchestrator_resume", { taskId });
    // hubCall only reads stable setters.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const closeTask = useCallback((taskId: string) => {
    void hubCall("orchestrator_close", { taskId });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** Shows which files going back would overwrite before anything is touched. */
  const askRestore = useCallback(
    (jobId: string, project: string) => {
      setNotice("");
      void invoke<{ files: RestoreFile[] }>("orchestrator_restore_preview", { jobId })
        .then((result) => setPendingRestore({ jobId, project, files: result.files ?? [] }))
        .catch(flowError);
    },
    [flowError],
  );

  async function confirmRestore() {
    const pending = pendingRestore;
    setPendingRestore(null);
    if (!pending) return;
    setHubBusy(true);
    try {
      // A job may have changed the folder while the dialog was open; the list is shown again then.
      const latest = await invoke<{ files: RestoreFile[] }>("orchestrator_restore_preview", { jobId: pending.jobId });
      const fresh = latest.files ?? [];
      const key = (files: RestoreFile[]) => files.map((file) => `${file.change}:${file.path}`).sort().join("\n");
      if (key(fresh) !== key(pending.files)) {
        setPendingRestore({ ...pending, files: fresh, changed: true });
        return;
      }
      const result = await invoke<{ files: RestoreFile[] }>("orchestrator_restore", { jobId: pending.jobId });
      const count = result.files?.length ?? 0;
      setError("");
      setNotice(count ? `作業前の状態に ${count} 件のファイルを戻しました` : "戻すファイルはありませんでした");
    } catch (failure) {
      flowError(failure);
    } finally {
      setHubBusy(false);
    }
  }

  async function confirmInit() {
    const project = pendingInit;
    setPendingInit(null);
    if (!project) return;
    setHubBusy(true);
    try {
      await invoke<Preflight>("orchestrator_git_init", { project });
      // Asking again drops any check that started before the init and would answer late.
      setPreflightTick((tick) => tick + 1);
      setError("");
      setNotice(`${baseName(project)} でジョブを受けられるようにしました`);
    } catch (failure) {
      flowError(failure);
    } finally {
      setHubBusy(false);
    }
  }

  /** Kept apart from hubBusy, so a restore or an init in another folder never blocks the stop. */
  async function stopAll() {
    setStopping(true);
    try {
      await invoke("orchestrator_stop_all");
    } catch (failure) {
      flowError(failure);
    } finally {
      setStopping(false);
    }
  }

  async function toggleAccepting(accepting: boolean) {
    setHubBusy(true);
    try {
      const view = await invoke<GatewayView>("gateway_get");
      if (accepting && view.allowedProjects.length === 0) {
        setSettingsPage("gateway");
        setSettingsOpen(true);
        return;
      }
      await invoke("gateway_save", { accepting, allowedProjects: view.allowedProjects });
    } catch (failure) {
      flowError(failure);
    } finally {
      setHubBusy(false);
    }
  }

  async function resumeProject(project: string) {
    await hubCall("orchestrator_resume_project", { project });
  }

  /** Opening a task moves the folder selection with it, so the left pane and the task agree. */
  function selectTask(id: string | null) {
    if (id !== orch.activeTaskId) setLogTarget(null);
    stickRef.current = true;
    const task = id ? orch.tasks[id] : null;
    if (task && !sameProject(task.project, folder)) {
      setFolder(task.project);
      localStorage.setItem(LAST_KEY, task.project);
    }
    if (task) {
      setOpenFolders((current) =>
        current.some((item) => sameProject(item, task.project)) ? current : [...current, task.project],
      );
    }
    setError("");
    setNotice("");
    setOrch((current) => reduceOrchestra(current, { type: "select", id }));
  }

  function toggleTaskOpen(id: string) {
    setOpenTasks((current) => (current.includes(id) ? current.filter((item) => item !== id) : [...current, id]));
  }

  const openLog = useCallback((target: LogTarget) => {
    setLogTarget(target);
    void invoke<LogLine[]>("agent_log", { nodeId: target.nodeId })
      .then((lines) =>
        setOrch((current) => reduceOrchestra(current, { type: "logs-loaded", nodeId: target.nodeId, lines, kind: "raw" })),
      )
      .catch(() => {});
    void invoke<LogLine[]>("agent_brief", { nodeId: target.nodeId })
      .then((lines) =>
        setOrch((current) => reduceOrchestra(current, { type: "logs-loaded", nodeId: target.nodeId, lines, kind: "brief" })),
      )
      .catch(() => {});
  }, []);

  const chooseLogView = useCallback((view: LogView) => {
    setLogView(view);
    localStorage.setItem(LOG_VIEW_KEY, view);
  }, []);

  function deleteTask(id: string) {
    setPendingTaskDelete(null);
    const runs = new Set(orch.tasks[id]?.runs.map((run) => run.id) ?? []);
    void invoke("orchestrator_delete", { taskId: id })
      .then(() => {
        setLogTarget((current) => (current && runs.has(current.nodeId) ? null : current));
        pushOrchRef.current({ type: "task-removed", id });
      })
      .catch(flowError);
  }

  const closeLog = useCallback(() => setLogTarget(null), []);

  function setContext(nextOpen: boolean) {
    setContextOpen(nextOpen);
    localStorage.setItem(CONTEXT_KEY, nextOpen ? "1" : "0");
  }

  function setMotionEnabled(enabled: boolean) {
    setMotion(enabled);
    localStorage.setItem(MOTION_KEY, enabled ? "1" : "0");
  }

  async function checkUpdate() {
    setUpdateBusy(true);
    const previous = updateRef.current;
    updateRef.current = null;
    setHasUpdate(false);
    if (previous) void previous.close();
    try {
      const update = await check();
      if (!update) {
        setUpdateNote("いまの版が最新です");
        return;
      }
      updateRef.current = update;
      setHasUpdate(true);
      setUpdateNote(`新しい版 ${update.version} があります`);
    } catch (failure) {
      setUpdateNote(formatUpdateError(failure));
    } finally {
      setUpdateBusy(false);
    }
  }

  async function confirmApply() {
    const update = updateRef.current;
    setPendingApply(false);
    if (!update || import.meta.env.DEV) return;
    setUpdateBusy(true);
    try {
      await update.downloadAndInstall();
      setUpdateNote("インストールを開始しました。アプリは終了します。");
    } catch (failure) {
      setUpdateNote(formatUpdateError(failure));
    } finally {
      setUpdateBusy(false);
    }
  }

  const live = useMemo(() => liveRun(orchTasks), [orchTasks]);
  const waiting = useMemo(() => queuedRuns(orchTasks).length, [orchTasks]);
  const agents = useMemo(() => registry?.agents ?? [], [registry]);
  const gateway = orch.gateway;
  const pipeFailed = gateway.status?.state === "failed";
  const projectAllowed = Boolean(viewProject) && allowed.some((path) => sameProject(path, viewProject));
  const gatewayValue = pipeFailed ? "failed" : gateway.accepting ? "on" : "off";
  const repo = preflight && sameProject(preflight.project, viewProject) ? preflight.result : null;
  const repoProblem = !repo ? "" : !repo.isGit ? "git のリポジトリではありません" : !repo.hasCommit ? "コミットがありません" : "";
  const needle = filter.trim().toLowerCase();
  const openTitle = viewTask ? viewTask.title || "無題" : folder ? baseName(folder) : "";
  const projectRejections = gateway.rejections.filter((item) => viewProject && sameProject(item.project, viewProject));
  // Going back is refused while a job of the same folder runs or waits, since it would race the job.
  const projectBusy = (project: string) =>
    Object.values(orchTasks).some(
      (task) => sameProject(task.project, project) && task.runs.some((run) => !runFinished(run.status)),
    );
  const restoreBlocked = projectBusy(viewProject);
  const busyAnywhere = Boolean(live) || waiting > 0;

  return (
    <div className={`desk${contextOpen ? "" : " context-shut"}${motion ? " motion" : ""}`}>
      <aside className="rail">
        <label className="finder">
          <SearchIcon />
          <input value={filter} placeholder="検索" aria-label="検索" onChange={(event) => setFilter(event.target.value)} />
        </label>
        <button className="new-chat" type="button" onClick={() => void addProject()}>
          <FolderIcon />
          新規プロジェクト
        </button>
        <div className="folder-line" />
        <div className="folder-scroll">
          {projects.map((path) => {
            const selected = sameProject(path, folder);
            const tasks = projectTasks(orchTasks, path);
            const shown = needle
              ? tasks.filter(
                  (task) =>
                    task.title.toLowerCase().includes(needle) ||
                    task.runs.some((run) => run.title.toLowerCase().includes(needle)),
                )
              : tasks;
            if (needle && !baseName(path).toLowerCase().includes(needle) && shown.length === 0) return null;
            const isOpen = needle.length > 0 || openFolders.some((item) => sameProject(item, path));
            const mark = projectBadge(tasks);
            return (
              <div key={path} className="folder-block">
                <button
                  type="button"
                  className={selected ? "folder-node on" : "folder-node"}
                  title={path}
                  aria-expanded={isOpen}
                  onClick={() => {
                    if (selected) toggleFolder(path);
                    else chooseFolder(path);
                  }}
                >
                  <span className={isOpen ? "twist open" : "twist"} aria-hidden="true">
                    <svg width="15" height="15" viewBox="0 0 15 15">
                      <path d="M5.4 3.2 10.6 7.5 5.4 11.8z" fill="currentColor" />
                    </svg>
                  </span>
                  <span className="folder-name">{baseName(path)}</span>
                  {mark && <StatusBadge badge={mark.badge} label={mark.label} />}
                </button>
                <div className={isOpen ? "tree-fold open" : "tree-fold"} inert={!isOpen}>
                  <ul className="tree">
                    {shown.map((task) => {
                      const on = task.id === orch.activeTaskId;
                      const expanded = openTasks.includes(task.id) && task.runs.length > 0;
                      const taskMark = taskBadge(task);
                      return (
                        <Fragment key={task.id}>
                          <li className={on ? "tree-row task on" : "tree-row task"}>
                            <button
                              type="button"
                              className={expanded ? "task-twist open" : "task-twist"}
                              aria-label={expanded ? "畳む" : "開く"}
                              aria-expanded={expanded}
                              disabled={task.runs.length === 0}
                              onClick={() => toggleTaskOpen(task.id)}
                            >
                              <svg width="11" height="11" viewBox="0 0 15 15" aria-hidden="true">
                                <path d="M5.4 3.2 10.6 7.5 5.4 11.8z" fill="currentColor" />
                              </svg>
                            </button>
                            <button
                              type="button"
                              className="session"
                              title={task.title}
                              onClick={() => {
                                selectTask(task.id);
                                setOpenTasks((current) => (current.includes(task.id) ? current : [...current, task.id]));
                              }}
                            >
                              <StatusBadge badge={taskMark.badge} label={taskMark.label} />
                              <strong>{task.title || "無題"}</strong>
                            </button>
                            {!taskActive(task) && (
                              <button type="button" className="icon" aria-label="削除" onClick={() => setPendingTaskDelete(task.id)}>
                                ×
                              </button>
                            )}
                          </li>
                          {expanded &&
                            task.runs.map((run) => {
                              const who = agentName(agents, run.agentId);
                              const what = run.title || "無題";
                              const nodeOn = logTarget?.nodeId === run.id;
                              return (
                                <li key={run.id} className={nodeOn ? "tree-row node on" : "tree-row node"}>
                                  <button
                                    type="button"
                                    className="session"
                                    title={`${who} · ${what}`}
                                    onClick={() => {
                                      selectTask(task.id);
                                      openLog({ nodeId: run.id, title: `${who} · ${what}` });
                                    }}
                                  >
                                    <StatusBadge badge={runBadge(run.status)} label={RUN_STATUS_LABEL[run.status]} />
                                    <span className="node-who">[{who}]</span>
                                    <span className="node-what">{what}</span>
                                  </button>
                                </li>
                              );
                            })}
                        </Fragment>
                      );
                    })}
                  </ul>
                </div>
              </div>
            );
          })}
        </div>
        <div className="rail-foot">
          <button type="button" className="settings-open" aria-expanded={settingsOpen} onClick={() => setSettingsOpen((isOpen) => !isOpen)}>
            <GearIcon />
            設定
            {pipeFailed && <i className="dot" />}
          </button>
        </div>
      </aside>

      <header className="chrome">
        <div className="chrome-drag" data-tauri-drag-region onDoubleClick={() => windowAct("toggleMaximize")}>
          <h2>{openTitle}</h2>
        </div>
        <button
          type="button"
          className="pane-toggle"
          aria-expanded={contextOpen}
          aria-label={contextOpen ? "詳細を畳む" : "詳細を開く"}
          onClick={() => setContext(!contextOpen)}
        >
          <PaneGlyph />
        </button>
        <div className="window-controls">
          <button type="button" aria-label="最小化" onClick={() => windowAct("minimize")}><CaptionGlyph kind="min" /></button>
          <button type="button" aria-label="最大化" onClick={() => windowAct("toggleMaximize")}><CaptionGlyph kind="max" /></button>
          <button type="button" className="close" aria-label="閉じる" onClick={() => windowAct("close")}><CaptionGlyph kind="close" /></button>
        </div>
      </header>

      <main className="stage">
        <div className="sheet">
          {error && (
            <p className="banner warn">
              {error}
              <button type="button" onClick={() => setError("")}>閉じる</button>
            </p>
          )}
          {notice && (
            <p className="banner">
              {notice}
              <button type="button" onClick={() => setNotice("")}>閉じる</button>
            </p>
          )}

          <div className="transcript-wrap">
            <div
              className="transcript"
              ref={transcriptRef}
              onScroll={(event) => {
                const box = event.currentTarget;
                stickRef.current = box.scrollHeight - box.scrollTop - box.clientHeight < 80;
              }}
            >
              {viewTask && (
                <TaskTimeline
                  task={viewTask}
                  agents={agents}
                  busy={hubBusy}
                  restoreBlocked={restoreBlocked || hubBusy}
                  onResume={resumeTask}
                  onClose={closeTask}
                  onOpenLog={openLog}
                  onCancel={cancelJob}
                  onKill={killJob}
                  onRestore={askRestore}
                />
              )}
            </div>
            {!folder && !viewTask && (
              <div className="welcome">
                <p className="welcome-prompt">どのプロジェクトを見ますか？</p>
                <ProjectPicker folder={folder} folders={projects} onChoose={chooseFolder} onBrowse={() => void addProject()} />
              </div>
            )}
          </div>

          {logTarget && (
            <LogDrawer
              target={logTarget}
              lines={(logView === "brief" ? orch.briefs : orch.logs)[logTarget.nodeId] ?? []}
              view={logView}
              onView={chooseLogView}
              onClose={closeLog}
            />
          )}

          <div className="composer-dock">
            <div className="status-bar">
              <div className="meta-left">
                <QuietSelect
                  label="Grok Bot"
                  value={gatewayValue}
                  align="left"
                  disabled={hubBusy || pipeFailed}
                  choices={
                    pipeFailed
                      ? [{ value: "failed", label: "パイプ失敗" }]
                      : [
                          { value: "on", label: viewProject && !projectAllowed ? "受付中（未許可）" : "受付中" },
                          { value: "off", label: "受付停止" },
                        ]
                  }
                  onChange={(value) => void toggleAccepting(value === "on")}
                />
                {live && (
                  <button
                    type="button"
                    className="flow-indicator"
                    data-status="running"
                    title={live.run.title || live.task.title}
                    onClick={() => selectTask(live.task.id)}
                  >
                    <StatusBadge badge="spin" label={RUN_STATUS_LABEL.running} />
                    {agentName(agents, live.run.agentId)}
                    {waiting > 0 && <span className="flow-turns">+{waiting}</span>}
                  </button>
                )}
              </div>
              <span className="composer-spacer" />
              {viewTask && (
                <span className="flow-turns" title="このタスクのジョブの数と上限">
                  {viewTask.dispatches} / {viewTask.dispatchLimit}
                </span>
              )}
              <button
                type="button"
                className="loop-stop"
                disabled={stopping || !busyAnywhere}
                title="実行中のジョブを止め、待ちを取り消し、Grok Bot の受付を止めます"
                onClick={() => void stopAll()}
              >
                <span className="stop-mark" aria-hidden="true" />
                緊急停止
              </button>
            </div>
          </div>
        </div>
      </main>

      <aside className="context" aria-hidden={!contextOpen} inert={!contextOpen}>
        <div className="context-body">
          <section className="side-card">
            <div className="side-head">
              <h2>プロジェクト</h2>
              <strong title={viewProject}>{viewProject ? baseName(viewProject) : "—"}</strong>
            </div>
            {viewProject && (
              <dl>
                <div><dt>パス</dt><dd title={viewProject}>{viewProject}</dd></div>
                <div>
                  <dt>git</dt>
                  <dd className={repoProblem ? "warn" : ""}>{!repo ? "確認中" : repoProblem || repo.branch || "ブランチなし"}</dd>
                </div>
                <div><dt>Grok Bot</dt><dd className={projectAllowed ? "" : "warn"}>{projectAllowed ? "許可" : "未許可"}</dd></div>
                <div>
                  <dt>受付</dt>
                  <dd className={pipeFailed ? "warn" : ""}>{pipeFailed ? "パイプ失敗" : gateway.accepting ? "受付中" : "受付停止"}</dd>
                </div>
              </dl>
            )}
            {viewProject && repoProblem && (
              <button className="pill" type="button" disabled={hubBusy} onClick={() => setPendingInit(viewProject)}>
                {repo && !repo.isGit ? "git を初期化" : "最初のコミットを作る"}
              </button>
            )}
            {viewProject && !projectAllowed && (
              <button
                className="pill"
                type="button"
                onClick={() => {
                  setSettingsPage("gateway");
                  setSettingsOpen(true);
                }}
              >
                許可を設定
              </button>
            )}
          </section>
          {viewTask && (
            <section className="side-card">
              <div className="side-head">
                <h2>タスク</h2>
                <strong>{TASK_STATUS_LABEL[viewTask.status]}</strong>
              </div>
              <dl>
                <div><dt>出した側</dt><dd>{SOURCE_LABEL[viewTask.source]}</dd></div>
                <div><dt>ジョブ</dt><dd>{viewTask.dispatches} / {viewTask.dispatchLimit}</dd></div>
                <div><dt>待ち</dt><dd>{viewTask.runs.filter((run) => run.status === "queued").length}</dd></div>
                <div><dt>更新</dt><dd>{new Date(viewTask.updatedAt).toLocaleString()}</dd></div>
              </dl>
            </section>
          )}
          {projectRejections.length > 0 && (
            <section className="side-card">
              <div className="side-head">
                <h2>最近の拒否</h2>
              </div>
              <ul className="gateway-rejections">
                {projectRejections.map((item) => (
                  <li key={`${item.at}:${item.code}`}>
                    <span className="flow-meta">{new Date(item.at).toLocaleTimeString()}</span>
                    <code>{item.agent}</code>
                    <span>{REJECTION_LABEL[item.code] ?? item.code}</span>
                    {item.code === "project_paused" && (
                      <button type="button" className="flow-log" disabled={hubBusy} onClick={() => void resumeProject(item.project)}>
                        再開
                      </button>
                    )}
                  </li>
                ))}
              </ul>
            </section>
          )}
        </div>
      </aside>

      {settingsOpen && (
        <div className="modal" onMouseDown={() => setSettingsOpen(false)}>
          <section
            className="settings-window"
            role="dialog"
            aria-label="設定"
            onMouseDown={(event) => event.stopPropagation()}
          >
            <header className="settings-bar">
              <h2>設定</h2>
              <button type="button" aria-label="閉じる" onClick={() => setSettingsOpen(false)}>×</button>
            </header>
            <div className="settings-body">
              <div className="settings-nav" role="tablist" aria-label="設定の分類">
                {SETTINGS_PAGES.map((page) => {
                  const selected = settingsPage === page.id;
                  return (
                    <button
                      key={page.id}
                      type="button"
                      role="tab"
                      aria-selected={selected}
                      className={selected ? "on" : ""}
                      onClick={() => setSettingsPage(page.id)}
                    >
                      {page.label}
                    </button>
                  );
                })}
              </div>
              <div className="settings-pane" role="tabpanel">
                {settingsPage === "display" && (
                  <div className="settings-frame">
                    <h3>画面アニメーション</h3>
                    <button
                      type="button"
                      role="checkbox"
                      aria-checked={motion}
                      className="view-option"
                      onClick={() => setMotionEnabled(!motion)}
                    >
                      <span className="quiet-check" aria-hidden="true">{motion ? "✓" : ""}</span>
                      開閉の動きを出す
                    </button>
                  </div>
                )}
                {settingsPage === "agents" && <AgentSettings registry={registry} onChange={setRegistry} />}
                {settingsPage === "gateway" && (
                  <GatewaySettings
                    folders={projects}
                    live={gateway}
                    agents={agents}
                    onSaved={(view) => setAllowed(view.allowedProjects)}
                  />
                )}
                {settingsPage === "app" && (
                  <>
                    <h3>このアプリ</h3>
                    <p className="settings-copy">版 {appVersion || "…"}</p>
                    <button type="button" className="pill" disabled={updateBusy} onClick={() => void checkUpdate()}>
                      新しい版を確認
                    </button>
                    {updateNote && <p className="settings-copy">{updateNote}</p>}
                    {import.meta.env.DEV && (
                      <p className="note">開発中の起動では適用できません。インストールした版で使ってください。</p>
                    )}
                    {hasUpdate && !import.meta.env.DEV && (
                      <button type="button" className="solid" disabled={updateBusy} onClick={() => setPendingApply(true)}>
                        最新の版を適用
                      </button>
                    )}
                  </>
                )}
              </div>
            </div>
          </section>
        </div>
      )}

      {pendingTaskDelete && (
        <div className="modal">
          <section>
            <h3>このタスクの記録を削除しますか</h3>
            <p>ジョブの結果とログを消します。プロジェクトのファイルと、各 CLI のセッションは残ります。</p>
            <div className="modal-actions">
              <button type="button" onClick={() => setPendingTaskDelete(null)}>やめる</button>
              <button className="solid danger" type="button" onClick={() => deleteTask(pendingTaskDelete)}>
                削除する
              </button>
            </div>
          </section>
        </div>
      )}
      {pendingRestore && (
        <div className="modal">
          <section>
            <h3>作業前の状態に戻しますか</h3>
            {pendingRestore.files.length === 0 ? (
              <p>{baseName(pendingRestore.project)} で戻すファイルはありません。</p>
            ) : (
              <>
                {pendingRestore.changed && <p className="warn-text">確認のあいだにファイルが変わりました。いまの一覧で確かめてください。</p>}
                <p>
                  {baseName(pendingRestore.project)} の {pendingRestore.files.length} 件のファイルを、このジョブの前の内容で上書きします。
                  いまの内容は失われます。あとから作られたファイルは消しません。
                </p>
                <ul className="restore-files">
                  {pendingRestore.files.map((file) => {
                    const change = FILE_CHANGE_MARK[file.change as keyof typeof FILE_CHANGE_MARK];
                    return (
                      <li key={file.path}>
                        <span className="file-mark" title={change?.label ?? file.change}>{change?.mark ?? "?"}</span>
                        <span className="file-path">{file.path}</span>
                      </li>
                    );
                  })}
                </ul>
              </>
            )}
            <div className="modal-actions">
              <button type="button" onClick={() => setPendingRestore(null)}>やめる</button>
              {pendingRestore.files.length > 0 && (
                <button
                  className="solid danger"
                  type="button"
                  disabled={hubBusy || projectBusy(pendingRestore.project)}
                  title={projectBusy(pendingRestore.project) ? "このプロジェクトでジョブが動いているあいだは戻せません" : undefined}
                  onClick={() => void confirmRestore()}
                >
                  戻す
                </button>
              )}
            </div>
          </section>
        </div>
      )}
      {pendingInit && (
        <div className="modal">
          <section>
            <h3>{baseName(pendingInit)} でジョブを受けられるようにしますか</h3>
            <p>
              git のリポジトリでなければ作り、空の最初のコミットを作ります。ファイルはコミットしません。
              ジョブはコミットがあるリポジトリでしか動かないため、その前提だけを満たします。
            </p>
            <div className="modal-actions">
              <button type="button" onClick={() => setPendingInit(null)}>やめる</button>
              <button className="solid" type="button" onClick={() => void confirmInit()}>
                作る
              </button>
            </div>
          </section>
        </div>
      )}
      {pendingApply && (
        <div className="modal">
          <section>
            <h3>最新の版をインストールしますか</h3>
            <p>最新の版をインストールすると、このアプリは終了します。タスクの記録は残ります。</p>
            <div className="modal-actions">
              <button type="button" onClick={() => setPendingApply(false)}>やめる</button>
              <button className="solid" type="button" onClick={() => void confirmApply()}>インストールする</button>
            </div>
          </section>
        </div>
      )}
    </div>
  );
}

function ProjectPicker({
  folder,
  folders,
  onChoose,
  onBrowse,
}: {
  folder: string;
  folders: string[];
  onChoose: (path: string) => void;
  onBrowse: () => void;
}) {
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    function onPointer(event: MouseEvent) {
      const target = event.target;
      if (target instanceof Node && rootRef.current?.contains(target)) return;
      setOpen(false);
    }
    function onKey(event: KeyboardEvent) {
      if (event.key === "Escape") setOpen(false);
    }
    window.addEventListener("mousedown", onPointer);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onPointer);
      window.removeEventListener("keydown", onKey);
    };
  }, [open]);
  return (
    <div className="project-pick" ref={rootRef}>
      <button
        type="button"
        className="project-pick-button"
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => {
          if (folders.length === 0) {
            onBrowse();
            return;
          }
          setOpen((current) => !current);
        }}
      >
        <span>{folder ? baseName(folder) : "プロジェクトを選択"}</span>
        <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
          <path d="M2.4 4.4 6 8l3.6-3.6" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
      </button>
      {open && (
        <ul className="project-pick-menu" role="listbox">
          {folders.map((path) => {
            const selected = path === folder;
            return (
              <li key={path}>
                <button
                  type="button"
                  role="option"
                  aria-selected={selected}
                  className="project-pick-item"
                  title={path}
                  onClick={() => {
                    setOpen(false);
                    if (!selected) onChoose(path);
                  }}
                >
                  <span className="quiet-check" aria-hidden="true">{selected ? "✓" : ""}</span>
                  <span className="project-pick-name">{baseName(path)}</span>
                </button>
              </li>
            );
          })}
          <li>
            <button
              type="button"
              className="project-pick-item"
              onClick={() => {
                setOpen(false);
                onBrowse();
              }}
            >
              <span className="quiet-check" aria-hidden="true" />
              <span>新規プロジェクト</span>
            </button>
          </li>
        </ul>
      )}
    </div>
  );
}

function QuietSelect({
  label,
  value,
  choices,
  disabled,
  align,
  onChange,
}: {
  label: string;
  value: string;
  choices: { value: string; label: string }[];
  disabled: boolean;
  align: "left" | "right";
  onChange: (value: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  const current = choices.find((choice) => choice.value === value);
  useEffect(() => {
    if (!open) return;
    function onPointer(event: MouseEvent) {
      const target = event.target;
      if (target instanceof Node && rootRef.current?.contains(target)) return;
      setOpen(false);
    }
    function onKey(event: KeyboardEvent) {
      if (event.key === "Escape") setOpen(false);
    }
    window.addEventListener("mousedown", onPointer);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onPointer);
      window.removeEventListener("keydown", onKey);
    };
  }, [open]);
  return (
    <div className={`quiet-select ${align}`} ref={rootRef}>
      <span>{label}</span>
      <button
        type="button"
        className="quiet-select-button"
        disabled={disabled}
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => setOpen((currentOpen) => !currentOpen)}
      >
        {current?.label || value || "—"}
      </button>
      {open && !disabled && (
        <ul className="quiet-menu" role="listbox">
          {choices.map((choice) => {
            const selected = choice.value === value;
            return (
              <li key={choice.value}>
                <button
                  type="button"
                  role="option"
                  aria-selected={selected}
                  className="quiet-menu-item"
                  onClick={() => {
                    setOpen(false);
                    if (!selected) onChange(choice.value);
                  }}
                >
                  <span className="quiet-check" aria-hidden="true">{selected ? "✓" : ""}</span>
                  {choice.label}
                </button>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}

function CaptionGlyph({ kind }: { kind: "min" | "max" | "close" }) {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
      {kind === "min" && <path d="M3.5 8h9" fill="none" stroke="currentColor" strokeWidth="1.15" />}
      {kind === "max" && <rect x="3.5" y="3.5" width="9" height="9" fill="none" stroke="currentColor" strokeWidth="1.15" />}
      {kind === "close" && (
        <path d="M4.2 4.2 11.8 11.8M11.8 4.2 4.2 11.8" fill="none" stroke="currentColor" strokeWidth="1.15" />
      )}
    </svg>
  );
}

function PaneGlyph() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
      <rect x="3.5" y="3.5" width="9" height="9" rx="1.2" fill="none" stroke="currentColor" strokeWidth="1.15" />
      <path d="M9.5 3.5v9" fill="none" stroke="currentColor" strokeWidth="1.15" />
      <g className="pane-chevron">
        <path
          d="M5.35 6.35 7.15 8 5.35 9.65"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.15"
          strokeLinecap="round"
          strokeLinejoin="round"
        />
      </g>
    </svg>
  );
}

function FolderIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
      <path d="M2.4 4.2h3.7l1.2 1.35h6.3v6.25H2.4V4.2z" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinejoin="round" />
    </svg>
  );
}

function SearchIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true">
      <circle cx="7" cy="7" r="4.2" fill="none" stroke="currentColor" strokeWidth="1.4" />
      <path d="M10.4 10.4 13 13" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" />
    </svg>
  );
}

function GearIcon() {
  return (
    <svg width="15" height="15" viewBox="0 0 24 24" aria-hidden="true">
      <path
        fill="currentColor"
        d="M19.14 12.94c.04-.31.06-.63.06-.94s-.02-.63-.06-.94l2.03-1.58a.5.5 0 0 0 .12-.64l-1.92-3.32a.5.5 0 0 0-.6-.22l-2.39.96a7.1 7.1 0 0 0-1.63-.94l-.36-2.54A.5.5 0 0 0 13.9 2h-3.8a.5.5 0 0 0-.49.42l-.36 2.54c-.58.22-1.13.54-1.63.94l-2.39-.96a.5.5 0 0 0-.6.22L2.71 8.48a.5.5 0 0 0 .12.64l2.03 1.58c-.04.31-.06.63-.06.94s.02.63.06.94l-2.03 1.58a.5.5 0 0 0-.12.64l1.92 3.32c.13.22.39.31.6.22l2.39-.96c.5.4 1.05.72 1.63.94l.36 2.54c.05.24.25.42.49.42h3.8c.24 0 .44-.18.49-.42l.36-2.54c.58-.22 1.13-.54 1.63-.94l2.39.96c.22.09.47 0 .6-.22l1.92-3.32a.5.5 0 0 0-.12-.64l-2.03-1.58zM12 15.5A3.5 3.5 0 1 1 12 8.5a3.5 3.5 0 0 1 0 7z"
      />
    </svg>
  );
}
