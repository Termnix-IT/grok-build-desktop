import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import {
  type DeskState,
  type DiffLine,
  type PermissionRequest,
  type ToolPayload,
  type TranscriptItem,
  freePercent,
  initialDesk,
  lineDiff,
  mergeSnapshot,
  modeChoices,
  reduceDesk,
  statusLabel,
  toolLabel,
  toolLooksLikePlan,
} from "./model";
import "./App.css";

const RECENT_KEY = "gbd-recent-folders";
const LAST_KEY = "gbd-last-folder";
const COLLAPSED_KEY = "gbd-collapsed-tools";

function readList(key: string) {
  try {
    const raw = JSON.parse(localStorage.getItem(key) ?? "[]");
    return Array.isArray(raw) ? raw.filter((item): item is string => typeof item === "string") : [];
  } catch {
    return [];
  }
}

function rememberFolder(path: string) {
  const recent = [path, ...readList(RECENT_KEY).filter((item) => item !== path)].slice(0, 8);
  localStorage.setItem(RECENT_KEY, JSON.stringify(recent));
  localStorage.setItem(LAST_KEY, path);
  return recent;
}

export default function App() {
  const [desk, setDesk] = useState<DeskState>(initialDesk);
  const [folder, setFolder] = useState("");
  const [recent, setRecent] = useState<string[]>(() => readList(RECENT_KEY));
  const [draft, setDraft] = useState("");
  const [files, setFiles] = useState<string[]>([]);
  const [suggestions, setSuggestions] = useState<string[]>([]);
  const [collapsed, setCollapsed] = useState<string[]>(() => readList(COLLAPSED_KEY));
  const [pendingLoad, setPendingLoad] = useState<{ id: string; warning: string } | null>(null);
  const [pendingDelete, setPendingDelete] = useState<string | null>(null);
  const transcriptRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const stickRef = useRef(true);

  useEffect(() => {
    const unlisten = listen<Record<string, unknown>>("agent-event", (event) => {
      setDesk((current) => reduceDesk(current, event.payload));
    });
    return () => {
      void unlisten.then((stop) => stop());
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    void invoke<Record<string, unknown>>("app_snapshot")
      .then((snapshot) => {
        if (!cancelled && snapshot) setDesk((current) => mergeSnapshot(current, snapshot));
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    const last = localStorage.getItem(LAST_KEY);
    if (last) void chooseFolder(last, false);
    // The restored folder is applied once, when the window opens.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const box = transcriptRef.current;
    if (!box || !stickRef.current) return;
    box.scrollTop = box.scrollHeight;
  }, [desk.items, desk.permission, desk.busy]);

  useEffect(() => {
    const field = inputRef.current;
    if (!field) return;
    field.style.height = "0px";
    field.style.height = `${Math.min(180, Math.max(48, field.scrollHeight))}px`;
  }, [draft, desk.sessionId]);

  const atMatch = draft.match(/(^|\s)@([^\s@]*)$/);
  const atQuery = atMatch?.[2] ?? null;

  useEffect(() => {
    if (atQuery === null || !folder) {
      setSuggestions([]);
      return;
    }
    let cancelled = false;
    const timer = window.setTimeout(() => {
      void invoke<string[]>("search_files", { query: atQuery })
        .then((paths) => {
          if (!cancelled) setSuggestions(paths);
        })
        .catch(() => {
          if (!cancelled) setSuggestions([]);
        });
    }, 120);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [atQuery, folder]);

  const gauge = freePercent(desk.usage, desk.contextTokens);
  const usedPercent = gauge ? Math.max(0, 100 - gauge.percent) : 0;
  const modes = useMemo(() => modeChoices(desk), [desk]);
  const planEntries = [...desk.items].reverse().find((item) => item.kind === "plan");

  async function chooseFolder(path: string, store = true) {
    setFolder(path);
    if (store) setRecent(rememberFolder(path));
    try {
      await invoke("set_folder", { path });
      setDesk((current) => ({ ...current, error: "", auth: false }));
    } catch (error) {
      setDesk((current) => ({ ...current, error: String(error) }));
    }
  }

  async function browseFolder() {
    const picked = await open({ directory: true, multiple: false, title: "プロジェクトフォルダ" });
    if (typeof picked === "string") await chooseFolder(picked);
  }

  async function send() {
    const text = draft.trim();
    if (!text || desk.busy) return;
    const attached = files;
    setDraft("");
    setFiles([]);
    setSuggestions([]);
    stickRef.current = true;
    setDesk((current) => ({
      ...current,
      error: "",
      busy: true,
      items: [
        ...current.items,
        {
          id: `local-user-${current.items.length}`,
          kind: "text",
          role: "user",
          text: attached.length ? `${text}\n\n${attached.map((file) => `添付: ${file}`).join("\n")}` : text,
        },
      ],
    }));
    try {
      await invoke("prompt", { text, files: attached });
    } catch (error) {
      setDesk((current) => ({ ...current, error: String(error), busy: false }));
    }
  }

  async function openSession(id: string) {
    const warning = await invoke<string | null>("session_owner", { id });
    if (warning) {
      setPendingLoad({ id, warning });
      return;
    }
    await load(id);
  }

  async function load(id: string) {
    setPendingLoad(null);
    stickRef.current = true;
    try {
      await invoke("load_session", { id });
    } catch (error) {
      setDesk((current) => ({ ...current, error: String(error), loading: false }));
    }
  }

  function attach(path: string) {
    setFiles((current) => (current.includes(path) ? current : [...current, path]));
    setDraft((current) => current.replace(/(^|\s)@[^\s@]*$/, "$1").trimEnd());
    setSuggestions([]);
    inputRef.current?.focus();
  }

  function insertAt() {
    setDraft((current) => {
      if (/(^|\s)@$/.test(current)) return current;
      const gap = current.length === 0 || /\s$/.test(current) ? "" : " ";
      return `${current}${gap}@`;
    });
    inputRef.current?.focus();
  }

  function toggleTool(id: string) {
    setCollapsed((current) => {
      const next = current.includes(id) ? current.filter((item) => item !== id) : [...current, id];
      localStorage.setItem(COLLAPSED_KEY, JSON.stringify(next));
      return next;
    });
  }

  const folderName = folder ? folder.split(/[\\/]/).filter(Boolean).pop() : "";

  return (
    <div className="desk">
      <aside className="rail">
        <div className="mark">
          <span className="logo-mark" aria-hidden="true">G</span>
          <div>
            <p className="product">Grok Build</p>
            <p className="status">
              <i className={desk.ready ? "dot on" : "dot"} />
              {desk.ready ? "接続済み" : "起動しています"}
            </p>
          </div>
        </div>
        <button className="folder-pick" type="button" onClick={() => void browseFolder()}>
          <span>フォルダを選ぶ</span>
          <small>{folderName || "プロジェクトを開く"}</small>
        </button>
        <div className="recent">
          {recent.map((path) => (
            <button key={path} type="button" className={path === folder ? "recent on" : "recent"} onClick={() => void chooseFolder(path)}>
              {path.split(/[\\/]/).filter(Boolean).pop()}
            </button>
          ))}
        </div>
        <div className="session-head">
          <span>このフォルダ</span>
          <button type="button" disabled={!folder || desk.busy} onClick={() => void invoke("new_session").catch((error) => setDesk((current) => ({ ...current, error: String(error) })))}>
            新しい会話
          </button>
        </div>
        <ul className="sessions">
          {desk.sessions.map((session) => (
            <li key={session.sessionId} className={session.sessionId === desk.sessionId ? "session-row on" : "session-row"}>
              <button
                type="button"
                className="session"
                onClick={() => void openSession(session.sessionId)}
              >
                <strong>{session.title || "無題"}</strong>
                <small>{session.updatedAt ? session.updatedAt.slice(0, 16).replace("T", " ") : session.sessionId.slice(0, 8)}</small>
              </button>
              <button type="button" className="icon" aria-label="削除" onClick={() => setPendingDelete(session.sessionId)}>
                ×
              </button>
            </li>
          ))}
        </ul>
        {desk.sessionId && (
          <button type="button" className="quiet" disabled={desk.busy} onClick={() => void invoke("close_session")}>
            この会話を閉じる
          </button>
        )}
      </aside>

      <main className="stage">
        <header className="stage-bar">
          <h2>{desk.sessions.find((session) => session.sessionId === desk.sessionId)?.title || (desk.sessionId ? "無題の会話" : "会話はまだ開いていません")}</h2>
          <p className="conn">
            <i className={desk.ready ? "dot on" : "dot"} />
            {desk.ready ? desk.agentVersion || "接続済み" : "起動しています"}
          </p>
        </header>

        <div className="sheet">
          {desk.warning && <p className="banner warn">{desk.warning} ファイルの巻き戻しはしません。</p>}
          {desk.banner && <p className="banner">{desk.banner}</p>}
          {desk.error && (
            <p className="banner warn">
              {desk.error}
              {desk.auth && (
                <button type="button" onClick={() => void invoke("login")}>ログイン</button>
              )}
            </p>
          )}

          <div
            className="transcript"
            ref={transcriptRef}
            onScroll={(event) => {
              const box = event.currentTarget;
              stickRef.current = box.scrollHeight - box.scrollTop - box.clientHeight < 80;
            }}
          >
            {!folder && (
              <section className="empty">
                <p className="eyebrow">作業フォルダ</p>
                <h3>どのプロジェクトで会話するか選んでください。</h3>
                <p>会話の正本は Grok のセッションに残ります。このアプリはそれを写し取りません。</p>
                <button className="solid" type="button" onClick={() => void browseFolder()}>フォルダを選ぶ</button>
              </section>
            )}
            {folder && !desk.sessionId && !desk.loading && (
              <section className="empty">
                <h3>{folderName} で新しい会話を始められます。</h3>
                <p>左の一覧から過去の会話を開くこともできます。開いているターミナルのセッションは、警告を確認してから開いてください。</p>
              </section>
            )}
            {desk.items.map((item) => (
              <Article
                key={item.id}
                item={item}
                collapsed={collapsed.includes(item.kind === "tool" ? item.toolId : "")}
                onToggle={toggleTool}
              />
            ))}
            {desk.loading && <p className="pending">会話を読み込んでいます…</p>}
            {desk.busy && <p className="pending">応答しています…</p>}
          </div>

          {desk.permission && (
            <PermissionCard
              request={desk.permission}
              plan={planEntries?.kind === "plan" ? planEntries.entries : []}
              onDecide={(optionId) => {
                const id = desk.permission?.id;
                setDesk((current) => ({ ...current, permission: null }));
                if (id === undefined) return;
                void invoke("decide_permission", { id, optionId }).catch((error) =>
                  setDesk((current) => ({ ...current, error: String(error) })),
                );
              }}
            />
          )}

          <div className="composer-dock">
            <form
              className="composer"
              onSubmit={(event) => {
                event.preventDefault();
                void send();
              }}
            >
              {files.length > 0 && (
                <div className="chips">
                  {files.map((file) => (
                    <button key={file} type="button" onClick={() => setFiles((current) => current.filter((item) => item !== file))}>
                      {file.split(/[\\/]/).pop()} ×
                    </button>
                  ))}
                </div>
              )}
              {suggestions.length > 0 && (
                <ul className="suggest">
                  {suggestions.map((path) => (
                    <li key={path}>
                      <button type="button" onClick={() => attach(path)}>{path}</button>
                    </li>
                  ))}
                </ul>
              )}
              <textarea
                ref={inputRef}
                value={draft}
                placeholder={desk.sessionId ? "メッセージを入力" : "先に会話を開いてください"}
                disabled={!desk.sessionId || desk.loading}
                onChange={(event) => setDraft(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter" && !event.shiftKey) {
                    event.preventDefault();
                    if (suggestions.length > 0 && atQuery !== null) {
                      attach(suggestions[0]);
                      return;
                    }
                    void send();
                  }
                }}
              />
              <div className="composer-bar">
                <button type="button" className="orb ghost" aria-label="ファイルを添付" disabled={!desk.sessionId || desk.loading} onClick={insertAt}>
                  +
                </button>
                <span className="composer-hint">{draft.trim() ? `${draft.trim().length} 字` : "Enter で送信、Shift+Enter で改行"}</span>
                {desk.busy ? (
                  <button type="button" className="orb stop" aria-label="停止" onClick={() => void invoke("cancel_turn")}>
                    <span className="stop-mark" />
                  </button>
                ) : (
                  <button className="orb send" type="submit" aria-label="送信" disabled={!desk.sessionId || !draft.trim()}>
                    <SendIcon />
                  </button>
                )}
              </div>
            </form>
            <div className="composer-meta">
              <label className="meta-select">
                <span>{desk.modeConfirmed ? "権限" : "未確認"}</span>
                <select
                  value={desk.modeId}
                  disabled={!desk.sessionId || desk.busy}
                  onChange={(event) => {
                    const modeId = event.target.value;
                    void invoke("set_mode", { modeId }).catch((error) => setDesk((current) => ({ ...current, error: String(error) })));
                  }}
                >
                  {modes.map((mode) => (
                    <option key={mode.id} value={mode.id}>{mode.label}</option>
                  ))}
                </select>
              </label>
              <div className="meta-configs">
                {desk.configOptions.filter((option) => option.type === "select").map((option) => (
                  <label key={option.id} className="meta-select">
                    <span>{option.name}</span>
                    <select
                      value={option.currentValue}
                      disabled={!desk.sessionId || desk.busy}
                      onChange={(event) => void invoke("set_config", { id: option.id, value: event.target.value }).catch((error) => setDesk((current) => ({ ...current, error: String(error) })))}
                    >
                      {option.choices.map((choice) => (
                        <option key={choice.value} value={choice.value}>{choice.name}</option>
                      ))}
                      {option.currentValue && !option.choices.some((choice) => choice.value === option.currentValue) && (
                        <option value={option.currentValue}>{option.currentValue}</option>
                      )}
                    </select>
                  </label>
                ))}
              </div>
            </div>
          </div>
        </div>
      </main>

      <aside className="context">
        <section className="side-card">
          <div className="side-head">
            <h2>コンテキスト</h2>
            <strong>{gauge ? `${usedPercent}%` : "—"}</strong>
          </div>
          <div
            className="meter"
            role="meter"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={usedPercent}
            aria-label={gauge ? `使用 ${usedPercent}%、空き ${gauge.percent}%` : "未計測"}
          >
            <span style={{ width: `${usedPercent}%` }} />
          </div>
          {gauge ? (
            <dl>
              <div><dt>空き</dt><dd>{gauge.percent}% · {formatTokens(gauge.free)}</dd></div>
              <div><dt>使用</dt><dd>{formatTokens(gauge.used)}</dd></div>
              <div><dt>ウィンドウ</dt><dd>{formatTokens(gauge.size)}</dd></div>
            </dl>
          ) : (
            <p className="note">ターンが終わると、入力トークンとモデルのウィンドウから空きを出します。</p>
          )}
          {desk.usage?.outputTokens !== undefined && (
            <p className="note">出力 {formatTokens(desk.usage.outputTokens)}
              {desk.usage.cachedReadTokens ? ` / キャッシュ読取 ${formatTokens(desk.usage.cachedReadTokens)}` : ""}
              {desk.usage.reasoningTokens ? ` / 推論 ${formatTokens(desk.usage.reasoningTokens)}` : ""}
            </p>
          )}
        </section>

        <section className="side-card">
          <div className="side-head">
            <h2>セッション</h2>
          </div>
          <p className="note">巻き戻しは ACP では公開されていません。ここからファイルは戻りません。圧縮は Grok が内部で行うことがあり、結果が ACP に出ないときはその旨を表示します。</p>
          <button
            className="pill"
            type="button"
            disabled={!desk.sessionId || desk.busy}
            onClick={() => void invoke("compact_session").catch((error) => setDesk((current) => ({ ...current, error: String(error) })))}
          >
            圧縮を頼む
          </button>
          {desk.model && <p className="note model-line">{desk.model}</p>}
        </section>

        {desk.commands.length > 0 && (
          <section className="side-card">
            <div className="side-head">
              <h2>コマンド</h2>
            </div>
            <ul className="commands">
              {desk.commands.slice(0, 12).map((command) => (
                <li key={command.name}><code>/{command.name}</code> {command.description}</li>
              ))}
            </ul>
          </section>
        )}
      </aside>

      {pendingLoad && (
        <div className="modal">
          <section>
            <h3>別のプロセスが開いている可能性があります</h3>
            <p>{pendingLoad.warning}</p>
            <div>
              <button type="button" onClick={() => setPendingLoad(null)}>やめる</button>
              <button className="solid" type="button" onClick={() => void load(pendingLoad.id)}>それでも開く</button>
            </div>
          </section>
        </div>
      )}
      {pendingDelete && (
        <div className="modal">
          <section>
            <h3>この会話を削除しますか</h3>
            <p>Grok のセッション履歴から消します。このアプリは写しを持っていません。</p>
            <div>
              <button type="button" onClick={() => setPendingDelete(null)}>やめる</button>
              <button
                className="solid danger"
                type="button"
                onClick={() => {
                  const id = pendingDelete;
                  setPendingDelete(null);
                  void invoke("delete_session", { id }).catch((error) => setDesk((current) => ({ ...current, error: String(error) })));
                }}
              >
                削除する
              </button>
            </div>
          </section>
        </div>
      )}
    </div>
  );
}

function Article({ item, collapsed, onToggle }: { item: TranscriptItem; collapsed: boolean; onToggle: (id: string) => void }) {
  if (item.kind === "text" && item.role === "user") {
    return <article className="bubble user"><MarkdownBody text={item.text} /></article>;
  }
  if (item.kind === "text" && item.role === "assistant") {
    return <article className="bubble assistant"><MarkdownBody text={item.text} /></article>;
  }
  if (item.kind === "text" && item.role === "thought") {
    return (
      <details className="thought">
        <summary>思考</summary>
        <MarkdownBody text={item.text} />
      </details>
    );
  }
  if (item.kind === "tool") {
    return <ToolCard tool={item.tool} collapsed={collapsed} onToggle={() => onToggle(item.toolId)} />;
  }
  if (item.kind === "plan") {
    return (
      <article className="plan">
        <h3>プラン</h3>
        <ol>
          {item.entries.map((entry, index) => (
            <li key={`${entry.content}-${index}`} data-status={entry.status}>
              <span>{statusLabel(entry.status)}</span>
              {entry.content}
            </li>
          ))}
        </ol>
      </article>
    );
  }
  if (item.kind === "notice") {
    return <article className="notice"><strong>{item.title}</strong><p>{item.body}</p></article>;
  }
  if (item.kind === "compaction") {
    return <article className="notice"><strong>{item.text}</strong></article>;
  }
  return (
    <details className="unknown">
      <summary>未対応の更新</summary>
      <pre>{item.text}</pre>
    </details>
  );
}

function ToolCard({ tool, collapsed, onToggle }: { tool: ToolPayload; collapsed: boolean; onToggle: () => void }) {
  const content = Array.isArray(tool.content) ? tool.content : [];
  return (
    <article className="tool">
      <button type="button" className="tool-head" onClick={onToggle}>
        <span>{toolLabel(tool)}</span>
        <strong>{tool.title || tool.name || "ツール"}</strong>
        <em>{statusLabel(tool.status)}</em>
      </button>
      {!collapsed && (
        <div className="tool-body">
          {tool.locations?.map((location, index) => (
            <p key={`${location.path}-${index}`} className="path">{location.path}{location.line ? `:${location.line}` : ""}</p>
          ))}
          {content.map((block, index) => <ToolBlock key={index} block={block} />)}
          {tool.rawInput !== undefined && (
            <details>
              <summary>入力</summary>
              <pre>{JSON.stringify(tool.rawInput, null, 2)}</pre>
            </details>
          )}
        </div>
      )}
    </article>
  );
}

function ToolBlock({ block }: { block: unknown }) {
  if (!block || typeof block !== "object") return null;
  const row = block as Record<string, unknown>;
  if (row.type === "diff") {
    const path = typeof row.path === "string" ? row.path : "";
    const oldText = typeof row.oldText === "string" ? row.oldText : "";
    const newText = typeof row.newText === "string" ? row.newText : "";
    const lines: DiffLine[] = oldText ? lineDiff(oldText, newText) : newText.split("\n").map((text) => ({ kind: "add", text }));
    return (
      <div className="diff">
        <p className="path">{path}</p>
        <pre>
          {lines.map((line, index) => (
            <div key={index} className={line.kind}>{line.kind === "add" ? "+" : line.kind === "del" ? "-" : " "}{line.text}</div>
          ))}
        </pre>
      </div>
    );
  }
  const nested = row.content && typeof row.content === "object" ? (row.content as Record<string, unknown>).text : undefined;
  if (typeof nested === "string" && nested) return <pre className="tool-text">{nested}</pre>;
  return null;
}

function PermissionCard({
  request,
  plan,
  onDecide,
}: {
  request: PermissionRequest;
  plan: { content: string; status: string }[];
  onDecide: (optionId: string | null) => void;
}) {
  const large = toolLooksLikePlan(request.tool);
  return (
    <section className={large ? "permission large" : "permission"}>
      <p className="eyebrow">{large ? "プランの確認" : "許可が必要です"}</p>
      <h3>{request.tool.title || request.tool.name || "ツールの実行"}</h3>
      {large && plan.length > 0 && (
        <ol>
          {plan.map((entry, index) => <li key={index}>{entry.content}</li>)}
        </ol>
      )}
      <div className="choices">
        {request.options.map((option) => (
          <button
            key={option.optionId}
            type="button"
            className={option.kind.startsWith("reject") ? "danger" : "solid"}
            onClick={() => onDecide(option.optionId)}
          >
            {option.name}
          </button>
        ))}
        <button type="button" onClick={() => onDecide(null)}>中止</button>
      </div>
    </section>
  );
}

function MarkdownBody({ text }: { text: string }) {
  return <Markdown remarkPlugins={[remarkGfm]}>{text}</Markdown>;
}

function formatTokens(value: number) {
  return new Intl.NumberFormat("ja-JP").format(value);
}

function SendIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
      <path d="M8 12.2V3.8M8 3.8 4.4 7.4M8 3.8l3.6 3.6" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}
