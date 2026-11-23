import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { type AgentSpec, type GatewayState, type GatewayView, REJECTION_LABEL, sameProject } from "./orchestra";

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

/** A PowerShell single-quoted literal, so `$` and backticks in a path are not expanded. */
function psLiteral(text: string) {
  return `'${text.replace(/'/g, "''")}'`;
}

function usageText(cliPath: string, project: string) {
  const job = JSON.stringify({ project: project || "C:\\path\\to\\project", agent: "codex", prompt: "…", title: "…" });
  return [
    `$gbd = ${psLiteral(cliPath)}`,
    "& $gbd version",
    "& $gbd agents",
    "& $gbd submit job.json",
    "& $gbd status <job_id>",
    "& $gbd wait <job_id> --timeout 600",
    "& $gbd result <job_id>",
    "& $gbd log <job_id> --tail 50",
    "& $gbd log <job_id> --tail 50 --raw",
    "& $gbd cancel <job_id>",
    "",
    "# job.json（続きは task_id を足す）",
    job,
    "",
    "# 日本語が化けるときは --ascii、詳しくは & $gbd help",
  ].join("\n");
}

/** The text a user pastes into Grok Bot, filled in with this machine's gbd path, folders and agents. */
function instructionText(cliPath: string, projects: string[], agents: AgentSpec[]) {
  const folders = projects.length ? projects.map((path) => `- ${path}`) : ["- （まだありません。アプリの設定の「連携」で足します）"];
  const enabled = agents.filter((agent) => agent.enabled);
  const workers = enabled.length
    ? enabled.map((agent) => `- ${agent.id}（${agent.name}）: ${agent.role.trim() || "役割の説明なし"}`)
    : ["- （有効なエージェントがありません。& $gbd agents で確かめます）"];
  return [
    "あなたは Grok Build Desktop の司令塔です。作業はあなたが自分で書かず、下の gbd.exe で登録したエージェントに 1 つずつ頼みます。",
    "gbd.exe は 1 回の実行で 1 つの JSON を標準出力に返します。PowerShell で次のように呼びます。",
    "",
    `$gbd = ${psLiteral(cliPath)}`,
    "& $gbd version            # アプリにつながるかを確かめる",
    "& $gbd agents             # 使えるエージェントと役割",
    "& $gbd submit job.json    # ジョブを登録する。job_id と task_id が返る",
    "& $gbd wait <job_id> --timeout 1200",
    "& $gbd result <job_id>",
    "& $gbd log <job_id> --tail 50        # 読みやすい形のログ",
    "& $gbd log <job_id> --tail 50 --raw  # CLI の出力そのまま",
    "& $gbd cancel <job_id>",
    "",
    "job.json は UTF-8 で、次の形です。同じ作業の続きは、前の結果の task_id を足して同じタスクに積みます。",
    JSON.stringify({ project: projects[0] || "C:\\path\\to\\repo", agent: enabled[0]?.id || "claude", title: "短い題", prompt: "頼む内容" }),
    "",
    "ジョブを出せるフォルダ:",
    ...folders,
    "",
    "エージェント:",
    ...workers,
    "",
    "結果の読み方:",
    "- 成否は status と error で判断します。exit_code が 0 でも失敗していることがあります。status は queued、running、succeeded、failed、timed_out、killed、cancelled のどれかです。",
    "- summary が答え、denied が拒否された操作、changed_files と diff_stat が変更です。",
    "- recovery_point はツリーの ID で、git checkout には使えません。戻すときは recovery_command のコマンドを使うか、ユーザーにアプリの「作業前に戻す」を頼みます。",
    "- ジョブはすべてのフォルダを通して 1 つずつ順に動きます。同時に出しても待ち行列に並ぶだけです。",
    "",
    "断られたとき（error.code）:",
    "- not_accepting: ユーザーが受付を止めています。再開を頼み、勝手に出し直しません。",
    "- project_not_allowed: 上のフォルダ以外です。ユーザーに許可を頼みます。",
    "- not_git_repo: git のリポジトリでないかコミットがありません。ユーザーにアプリで初期化を頼みます。",
    "- project_paused、task_paused、task_limit: 上限か一時停止です。ユーザーの判断を待ちます。",
    "- queue_full: 待ちが減ってから出します。",
    "- app_not_running: アプリが起動していません。ユーザーに起動を頼みます。",
    "",
    "何をするか、いつ終えるかはあなたが決めます。削除や公開のように戻せない操作は、ジョブに頼む前にユーザーに確かめます。",
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

export function GatewaySettings({
  folders,
  live,
  agents,
  onSaved,
}: {
  folders: string[];
  live: GatewayState;
  agents: AgentSpec[];
  onSaved: (view: GatewayView) => void;
}) {
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
      onSaved(next);
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
                <button
                  type="button"
                  className="pill"
                  onClick={() => copy("brief", instructionText(cliPath, view.allowedProjects, agents))}
                >
                  {copied === "brief" ? "コピーしました" : "Grok Bot への指示をコピー"}
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
