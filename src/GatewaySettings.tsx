import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { type GatewayState, type GatewayView, REJECTION_LABEL, sameProject } from "./orchestra";

function errorText(error: unknown) {
  if (error instanceof Error && error.message) return error.message;
  return typeof error === "string" ? error : String(error);
}

function baseName(path: string) {
  return path.split(/[\\/]/).filter(Boolean).pop() || path;
}

function statusText(view: GatewayView | null, live: GatewayState) {
  const status = live.status ?? view?.status ?? null;
  if (!status) return "確認中";
  if (status.state === "starting") return "パイプを準備しています";
  if (status.state === "failed") return `パイプを開けませんでした: ${status.error}`;
  return "パイプで待ち受けています";
}

function usageText(cliPath: string, project: string) {
  const job = JSON.stringify({ project: project || "C:\\path\\to\\project", agent: "codex", prompt: "…", title: "…" });
  return [
    `$gbd = "${cliPath}"`,
    "& $gbd version",
    "& $gbd agents",
    "& $gbd submit job.json",
    "& $gbd wait <job_id> --timeout 600",
    "& $gbd result <job_id>",
    "& $gbd log <job_id> --tail 50",
    "& $gbd cancel <job_id>",
    "",
    "# job.json（続きは task_id を足す）",
    job,
    "",
    "# 日本語が化けるときは --ascii、詳しくは & $gbd help",
  ].join("\n");
}

type Draft = { accepting: boolean; allowedProjects: string[] };

function toDraft(view: GatewayView): Draft {
  return { accepting: view.accepting, allowedProjects: view.allowedProjects };
}

function sameDraft(draft: Draft, view: GatewayView) {
  return (
    draft.accepting === view.accepting &&
    draft.allowedProjects.length === view.allowedProjects.length &&
    draft.allowedProjects.every((path) => view.allowedProjects.some((item) => sameProject(item, path)))
  );
}

export function GatewaySettings({ folders, live }: { folders: string[]; live: GatewayState }) {
  const [view, setView] = useState<GatewayView | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const viewRef = useRef<GatewayView | null>(null);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState("");
  const [selftest, setSelftest] = useState("");
  const [copied, setCopied] = useState("");

  const liveState = live.status?.state ?? "";
  useEffect(() => {
    let cancelled = false;
    void invoke<GatewayView>("gateway_get")
      .then((next) => {
        if (cancelled) return;
        const previous = viewRef.current;
        viewRef.current = next;
        setView(next);
        // A change made elsewhere (the meta row, an emergency stop) replaces an untouched draft.
        setDraft((current) => (!current || !previous || sameDraft(current, previous) ? toDraft(next) : current));
      })
      .catch((error) => {
        if (!cancelled) setNote(errorText(error));
      });
    return () => {
      cancelled = true;
    };
  }, [liveState, live.accepting]);

  const choices = useMemo(() => {
    const list: string[] = [];
    for (const path of [...(draft?.allowedProjects ?? []), ...folders]) {
      if (path && !list.some((item) => sameProject(item, path))) list.push(path);
    }
    return list;
  }, [draft, folders]);

  if (!view || !draft) return <p className="note">{note || "読み込み中"}</p>;
  const dirty = !sameDraft(draft, view);
  const cliPath = view.cliPath ?? "";

  function toggleProject(path: string) {
    setDraft((current) => {
      if (!current) return current;
      const on = current.allowedProjects.some((item) => sameProject(item, path));
      return {
        ...current,
        allowedProjects: on
          ? current.allowedProjects.filter((item) => !sameProject(item, path))
          : [...current.allowedProjects, path],
      };
    });
    setNote("");
  }

  async function addFolder() {
    const picked = await open({ directory: true, multiple: false, title: "Grok Bot に許可するフォルダ" });
    if (typeof picked !== "string") return;
    setDraft((current) =>
      current && !current.allowedProjects.some((item) => sameProject(item, picked))
        ? { ...current, allowedProjects: [...current.allowedProjects, picked] }
        : current,
    );
  }

  async function save() {
    if (!draft) return;
    setBusy(true);
    try {
      const next = await invoke<GatewayView>("gateway_save", {
        accepting: draft.accepting,
        allowedProjects: draft.allowedProjects,
      });
      viewRef.current = next;
      setView(next);
      setDraft(toDraft(next));
      setNote("保存しました");
    } catch (error) {
      setNote(errorText(error));
    } finally {
      setBusy(false);
    }
  }

  async function runSelftest() {
    setBusy(true);
    setSelftest("");
    try {
      const result = await invoke<Record<string, unknown>>("gateway_selftest");
      setSelftest(JSON.stringify(result, null, 2));
    } catch (error) {
      setSelftest(errorText(error));
    } finally {
      setBusy(false);
    }
  }

  function copy(label: string, text: string) {
    void navigator.clipboard
      .writeText(text)
      .then(() => setCopied(label))
      .catch(() => setCopied(""));
  }

  async function resumeProject(project: string) {
    try {
      await invoke("orchestrator_resume_project", { project });
      setNote(`${baseName(project)} の連続実行の数を戻しました`);
    } catch (error) {
      setNote(errorText(error));
    }
  }

  return (
    <>
      <fieldset className="plain-fieldset" disabled={busy}>
        <div className="settings-frame">
          <h3>Grok Bot の受付</h3>
          <button
            type="button"
            role="checkbox"
            aria-checked={draft.accepting}
            className="view-option"
            onClick={() => setDraft((current) => (current ? { ...current, accepting: !current.accepting } : current))}
          >
            <span className="quiet-check" aria-hidden="true">{draft.accepting ? "✓" : ""}</span>
            受け付ける
          </button>
          <p className={liveState === "failed" ? "agent-probe warn" : "agent-probe"}>{statusText(view, live)}</p>
        </div>

        <div className="settings-frame">
          <h3>許可するフォルダ</h3>
          {choices.length === 0 && <p className="note">まだありません</p>}
          {choices.map((path) => {
            const on = draft.allowedProjects.some((item) => sameProject(item, path));
            return (
              <button
                key={path}
                type="button"
                role="checkbox"
                aria-checked={on}
                className="view-option gateway-folder"
                title={path}
                onClick={() => toggleProject(path)}
              >
                <span className="quiet-check" aria-hidden="true">{on ? "✓" : ""}</span>
                <strong>{baseName(path)}</strong>
                <code>{path}</code>
              </button>
            );
          })}
          <button type="button" className="pill" onClick={() => void addFolder()}>
            フォルダを追加
          </button>
        </div>

        <div className="agent-actions">
          <button type="button" className="solid" disabled={!dirty} onClick={() => void save()}>
            保存
          </button>
          <button
            type="button"
            className="pill"
            disabled={!dirty}
            onClick={() => setDraft(toDraft(view))}
          >
            取り消し
          </button>
        </div>
        {note && <p className="settings-copy">{note}</p>}

        <div className="settings-frame gateway-cli">
          <h3>gbd.exe</h3>
          {cliPath ? (
            <>
              <code className="gateway-path">{cliPath}</code>
              <pre className="gateway-usage">{usageText(cliPath, view.allowedProjects[0] ?? "")}</pre>
              <div className="agent-actions">
                <button type="button" className="pill" onClick={() => copy("usage", usageText(cliPath, view.allowedProjects[0] ?? ""))}>
                  {copied === "usage" ? "コピーしました" : "使い方をコピー"}
                </button>
                <button type="button" className="pill" onClick={() => void runSelftest()}>
                  自己診断
                </button>
              </div>
            </>
          ) : (
            <p className="agent-probe warn">アプリの隣に gbd.exe が見つかりません</p>
          )}
          {view.pipe && <p className="agent-probe">{view.pipe}</p>}
          {selftest && <pre className="gateway-usage">{selftest}</pre>}
        </div>
      </fieldset>

      {live.rejections.length > 0 && (
        <div className="settings-frame">
          <h3>最近の拒否</h3>
          <ul className="gateway-rejections">
            {live.rejections.map((item) => (
              <li key={`${item.at}:${item.code}:${item.project}`}>
                <span className="flow-meta">{new Date(item.at).toLocaleTimeString()}</span>
                <span title={item.project}>{baseName(item.project)}</span>
                <code>{item.agent}</code>
                <span>{REJECTION_LABEL[item.code] ?? item.code}</span>
                {item.code === "project_paused" && (
                  <button type="button" className="flow-log" onClick={() => void resumeProject(item.project)}>
                    再開
                  </button>
                )}
              </li>
            ))}
          </ul>
        </div>
      )}
    </>
  );
}
