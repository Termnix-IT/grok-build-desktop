import { memo, useEffect, useLayoutEffect, useRef, useState } from "react";
import { MarkdownBody } from "./MarkdownBody";
import {
  type AgentSpec,
  type Badge,
  type LogLine,
  type RunRecord,
  type TaskRecord,
  ERROR_KIND_LABEL,
  FILE_CHANGE_MARK,
  RUN_STATUS_LABEL,
  SOURCE_LABEL,
  TASK_STATUS_LABEL,
  agentKind,
  agentName,
  formatSeconds,
  runBadge,
} from "./orchestra";

export interface LogTarget {
  nodeId: string;
  title: string;
}

const BADGE_LABEL: Record<Badge, string> = {
  spin: "実行中",
  wait: "待機中",
  done: "完了",
  error: "エラー",
  ask: "要確認",
  stop: "停止",
};

export function StatusBadge({ badge, label }: { badge: Badge; label?: string }) {
  return <span className={`badge ${badge}`} role="img" aria-label={label ?? BADGE_LABEL[badge]} title={label ?? BADGE_LABEL[badge]} />;
}

export interface JobActions {
  onOpenLog: (target: LogTarget) => void;
  onCancel: (jobId: string) => void;
  onKill: (jobId: string) => void;
}

export function TaskTimeline({
  task,
  agents,
  busy,
  onResume,
  onClose,
  onOpenLog,
  onCancel,
  onKill,
}: JobActions & {
  task: TaskRecord;
  agents: AgentSpec[];
  busy: boolean;
  onResume: (taskId: string) => void;
  onClose: (taskId: string) => void;
}) {
  return (
    <div className="flow">
      <header className="flow-task">
        <span className={`source-chip ${task.source}`}>{SOURCE_LABEL[task.source]}</span>
        <span className="flow-meta">{TASK_STATUS_LABEL[task.status]}</span>
        <span className="flow-meta" title="このタスクのジョブの数と上限">
          {task.dispatches} / {task.dispatchLimit}
        </span>
        <span className="flow-path" title={task.project}>{task.project}</span>
        {task.status !== "closed" && (
          <button type="button" className="flow-log" disabled={busy} onClick={() => onClose(task.id)}>
            閉じる
          </button>
        )}
      </header>
      {task.runs.map((run) => (
        <JobCard key={run.id} run={run} agents={agents} onOpenLog={onOpenLog} onCancel={onCancel} onKill={onKill} />
      ))}
      {task.status === "paused" && (
        <article className="notice flow-stop">
          <strong>{TASK_STATUS_LABEL.paused}</strong>
          {task.pauseReason && <p className="md-plain">{task.pauseReason}</p>}
          <div className="flow-actions">
            <button type="button" className="solid" disabled={busy} onClick={() => onResume(task.id)}>
              再開
            </button>
          </div>
        </article>
      )}
      {task.status === "closed" && (
        <article className="notice flow-stop">
          <strong>{TASK_STATUS_LABEL.closed}</strong>
        </article>
      )}
    </div>
  );
}

const JobCard = memo(function JobCard({
  run,
  agents,
  ...actions
}: JobActions & {
  run: RunRecord;
  agents: AgentSpec[];
}) {
  const who = agentName(agents, run.agentId);
  const seconds =
    run.startedAt && run.endedAt ? Math.max(0, Math.round((run.endedAt - run.startedAt) / 1000)) : null;
  const errorText = run.error?.message?.trim() ?? "";
  const showError = Boolean(run.error) && errorText !== run.summary.trim();
  const failed = run.status === "failed" || run.status === "timedOut";
  return (
    <article className={failed ? "flow-card fail" : "flow-card"} data-status={run.status}>
      <header className="flow-head">
        <StatusBadge badge={runBadge(run.status)} label={RUN_STATUS_LABEL[run.status]} />
        <span className={`agent-chip ${agentKind(agents, run.agentId)}`}>{who}</span>
        <strong title={run.title}>{run.title || "無題"}</strong>
        <span className={`source-chip ${run.source}`}>{SOURCE_LABEL[run.source]}</span>
        <button
          type="button"
          className="flow-log"
          onClick={() => actions.onOpenLog({ nodeId: run.id, title: `${who} · ${run.title || run.id}` })}
        >
          ログ
        </button>
      </header>
      <div className="flow-status">
        <span>{RUN_STATUS_LABEL[run.status]}</span>
        {typeof run.exitCode === "number" && <span className="flow-meta">終了コード {run.exitCode}</span>}
        {seconds !== null && <span className="flow-meta">{formatSeconds(seconds)}</span>}
        {run.status === "running" && (
          <span className="flow-actions">
            <button type="button" className="pill" onClick={() => actions.onCancel(run.id)}>
              止める
            </button>
            <button type="button" className="pill danger" onClick={() => actions.onKill(run.id)}>
              緊急停止
            </button>
          </span>
        )}
        {run.status === "queued" && (
          <span className="flow-actions">
            <button type="button" className="pill" onClick={() => actions.onCancel(run.id)}>
              取り消す
            </button>
          </span>
        )}
      </div>
      <details className="flow-prompt">
        <summary>プロンプト</summary>
        <pre>{run.prompt}</pre>
      </details>
      {run.summary && (
        <div className="flow-result">
          <MarkdownBody text={run.summary} />
        </div>
      )}
      {showError && run.error && (
        <p className="flow-error">
          {ERROR_KIND_LABEL[run.error.kind]}
          {errorText ? `: ${errorText}` : ""}
        </p>
      )}
      {run.denied.length > 0 && (
        <section className="flow-part">
          <h4>拒否された操作</h4>
          <ul className="flow-denied">
            {run.denied.map((item, index) => (
              <li key={index}>{item}</li>
            ))}
          </ul>
        </section>
      )}
      {run.changedFiles.length > 0 && (
        <details className="flow-prompt" open={run.changedFiles.length <= 12}>
          <summary>変更したファイル {run.changedFiles.length} 件</summary>
          <ul className="flow-files">
            {run.changedFiles.map((file) => {
              const change = FILE_CHANGE_MARK[file.change] ?? { mark: "?", label: file.change };
              return (
                <li key={`${file.change}:${file.path}`} className={`path ${file.change}`}>
                  <span className="file-mark" title={change.label}>{change.mark}</span>
                  <span className="file-path">{file.path}</span>
                  {(typeof file.added === "number" || typeof file.deleted === "number") && (
                    <span className="file-count">
                      {typeof file.added === "number" && <span className="plus">+{file.added}</span>}
                      {typeof file.deleted === "number" && <span className="minus">-{file.deleted}</span>}
                    </span>
                  )}
                </li>
              );
            })}
          </ul>
        </details>
      )}
      {run.diffStat && <pre className="flow-diff">{lastLine(run.diffStat)}</pre>}
      {run.truncated && <p className="flow-meta">差分の一部を省略しました</p>}
      {run.recoveryPoint && <RecoveryPoint hash={run.recoveryPoint} />}
    </article>
  );
});

function RecoveryPoint({ hash }: { hash: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <p className="flow-recovery">
      <span className="flow-meta">作業前</span>
      <code title={hash}>{hash.slice(0, 12)}</code>
      <button
        type="button"
        className="flow-log"
        onClick={() => {
          void navigator.clipboard
            .writeText(hash)
            .then(() => setCopied(true))
            .catch(() => setCopied(false));
        }}
      >
        {copied ? "コピーしました" : "コピー"}
      </button>
    </p>
  );
}

function lastLine(text: string) {
  const lines = text.trimEnd().split(/\r?\n/);
  return (lines[lines.length - 1] ?? "").trim();
}

export function LogDrawer({
  target,
  lines,
  onClose,
}: {
  target: LogTarget;
  lines: LogLine[];
  onClose: () => void;
}) {
  const boxRef = useRef<HTMLDivElement>(null);
  const [follow, setFollow] = useState(true);
  const followRef = useRef(true);
  followRef.current = follow;

  useEffect(() => {
    setFollow(true);
  }, [target.nodeId]);

  useLayoutEffect(() => {
    const box = boxRef.current;
    if (box && followRef.current) box.scrollTop = box.scrollHeight;
  }, [lines, follow]);

  useEffect(() => {
    function onKey(event: KeyboardEvent) {
      if (event.key === "Escape") onClose();
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <section className="log-drawer" aria-label="ログ">
      <header className="log-head">
        <strong>{target.title}</strong>
        <span className="flow-meta">{lines.length}行</span>
        <button
          type="button"
          role="checkbox"
          aria-checked={follow}
          className="view-option log-follow"
          onClick={() => setFollow((current) => !current)}
        >
          <span className="quiet-check" aria-hidden="true">{follow ? "✓" : ""}</span>
          追従
        </button>
        <button type="button" className="log-close" aria-label="閉じる" onClick={onClose}>×</button>
      </header>
      <div
        className="log-lines"
        ref={boxRef}
        onScroll={(event) => {
          const box = event.currentTarget;
          const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 24;
          if (!atBottom && followRef.current) setFollow(false);
        }}
      >
        {lines.length === 0 ? (
          <p className="log-empty">まだありません</p>
        ) : (
          lines.map((line) => (
            <div key={line.seq} className={`log-line ${line.stream}`}>
              {line.text || " "}
            </div>
          ))
        )}
      </div>
    </section>
  );
}
