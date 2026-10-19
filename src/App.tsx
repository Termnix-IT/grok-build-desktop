import { Fragment, useEffect, useMemo, useRef, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import { check, type Update } from "@tauri-apps/plugin-updater";
import Markdown from "react-markdown";
import remarkBreaks from "remark-breaks";
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

type LocalImage = { id: string; name: string; preview: string };
type StageRecord = {
  kind: string;
  id?: string | null;
  path?: string | null;
  name: string;
  preview?: string | null;
  message?: string | null;
};

function errorText(error: unknown) {
  if (error instanceof Error && error.message) return error.message;
  if (typeof error === "string") return error;
  return String(error);
}

function bytesToBase64(bytes: Uint8Array) {
  let binary = "";
  const chunk = 0x8000;
  for (let index = 0; index < bytes.length; index += chunk) {
    binary += String.fromCharCode(...bytes.subarray(index, index + chunk));
  }
  return btoa(binary);
}

function imageNameFor(file: File) {
  if (/\.(png|jpe?g|gif|webp)$/i.test(file.name)) return file.name;
  const ext =
    file.type === "image/png"
      ? "png"
      : file.type === "image/jpeg"
        ? "jpg"
        : file.type === "image/gif"
          ? "gif"
          : file.type === "image/webp"
            ? "webp"
            : "";
  if (!ext) return "";
  const base = file.name.replace(/\.[^.]+$/, "") || "image";
  return `${base}.${ext}`;
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
  const [images, setImages] = useState<LocalImage[]>([]);
  const [suggestions, setSuggestions] = useState<string[]>([]);
  const [collapsed, setCollapsed] = useState<string[]>(() => readList(COLLAPSED_KEY));
  const [pendingLoad, setPendingLoad] = useState<{ id: string; warning: string } | null>(null);
  const [pendingDelete, setPendingDelete] = useState<string | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [settingsPos, setSettingsPos] = useState<{ left: number; bottom: number } | null>(null);
  const [accountUsage, setAccountUsage] = useState("");
  const [dragOver, setDragOver] = useState(false);
  const [appVersion, setAppVersion] = useState("");
  const [updateNote, setUpdateNote] = useState("");
  const [updateBusy, setUpdateBusy] = useState(false);
  const [hasUpdate, setHasUpdate] = useState(false);
  const [pendingApply, setPendingApply] = useState(false);
  const transcriptRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const stickRef = useRef(true);
  const sheetRef = useRef<HTMLDivElement>(null);
  const settingsButtonRef = useRef<HTMLButtonElement>(null);
  const settingsPopRef = useRef<HTMLDivElement>(null);
  const usageWatch = useRef(false);
  const usageBuf = useRef("");
  const pathDropAt = useRef(0);
  const updateRef = useRef<Update | null>(null);
  const sendingRef = useRef(false);
  const takePathsRef = useRef<(paths: string[]) => void>(() => {});
  const takeFilesRef = useRef<(dropped: File[]) => void>(() => {});

  useEffect(() => {
    const unlisten = listen<Record<string, unknown>>("agent-event", (event) => {
      const payload = event.payload;
      if (usageWatch.current) {
        if (payload.type === "chunk" && payload.role === "assistant" && typeof payload.text === "string") {
          usageBuf.current += payload.text;
        }
        if (payload.type === "notice" && typeof payload.description === "string") {
          usageBuf.current += payload.description;
        }
        if (payload.type === "turn" || payload.type === "error") {
          const text = usageBuf.current.trim();
          if (!(payload.type === "error" && !text)) {
            usageWatch.current = false;
            setAccountUsage(text || "ACP では使用量の内訳が返りません");
          }
        }
      }
      setDesk((current) => reduceDesk(current, payload));
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
    void getVersion().then(setAppVersion).catch(() => setAppVersion("不明"));
    return () => {
      void updateRef.current?.close();
    };
  }, []);

  useEffect(() => {
    if (!settingsOpen) return;
    function onPointer(event: MouseEvent) {
      const target = event.target;
      if (!(target instanceof Node)) return;
      if (settingsPopRef.current?.contains(target) || settingsButtonRef.current?.contains(target)) return;
      setSettingsOpen(false);
    }
    function onKey(event: KeyboardEvent) {
      if (event.key === "Escape") setSettingsOpen(false);
    }
    window.addEventListener("mousedown", onPointer);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onPointer);
      window.removeEventListener("keydown", onKey);
    };
  }, [settingsOpen]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    function pointInSheet(position: { x: number; y: number }) {
      const sheet = sheetRef.current;
      if (!sheet) return false;
      const ratio = window.devicePixelRatio || 1;
      const x = position.x / ratio;
      const y = position.y / ratio;
      const rect = sheet.getBoundingClientRect();
      return x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom;
    }
    void getCurrentWebview()
      .onDragDropEvent((event) => {
        const payload = event.payload;
        if (payload.type === "leave") {
          setDragOver(false);
          return;
        }
        if (payload.type === "enter" || payload.type === "over") {
          setDragOver(pointInSheet(payload.position));
          return;
        }
        setDragOver(false);
        if (!pointInSheet(payload.position) || payload.paths.length === 0) return;
        pathDropAt.current = Date.now();
        takePathsRef.current(payload.paths);
      })
      .then((stop) => {
        if (cancelled) stop();
        else unlisten = stop;
      })
      .catch(() => {});
    return () => {
      cancelled = true;
      unlisten?.();
    };
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

  function absorb(records: StageRecord[]) {
    const rejected: string[] = [];
    const nextFiles: string[] = [];
    const nextImages: LocalImage[] = [];
    for (const record of records) {
      if (record.kind === "image" && record.id && record.preview) {
        nextImages.push({ id: record.id, name: record.name, preview: record.preview });
      } else if (record.kind === "file" && record.path) {
        nextFiles.push(record.path);
      } else {
        rejected.push(record.message || `${record.name} は添付できません`);
      }
    }
    if (nextFiles.length) {
      setFiles((current) => {
        const merged = [...current];
        for (const path of nextFiles) {
          if (!merged.includes(path)) merged.push(path);
        }
        return merged;
      });
    }
    if (nextImages.length) setImages((current) => [...current, ...nextImages]);
    return rejected;
  }

  async function stagePaths(paths: string[]) {
    try {
      const records = await invoke<StageRecord[]>("stage_paths", { paths });
      const rejected = absorb(records);
      if (rejected.length) setDesk((current) => ({ ...current, error: rejected.join(" ") }));
    } catch (error) {
      setDesk((current) => ({ ...current, error: errorText(error) }));
    }
  }

  async function stageDroppedFiles(dropped: File[]) {
    const paths: string[] = [];
    const messages: string[] = [];
    const byteJobs: { name: string; dataBase64: string }[] = [];
    for (const file of dropped) {
      const withPath = file as File & { path?: string };
      const path = typeof withPath.path === "string" ? withPath.path : "";
      if (path) {
        paths.push(path);
        continue;
      }
      const name = imageNameFor(file);
      if (!name) {
        messages.push(`${file.name || "このファイル"} はパスがないため添付できません`);
        continue;
      }
      const bytes = new Uint8Array(await file.arrayBuffer());
      byteJobs.push({ name, dataBase64: bytesToBase64(bytes) });
    }
    if (paths.length) {
      try {
        messages.push(...absorb(await invoke<StageRecord[]>("stage_paths", { paths })));
      } catch (error) {
        messages.push(errorText(error));
      }
    }
    for (const job of byteJobs) {
      try {
        const record = await invoke<StageRecord>("stage_bytes", { name: job.name, dataBase64: job.dataBase64 });
        messages.push(...absorb([record]));
      } catch (error) {
        messages.push(errorText(error));
      }
    }
    if (messages.length) setDesk((current) => ({ ...current, error: messages.join(" ") }));
  }

  takePathsRef.current = (paths) => {
    void stagePaths(paths);
  };
  takeFilesRef.current = (dropped) => {
    void stageDroppedFiles(dropped);
  };

  function removeImage(id: string) {
    setImages((current) => current.filter((image) => image.id !== id));
    void invoke("unstage", { id }).catch(() => {});
  }

  async function submit(
    text: string,
    attached: string[],
    attachedImages: LocalImage[],
    options?: { preserveDraft?: boolean; watchUsage?: boolean },
  ) {
    const trimmed = text.trim();
    if (sendingRef.current || desk.busy) return;
    if (!trimmed && attachedImages.length === 0) return;
    sendingRef.current = true;
    if (!options?.preserveDraft) {
      setDraft("");
      setFiles([]);
      setImages([]);
      setSuggestions([]);
    }
    stickRef.current = true;
    if (options?.watchUsage) {
      usageBuf.current = "";
      usageWatch.current = true;
      setAccountUsage("確認しています…");
    }
    const lines = [
      ...attachedImages.map((image) => `添付: ${image.name}`),
      ...attached.map((file) => `添付: ${file}`),
    ];
    const shown = lines.length ? (trimmed ? `${trimmed}\n\n${lines.join("\n")}` : lines.join("\n")) : trimmed;
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
          text: shown,
          images: attachedImages.length
            ? attachedImages.map((image) => ({ id: image.id, name: image.name, preview: image.preview }))
            : undefined,
        },
      ],
    }));
    try {
      await invoke("prompt", {
        text: trimmed,
        files: attached,
        imageIds: attachedImages.map((image) => image.id),
      });
    } catch (error) {
      if (!options?.preserveDraft) {
        setDraft(text);
        setFiles(attached);
        setImages(attachedImages);
      }
      if (options?.watchUsage && usageWatch.current) {
        usageWatch.current = false;
        setAccountUsage(errorText(error));
      }
      setDesk((current) => ({ ...current, error: errorText(error), busy: false }));
    } finally {
      sendingRef.current = false;
    }
  }

  function send() {
    void submit(draft, files, images);
  }

  function askUsage() {
    const has = desk.commands.some((command) => command.name === "usage" || command.name === "cost");
    if (!has) {
      setAccountUsage("このセッションでは使用量コマンドがありません");
      return;
    }
    void submit("/usage", [], [], { preserveDraft: true, watchUsage: true });
  }

  function insertCommand(name: string) {
    setDraft((current) => {
      const token = `/${name} `;
      if (!current.trim()) return token;
      return current.endsWith(" ") || current.endsWith("\n") ? `${current}${token}` : `${current} ${token}`;
    });
    setSettingsOpen(false);
    inputRef.current?.focus();
  }

  function toggleSettings() {
    if (settingsOpen) {
      setSettingsOpen(false);
      return;
    }
    const rect = settingsButtonRef.current?.getBoundingClientRect();
    if (rect) setSettingsPos({ left: rect.left, bottom: window.innerHeight - rect.top + 8 });
    setSettingsOpen(true);
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
    } catch (error) {
      setUpdateNote(formatUpdateError(error));
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
    } catch (error) {
      setUpdateNote(formatUpdateError(error));
    } finally {
      setUpdateBusy(false);
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
        <div className="rail-foot">
          <button ref={settingsButtonRef} type="button" className="settings-open" aria-expanded={settingsOpen} onClick={toggleSettings}>
            <GearIcon />
            設定
          </button>
        </div>
      </aside>

      <main className="stage">
        <header className="stage-bar">
          <h2>{desk.sessions.find((session) => session.sessionId === desk.sessionId)?.title || (desk.sessionId ? "無題の会話" : "会話はまだ開いていません")}</h2>
          <p className="conn">
            <i className={desk.ready ? "dot on" : "dot"} />
            {desk.ready ? desk.agentVersion || "接続済み" : "起動しています"}
          </p>
        </header>

        <div
          className="sheet"
          ref={sheetRef}
          onDragOver={(event) => {
            if (![...event.dataTransfer.types].includes("Files")) return;
            event.preventDefault();
            setDragOver(true);
          }}
          onDragLeave={(event) => {
            const next = event.relatedTarget;
            if (next instanceof Node && event.currentTarget.contains(next)) return;
            setDragOver(false);
          }}
          onDrop={(event) => {
            event.preventDefault();
            setDragOver(false);
            const dropped = [...event.dataTransfer.files];
            window.setTimeout(() => {
              if (Date.now() - pathDropAt.current < 1000) return;
              takeFilesRef.current(dropped);
            }, 300);
          }}
        >
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
              className={dragOver ? "composer drag" : "composer"}
              onSubmit={(event) => {
                event.preventDefault();
                send();
              }}
            >
              {(images.length > 0 || files.length > 0) && (
                <div className="chips">
                  {images.map((image) => (
                    <button key={image.id} type="button" className="image-chip" onClick={() => removeImage(image.id)}>
                      <img src={image.preview} alt="" />
                      {image.name} ×
                    </button>
                  ))}
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
                  <button className="orb send" type="submit" aria-label="送信" disabled={!desk.sessionId || desk.loading || (!draft.trim() && images.length === 0)}>
                    <SendIcon />
                  </button>
                )}
              </div>
            </form>
            <div className="composer-meta">
              <QuietSelect
                label={desk.modeConfirmed ? "権限" : "未確認"}
                value={desk.modeId}
                align="left"
                disabled={!desk.sessionId || desk.busy}
                choices={modes.map((mode) => ({ value: mode.id, label: mode.label }))}
                onChange={(modeId) => {
                  void invoke("set_mode", { modeId }).catch((error) => setDesk((current) => ({ ...current, error: errorText(error) })));
                }}
              />
              <div className="meta-configs">
                {(() => {
                  const selects = desk.configOptions.filter((option) => option.type === "select");
                  const modelIndex = selects.findIndex(
                    (option) => option.name.toLowerCase() === "model" || option.id.toLowerCase().includes("model"),
                  );
                  const ring = <ContextRing gauge={gauge} usedPercent={usedPercent} />;
                  if (selects.length === 0) return ring;
                  return selects.map((option, index) => {
                    const choices = option.choices.map((choice) => ({ value: choice.value, label: choice.name }));
                    if (option.currentValue && !choices.some((choice) => choice.value === option.currentValue)) {
                      choices.push({ value: option.currentValue, label: option.currentValue });
                    }
                    return (
                      <Fragment key={option.id}>
                        {index === modelIndex && ring}
                        {modelIndex < 0 && index === 0 && ring}
                        <QuietSelect
                          label={option.name}
                          value={option.currentValue}
                          align="right"
                          disabled={!desk.sessionId || desk.busy}
                          choices={choices}
                          onChange={(value) => {
                            void invoke("set_config", { id: option.id, value }).catch((error) =>
                              setDesk((current) => ({ ...current, error: errorText(error) })),
                            );
                          }}
                        />
                      </Fragment>
                    );
                  });
                })()}
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
      </aside>

      {settingsOpen && settingsPos && (
        <div
          className="settings-pop"
          ref={settingsPopRef}
          style={{ left: settingsPos.left, bottom: settingsPos.bottom }}
          role="dialog"
          aria-label="設定"
        >
          <h3>Grok の使用量</h3>
          <button type="button" className="pill" disabled={!desk.sessionId || desk.busy || desk.loading} onClick={askUsage}>
            使用量を確認
          </button>
          {accountUsage && <p className="settings-copy">{accountUsage}</p>}
          <h3>コマンド</h3>
          {desk.commands.length === 0 ? (
            <p className="note">接続すると、使えるコマンドがここに出ます。</p>
          ) : (
            <ul className="settings-commands">
              {desk.commands.map((command) => (
                <li key={command.name}>
                  <button type="button" onClick={() => insertCommand(command.name)}>
                    <code>/{command.name}</code>
                    {command.description && <small>{command.description}</small>}
                  </button>
                </li>
              ))}
            </ul>
          )}
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
        </div>
      )}

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
      {pendingApply && (
        <div className="modal">
          <section>
            <h3>最新の版をインストールしますか</h3>
            <p>最新の版をインストールすると、このアプリは終了します。会話は Grok のセッションに残ります。</p>
            <div>
              <button type="button" onClick={() => setPendingApply(false)}>やめる</button>
              <button className="solid" type="button" onClick={() => void confirmApply()}>インストールする</button>
            </div>
          </section>
        </div>
      )}
    </div>
  );
}

function Article({ item, collapsed, onToggle }: { item: TranscriptItem; collapsed: boolean; onToggle: (id: string) => void }) {
  if (item.kind === "text" && item.role === "user") {
    return (
      <article className="bubble user">
        {item.images && item.images.length > 0 && (
          <div className="bubble-images">
            {item.images.map((image) => (
              <img key={image.id} src={image.preview} alt={image.name} />
            ))}
          </div>
        )}
        {item.text && <MarkdownBody text={item.text} />}
      </article>
    );
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
  return <Markdown remarkPlugins={[remarkGfm, remarkBreaks]}>{text}</Markdown>;
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

function ContextRing({
  gauge,
  usedPercent,
}: {
  gauge: ReturnType<typeof freePercent>;
  usedPercent: number;
}) {
  const radius = 14;
  const circumference = 2 * Math.PI * radius;
  const dash = gauge ? (Math.min(100, Math.max(0, usedPercent)) / 100) * circumference : 0;
  const label = gauge
    ? `使用 ${usedPercent}%、空き ${gauge.percent}%、ウィンドウ ${formatTokens(gauge.size)}`
    : "コンテキストは未計測";
  const tip = gauge
    ? `空き ${gauge.percent}% · 使用 ${formatTokens(gauge.used)} · ウィンドウ ${formatTokens(gauge.size)}`
    : "未計測";
  return (
    <span className="context-ring" title={tip} role="img" aria-label={label}>
      <svg width="34" height="34" viewBox="0 0 34 34" aria-hidden="true">
        <circle cx="17" cy="17" r={radius} fill="none" stroke="#3f3f3f" strokeWidth="3" />
        <circle
          cx="17"
          cy="17"
          r={radius}
          fill="none"
          stroke="#4c9aff"
          strokeWidth="3"
          strokeDasharray={`${dash} ${circumference}`}
          transform="rotate(-90 17 17)"
        />
      </svg>
      <span>{gauge ? usedPercent : "—"}</span>
    </span>
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
