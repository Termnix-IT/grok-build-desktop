import { memo, useEffect, useLayoutEffect, useRef, useState } from "react";
import { MarkdownBody } from "./MarkdownBody";
import {
  type AgentSpec,
  type Badge,
  type LogLine,
  type Planning,
  type RunRecord,
  type Step,
  type TaskRecord,
  NODE_STATUS_LABEL,
  TASK_STATUS_LABEL,
  agentName,
  nodeBadge,
} from "./orchestra";

export interface LogTarget {
  nodeId: string;
  title: string;
}

const BADGE_LABEL: Record<Badge, string> = {
  spin: "実行中",
  done: "完了",
  error: "エラー",
  ask: "要確認",
  stop: "停止",
};

export function StatusBadge({ badge, label }: { badge: Badge; label?: string }) {
  return <span className={`badge ${badge}`} role="img" aria-label={label ?? BADGE_LABEL[badge]} title={label ?? BADGE_LABEL[badge]} />;
}

export function FlowTimeline({
  task,
  agents,
  planning,
  onOpenLog,
}: {
  task: TaskRecord;
  agents: AgentSpec[];
  planning: Planning | null;
  onOpenLog: (target: LogTarget) => void;
}) {
  const pendingPlan = planning && !task.steps.some((step) => step.id === planning.stepId) ? planning : null;
  const stopped = task.status === "paused" || task.status === "error" || task.status === "aborted";
  return (
    <div className="flow">
      <article className="bubble user">
        <MarkdownBody text={task.goal} />
      </article>
      {task.steps.map((step) => (
        <StepCard key={step.id} step={step} agents={agents} onOpenLog={onOpenLog} />
      ))}
      {pendingPlan && (
        <ThinkingCard turn={pendingPlan.turn} onLog={() => onOpenLog({ nodeId: pendingPlan.nodeId, title: `Grok · 判断 ${pendingPlan.turn}` })} />
      )}
      {stopped && (
        <article className={task.status === "paused" ? "notice flow-stop" : "notice flow-stop warn"}>
          <strong>{TASK_STATUS_LABEL[task.status]}</strong>
          {task.reason && task.reason !== task.steps[task.steps.length - 1]?.error && <p className="md-plain">{task.reason}</p>}
        </article>
      )}
    </div>
  );
}

const StepCard = memo(function StepCard({
  step,
  agents,
  onOpenLog,
}: {
  step: Step;
  agents: AgentSpec[];
  onOpenLog: (target: LogTarget) => void;
}) {
  const decision = step.decision;
  const planLog = () => onOpenLog({ nodeId: step.planNodeId, title: `Grok · 判断 ${step.turn}` });
  if (!decision) {
    if (!step.error) return <ThinkingCard turn={step.turn} onLog={planLog} />;
    return (
      <article className="flow-card fail">
        <header className="flow-head">
          <StatusBadge badge="error" />
          <span className="agent-chip grok">Grok</span>
          <strong>判断の失敗</strong>
          <span className="flow-turn">{step.turn}</span>
          <button type="button" className="flow-log" onClick={planLog}>ログ</button>
        </header>
        <p className="flow-error">{step.error}</p>
      </article>
    );
  }
  const kind = decision.nextAction;
  const title =
    decision.stepTitle || (kind === "dispatch" ? "作業" : kind === "complete" ? "完了" : "質問");
  const target = kind === "dispatch" ? agents.find((agent) => agent.id === decision.targetAgent) : undefined;
  return (
    <>
      <article className={`flow-card ${kind}`}>
        <header className="flow-head">
          <span className="agent-chip grok">Grok</span>
          <strong>{title}</strong>
          <span className="flow-turn">{step.turn}</span>
          <button type="button" className="flow-log" onClick={planLog}>判断のログ</button>
        </header>
        <section className="flow-part">
          <h4>理由</h4>
          <MarkdownBody text={decision.reasoning} />
        </section>
        {kind === "dispatch" && (
          <>
            <div className="flow-route">
              <span className="flow-arrow" aria-hidden="true">→</span>
              <span className={`agent-chip ${target?.kind ?? "custom"}`}>{agentName(agents, decision.targetAgent)}</span>
            </div>
            {decision.convertedPrompt && (
              <details className="flow-prompt">
                <summary>プロンプト</summary>
                <pre>{decision.convertedPrompt}</pre>
              </details>
            )}
            {step.run && (
              <RunBlock
                run={step.run}
                onLog={() =>
                  onOpenLog({ nodeId: step.run!.nodeId, title: `${agentName(agents, step.run!.agentId)} · ${title}` })
                }
              />
            )}
          </>
        )}
        {kind === "complete" && decision.summary && (
          <section className="flow-part flow-summary">
            <h4>結果</h4>
            <MarkdownBody text={decision.summary} />
          </section>
        )}
        {kind === "askHuman" && decision.question && (
          <section className="flow-part flow-question">
            <h4>質問</h4>
            <MarkdownBody text={decision.question} />
          </section>
        )}
      </article>
      {step.reply && (
        <article className="bubble user">
          <MarkdownBody text={step.reply} />
        </article>
      )}
    </>
  );
});

function ThinkingCard({ turn, onLog }: { turn: number; onLog: () => void }) {
  return (
    <article className="flow-card thinking">
      <header className="flow-head">
        <StatusBadge badge="spin" label="判断中" />
        <span className="agent-chip grok">Grok</span>
        <strong>判断</strong>
        <span className="flow-turn">{turn}</span>
        <button type="button" className="flow-log" onClick={onLog}>ログ</button>
      </header>
    </article>
  );
}

function RunBlock({ run, onLog }: { run: RunRecord; onLog: () => void }) {
  const seconds = run.endedAt ? Math.max(0, Math.round((run.endedAt - run.startedAt) / 1000)) : null;
  return (
    <section className="flow-run" data-status={run.status}>
      <header className="flow-head">
        <StatusBadge badge={nodeBadge(run.status)} label={NODE_STATUS_LABEL[run.status]} />
        <strong>{NODE_STATUS_LABEL[run.status]}</strong>
        {typeof run.exitCode === "number" && <span className="flow-meta">終了コード {run.exitCode}</span>}
        {seconds !== null && <span className="flow-meta">{formatSeconds(seconds)}</span>}
        <button type="button" className="flow-log" onClick={onLog}>ログ</button>
      </header>
      {run.summary && (
        <div className="flow-result">
          <MarkdownBody text={run.summary} />
        </div>
      )}
      {run.diffStat && <pre className="flow-diff">{run.diffStat}</pre>}
      {run.untracked.length > 0 && (
        <ul className="flow-files">
          {run.untracked.map((path) => (
            <li key={path} className="path">+ {path}</li>
          ))}
        </ul>
      )}
    </section>
  );
}

function formatSeconds(total: number) {
  if (total < 60) return `${total}秒`;
  const minutes = Math.floor(total / 60);
  const seconds = total % 60;
  return seconds ? `${minutes}分${seconds}秒` : `${minutes}分`;
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
