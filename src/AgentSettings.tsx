import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  type AgentSpec,
  type Limits,
  type OutputFormat,
  type ProbeResult,
  type PromptVia,
  type Registry,
  OUTPUT_LABEL,
  PROMPT_VIA_LABEL,
  blankCustomAgent,
} from "./orchestra";

const PROMPT_VIAS: PromptVia[] = ["stdin", "arg", "file"];
const OUTPUTS: OutputFormat[] = ["text", "claudeStreamJson", "codexJsonl", "grokJson", "antigravityJson"];

type NumberLimit = Exclude<keyof Limits, "pauseOnDeletion">;

const NUMBER_FIELDS: { key: NumberLimit; label: string }[] = [
  { key: "maxDispatchesPerTask", label: "1 タスクのジョブ" },
  { key: "maxConsecutivePerProject", label: "Grok Bot の連続実行" },
  { key: "sameErrorLimit", label: "同じ失敗で止める回数" },
  { key: "defaultTimeoutSecs", label: "1 回の上限（秒）" },
  { key: "maxQueue", label: "待ちの数" },
  { key: "logTailBytes", label: "Grok Bot に返す stderr（バイト）" },
];

function errorText(error: unknown) {
  if (error instanceof Error && error.message) return error.message;
  return typeof error === "string" ? error : String(error);
}

export function AgentSettings({
  registry,
  onChange,
}: {
  registry: Registry | null;
  onChange: (registry: Registry) => void;
}) {
  const [draft, setDraft] = useState<Registry | null>(registry);
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const [probes, setProbes] = useState<Record<string, ProbeResult | "busy">>({});

  useEffect(() => {
    setDraft(registry);
  }, [registry]);

  if (!draft) return <p className="note">読み込み中</p>;
  const dirty = JSON.stringify(draft) !== JSON.stringify(registry);

  function patchLimits(change: Partial<Limits>) {
    setDraft((current) => (current ? { ...current, limits: { ...current.limits, ...change } } : current));
    setNote("");
  }

  function patchAgent(id: string, change: Partial<AgentSpec>) {
    setDraft((current) =>
      current
        ? { ...current, agents: current.agents.map((agent) => (agent.id === id ? { ...agent, ...change } : agent)) }
        : current,
    );
    setNote("");
  }

  async function save() {
    if (!draft) return;
    setBusy(true);
    try {
      const saved = await invoke<Registry>("agents_config_save", { registry: draft });
      onChange(saved);
      setNote("保存しました");
    } catch (error) {
      setNote(errorText(error));
    } finally {
      setBusy(false);
    }
  }

  async function reset() {
    setBusy(true);
    try {
      const saved = await invoke<Registry>("agents_config_reset");
      onChange(saved);
      setProbes({});
      setNote("既定に戻しました");
    } catch (error) {
      setNote(errorText(error));
    } finally {
      setBusy(false);
    }
  }

  async function probe(id: string) {
    setProbes((current) => ({ ...current, [id]: "busy" }));
    try {
      const result = await invoke<ProbeResult>("agents_probe", { id });
      setProbes((current) => ({ ...current, [id]: result }));
    } catch (error) {
      setProbes((current) => ({ ...current, [id]: { id, error: errorText(error) } }));
    }
  }

  return (
    // Editing stays locked while a save is pending, so no edit is overwritten by the saved copy.
    <fieldset className="plain-fieldset" disabled={busy}>
      <div className="settings-frame">
        <h3>上限</h3>
        <div className="agent-grid">
          {NUMBER_FIELDS.map((field) => (
            <label key={field.key} className="field">
              <span>{field.label}</span>
              <input
                type="number"
                inputMode="numeric"
                value={String(draft.limits[field.key])}
                onChange={(event) => patchLimits({ [field.key]: Number(event.target.value) } as Partial<Limits>)}
              />
            </label>
          ))}
        </div>
        <button
          type="button"
          role="checkbox"
          aria-checked={draft.limits.pauseOnDeletion}
          className="view-option"
          onClick={() => patchLimits({ pauseOnDeletion: !draft.limits.pauseOnDeletion })}
        >
          <span className="quiet-check" aria-hidden="true">{draft.limits.pauseOnDeletion ? "✓" : ""}</span>
          追跡中のファイルが消えたら一時停止
        </button>
      </div>

      {draft.agents.map((agent) => {
        const result = probes[agent.id];
        return (
          <div key={agent.id} className="settings-frame agent-frame">
            <div className="agent-top">
              <button
                type="button"
                role="checkbox"
                aria-checked={agent.enabled}
                className="view-option agent-toggle"
                onClick={() => patchAgent(agent.id, { enabled: !agent.enabled })}
              >
                <span className="quiet-check" aria-hidden="true">{agent.enabled ? "✓" : ""}</span>
                <strong>{agent.name || agent.id}</strong>
                <code>{agent.id}</code>
              </button>
              <button type="button" className="pill" disabled={result === "busy" || dirty} onClick={() => void probe(agent.id)}>
                確認
              </button>
              {agent.kind === "custom" && (
                <button
                  type="button"
                  className="pill"
                  onClick={() =>
                    setDraft((current) =>
                      current ? { ...current, agents: current.agents.filter((item) => item.id !== agent.id) } : current,
                    )
                  }
                >
                  削除
                </button>
              )}
            </div>
            {result && result !== "busy" && (
              <p className={result.error ? "agent-probe warn" : "agent-probe"}>
                {result.error ? result.error : `${result.resolved ?? ""}${result.version ? ` · ${result.version}` : ""}`}
              </p>
            )}
            <div className="agent-grid">
              <label className="field">
                <span>名前</span>
                <input value={agent.name} onChange={(event) => patchAgent(agent.id, { name: event.target.value })} />
              </label>
              <label className="field">
                <span>プログラム</span>
                <input
                  value={agent.program}
                  spellCheck={false}
                  onChange={(event) => patchAgent(agent.id, { program: event.target.value })}
                />
              </label>
              <label className="field">
                <span>1 回の上限（秒）</span>
                <input
                  type="number"
                  inputMode="numeric"
                  placeholder={String(draft.limits.defaultTimeoutSecs)}
                  value={agent.timeoutSecs == null ? "" : String(agent.timeoutSecs)}
                  onChange={(event) =>
                    patchAgent(agent.id, {
                      timeoutSecs: event.target.value.trim() === "" ? null : Number(event.target.value),
                    })
                  }
                />
              </label>
              <label className="field wide">
                <span>引数</span>
                <LinesField value={agent.args} onChange={(args) => patchAgent(agent.id, { args })} />
              </label>
              <div className="field wide">
                <span>プロンプトの渡し方</span>
                <Segmented
                  value={agent.promptVia}
                  choices={PROMPT_VIAS.map((value) => ({ value, label: PROMPT_VIA_LABEL[value] }))}
                  onChange={(promptVia) => patchAgent(agent.id, { promptVia })}
                />
              </div>
              <div className="field wide">
                <span>出力</span>
                <Segmented
                  value={agent.output}
                  choices={OUTPUTS.map((value) => ({ value, label: OUTPUT_LABEL[value] }))}
                  onChange={(output) => patchAgent(agent.id, { output })}
                />
              </div>
              <label className="field wide">
                <span>役割</span>
                <textarea rows={2} value={agent.role} onChange={(event) => patchAgent(agent.id, { role: event.target.value })} />
              </label>
              <label className="field wide">
                <span>プロンプトの前置き</span>
                <textarea
                  rows={2}
                  maxLength={2000}
                  value={agent.promptPrefix ?? ""}
                  onChange={(event) => patchAgent(agent.id, { promptPrefix: event.target.value })}
                />
              </label>
            </div>
            <button
              type="button"
              role="checkbox"
              aria-checked={agent.allowDangerous}
              className="view-option"
              onClick={() => patchAgent(agent.id, { allowDangerous: !agent.allowDangerous })}
            >
              <span className="quiet-check" aria-hidden="true">{agent.allowDangerous ? "✓" : ""}</span>
              危険なフラグを許可
            </button>
            {agent.allowDangerous && <p className="danger-note">確認なしでファイルの削除や外部への送信が行われることがあります</p>}
          </div>
        );
      })}

      <div className="agent-actions">
        <button type="button" className="solid" disabled={!dirty || busy} onClick={() => void save()}>
          保存
        </button>
        <button type="button" className="pill" disabled={!dirty || busy} onClick={() => setDraft(registry)}>
          取り消し
        </button>
        <span className="composer-spacer" />
        <button
          type="button"
          className="pill"
          disabled={busy || draft.agents.length >= 16}
          onClick={() => setDraft((current) => (current ? { ...current, agents: [...current.agents, blankCustomAgent(current.agents)] } : current))}
        >
          カスタムを追加
        </button>
        <button type="button" className="pill" disabled={busy} onClick={() => void reset()}>
          既定に戻す
        </button>
      </div>
      {note && <p className="settings-copy">{note}</p>}
    </fieldset>
  );
}

function parseLines(text: string) {
  return text
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean);
}

function LinesField({ value, onChange }: { value: string[]; onChange: (value: string[]) => void }) {
  const joined = value.join("\n");
  const [text, setText] = useState(joined);
  useEffect(() => {
    setText((current) => (parseLines(current).join("\n") === joined ? current : joined));
  }, [joined]);
  return (
    <textarea
      className="mono"
      rows={Math.min(8, Math.max(2, value.length + 1))}
      spellCheck={false}
      value={text}
      onChange={(event) => {
        setText(event.target.value);
        onChange(parseLines(event.target.value));
      }}
    />
  );
}

function Segmented<T extends string>({
  value,
  choices,
  onChange,
}: {
  value: T;
  choices: { value: T; label: string }[];
  onChange: (value: T) => void;
}) {
  return (
    <div className="segmented" role="radiogroup">
      {choices.map((choice) => {
        const selected = choice.value === value;
        return (
          <button
            key={choice.value}
            type="button"
            role="radio"
            aria-checked={selected}
            className={selected ? "on" : ""}
            onClick={() => {
              if (!selected) onChange(choice.value);
            }}
          >
            {choice.label}
          </button>
        );
      })}
    </div>
  );
}
