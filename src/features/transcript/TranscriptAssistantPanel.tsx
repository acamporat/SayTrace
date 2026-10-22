import {
  Bot,
  LockKeyhole,
  MessageSquare,
  RefreshCw,
  Send,
  Sparkles,
  Trash2,
} from "lucide-react";
import { memo, useEffect, useRef, useState } from "react";
import type { ReactNode } from "react";
import { formatDuration } from "../../lib/format";
import { invokeCommand, isTauriRuntime, openExternalUrl } from "../../lib/tauri";
import type {
  ChatGptAgentStatus,
  LocalAgentStatus,
  TranscriptChatMessage,
  TranscriptCitation,
} from "../../types";

interface TranscriptAssistantPanelProps {
  meetingId?: string;
  meetingTitle: string;
  messages: TranscriptChatMessage[];
  status: LocalAgentStatus;
  loading: boolean;
  error?: string;
  transcriptReady: boolean;
  onOpenSpeakers: () => void;
  onAsk: (question: string, model?: string, provider?: "local" | "chatgpt", cloudConsent?: boolean) => Promise<void>;
  onClear: () => Promise<void>;
  onRefreshStatus: () => Promise<void>;
  onOpenCitation: (citation: TranscriptCitation) => void;
}

const starterQuestions = [
  "What decisions were made?",
  "List action items with owners and timing",
  "Summarize this meeting",
];

const inlineBoldPattern = /(\*\*[^*]+\*\*)/g;
const headingPattern = /^#{2,3}\s+(.+)$/;
const orderedItemPattern = /^\d+\.\s+(.+)$/;
const unorderedItemPattern = /^[-*•]\s+(.+)$/;

function renderInlineMarkdown(value: string): ReactNode[] {
  return value.split(inlineBoldPattern).filter(Boolean).map((part, index) =>
    part.startsWith("**") && part.endsWith("**") ? (
      <strong key={`${part}-${index}`}>{part.slice(2, -2)}</strong>
    ) : (
      <span key={`${part}-${index}`}>{part}</span>
    ),
  );
}

const AssistantAnswer = memo(function AssistantAnswer({ content }: { content: string }) {
  const lines = content.replace(/\r\n/g, "\n").split("\n");
  const blocks: ReactNode[] = [];
  let index = 0;

  while (index < lines.length) {
    const line = lines[index].trim();
    if (!line) {
      index += 1;
      continue;
    }

    const heading = line.match(headingPattern);
    if (heading) {
      blocks.push(<h3 key={`heading-${index}`}>{renderInlineMarkdown(heading[1])}</h3>);
      index += 1;
      continue;
    }

    const listKind = orderedItemPattern.test(line)
      ? "ordered"
      : unorderedItemPattern.test(line)
        ? "unordered"
        : undefined;
    if (listKind) {
      const items: ReactNode[] = [];
      while (index < lines.length) {
        const candidate = lines[index].trim();
        const ordered = candidate.match(orderedItemPattern);
        const unordered = candidate.match(unorderedItemPattern);
        const item = listKind === "ordered" ? ordered?.[1] : unordered?.[1];
        if (!item) break;
        items.push(<li key={`item-${index}`}>{renderInlineMarkdown(item)}</li>);
        index += 1;
      }
      blocks.push(
        listKind === "ordered" ? (
          <ol key={`list-${index}`}>{items}</ol>
        ) : (
          <ul key={`list-${index}`}>{items}</ul>
        ),
      );
      continue;
    }

    const paragraph: string[] = [line];
    index += 1;
    while (index < lines.length) {
      const candidate = lines[index].trim();
      if (!candidate || headingPattern.test(candidate) || orderedItemPattern.test(candidate) || unorderedItemPattern.test(candidate)) {
        break;
      }
      paragraph.push(candidate);
      index += 1;
    }
    blocks.push(
      <p key={`paragraph-${index}`}>{renderInlineMarkdown(paragraph.join(" "))}</p>,
    );
  }

  return <div className="assistant-answer">{blocks}</div>;
});

export function TranscriptAssistantPanel({
  meetingId,
  meetingTitle,
  messages,
  status,
  loading,
  error,
  transcriptReady,
  onOpenSpeakers,
  onAsk,
  onClear,
  onRefreshStatus,
  onOpenCitation,
}: TranscriptAssistantPanelProps) {
  const [draft, setDraft] = useState("");
  const [model, setModel] = useState(status.selectedModel ?? "");
  const [provider, setProvider] = useState<"local" | "chatgpt">("local");
  const [cloudConsent, setCloudConsent] = useState(false);
  const [cloudStatus, setCloudStatus] = useState<ChatGptAgentStatus>({ available: false, signedIn: false, models: [] });
  const [cloudModel, setCloudModel] = useState("");
  const [authBusy, setAuthBusy] = useState(false);
  const [authPending, setAuthPending] = useState(false);
  const [authError, setAuthError] = useState<string>();
  const scrollRef = useRef<HTMLDivElement>(null);
  const latestMessageRef = useRef<HTMLElement>(null);

  useEffect(() => { setCloudConsent(false); }, [meetingId, meetingTitle, provider]);

  async function refreshCloudStatus() {
    if (!isTauriRuntime()) {
      setAuthError("ChatGPT sign-in is available in the installed desktop app.");
      return;
    }
    try {
      const next = await invokeCommand("get_chatgpt_agent_status");
      setCloudStatus(next);
      setAuthError(next.error);
      if (next.signedIn) setAuthPending(false);
      setCloudModel((current) => next.models.some((item) => item.id === current) ? current : next.models[0]?.id ?? "");
    } catch (cause) {
      setAuthError(cause instanceof Error ? cause.message : "ChatGPT status could not be checked.");
    }
  }

  useEffect(() => {
    if (provider !== "chatgpt") return;
    void refreshCloudStatus();
    if (!authPending) return;
    const timer = window.setInterval(() => void refreshCloudStatus(), 5000);
    const timeout = window.setTimeout(() => setAuthPending(false), 180000);
    return () => { window.clearInterval(timer); window.clearTimeout(timeout); };
  }, [provider, authPending]);

  async function signIn() {
    setAuthBusy(true);
    setAuthError(undefined);
    try {
      const url = await invokeCommand("sign_in_chatgpt");
      await openExternalUrl(url);
      setAuthPending(true);
    } catch (cause) {
      setAuthError(cause instanceof Error ? cause.message : "ChatGPT sign-in could not start.");
    } finally { setAuthBusy(false); }
  }

  async function signOut() {
    setAuthBusy(true);
    setCloudConsent(false);
    setAuthPending(false);
    try {
      await invokeCommand("sign_out_chatgpt");
      setCloudStatus({ available: true, signedIn: false, models: [] });
      setCloudModel("");
    } catch (cause) {
      setAuthError(cause instanceof Error ? cause.message : "ChatGPT sign-out failed.");
    } finally { setAuthBusy(false); }
  }

  useEffect(() => {
    if (!status.models.some((candidate) => candidate.name === model)) {
      setModel(status.selectedModel ?? status.models[0]?.name ?? "");
    }
  }, [model, status.models, status.selectedModel]);

  useEffect(() => {
    const scroll = scrollRef.current;
    if (!scroll) return;
    const latestMessage = messages[messages.length - 1];
    if (
      !loading &&
      latestMessage?.role === "assistant" &&
      typeof latestMessageRef.current?.scrollIntoView === "function"
    ) {
      latestMessageRef.current.scrollIntoView({
        behavior: messages.length > 1 ? "smooth" : "auto",
        block: "start",
      });
      return;
    }
    if (typeof scroll.scrollTo === "function") {
      scroll.scrollTo({
        top: scroll.scrollHeight,
        behavior: messages.length > 1 ? "smooth" : "auto",
      });
    } else {
      scroll.scrollTop = scroll.scrollHeight;
    }
  }, [loading, messages]);

  const canAsk =
    transcriptReady && !loading && (provider === "local"
      ? status.state === "ready" && Boolean(model)
      : cloudStatus.available && cloudStatus.signedIn && Boolean(cloudModel) && cloudConsent && !authBusy);

  async function submit(question: string) {
    const value = question.trim();
    if (!value || !canAsk) return;
    setDraft("");
    await onAsk(value, provider === "local" ? model : cloudModel, provider, provider === "chatgpt" && cloudConsent);
  }

  return (
    <>
      <div className="side-panel-tabs" role="tablist" aria-label="Transcript tools">
        <button type="button" className="is-active" role="tab" aria-selected="true">
          Ask
        </button>
        <button
          type="button"
          role="tab"
          aria-selected="false"
          onClick={onOpenSpeakers}
        >
          Speakers
        </button>
      </div>
      <section className="assistant-panel" aria-label="Ask this transcript">
        <header className="assistant-panel__header">
          <span>
            <h2>Ask this transcript</h2>
            <small>
              <LockKeyhole size={11} /> {provider === "local" ? "Runs locally on this device" : "Uses your ChatGPT account through Codex"}
            </small>
          </span>
          {messages.length ? (
            <button
              className="icon-button"
              type="button"
              aria-label="Clear transcript conversation"
              title="Clear conversation"
              disabled={loading}
              onClick={() => void onClear()}
            >
              <Trash2 size={16} />
            </button>
          ) : null}
        </header>

        <div className="assistant-panel__scroll" ref={scrollRef}>
        <div className="assistant-provider">
          <label htmlFor="assistant-provider">Answer with</label>
          <select id="assistant-provider" value={provider} disabled={loading || authBusy}
            onChange={(event) => setProvider(event.target.value as "local" | "chatgpt")}>
            <option value="local">Local AI · On this device</option>
            <option value="chatgpt">ChatGPT · Codex usage</option>
          </select>
          {provider === "chatgpt" ? <>
            <p>Uses your available Codex plan usage or credits. Limits still apply. Questions, relevant transcript excerpts, speaker names, and conversation history are sent to OpenAI. Audio and video stay local.</p>
            {cloudStatus.signedIn ? <>
              <small>Connected{cloudStatus.email ? ` as ${cloudStatus.email}` : ""}{cloudStatus.planType ? ` · ${cloudStatus.planType}` : ""}</small>
              <button type="button" disabled={loading || authBusy} onClick={() => void signOut()}>Sign out</button>
            </> : <button type="button" disabled={loading || authBusy || authPending || !isTauriRuntime()} onClick={() => void signIn()}>
              {authPending ? "Finish sign-in in your browser…" : authBusy ? "Opening sign-in…" : "Continue with ChatGPT"}
            </button>}
            <label className="assistant-cloud-consent">
              <input type="checkbox" checked={cloudConsent} disabled={loading || !cloudStatus.signedIn}
                onChange={(event) => setCloudConsent(event.target.checked)} />
              Allow meeting text to be sent to OpenAI for these questions
            </label>
            {authError ? <p role="alert">{authError}</p> : null}
            <button type="button" disabled={authBusy || loading} onClick={() => void refreshCloudStatus()}>Refresh ChatGPT status</button>
          </> : null}
        </div>

          <div className="assistant-welcome">
            <Sparkles size={18} />
            <p>
              I’m SayTrace Assistant. Ask about <strong>{meetingTitle}</strong> and
              I’ll answer from this transcript with links to the exact moments.
            </p>
          </div>

          {!messages.length ? (
            <div className="assistant-starters" aria-label="Suggested questions">
              {starterQuestions.map((question) => (
                <button
                  key={question}
                  type="button"
                  disabled={!canAsk}
                  onClick={() => void submit(question)}
                >
                  <MessageSquare size={14} /> {question}
                </button>
              ))}
            </div>
          ) : null}

          <div className="assistant-messages" aria-live="polite">
            {messages.map((message, index) => (
              <article
                key={message.id}
                ref={index === messages.length - 1 ? latestMessageRef : undefined}
                className={`assistant-message assistant-message--${message.role}`}
              >
                {message.role === "assistant" ? (
                  <span className="assistant-message__icon" aria-hidden="true">
                    <Bot size={15} />
                  </span>
                ) : null}
                <div>
                  {message.role === "assistant" ? (
                    <><small className="assistant-message__provider">{message.model?.startsWith("chatgpt:") ? "ChatGPT · Codex" : "Local AI"}</small><AssistantAnswer content={message.content} /></>
                  ) : (
                    <p>{message.content}</p>
                  )}
                  {message.citations.length ? (
                    <div className="assistant-citations" aria-label="Answer citations">
                      {message.citations.map((citation) => (
                        <button
                          key={`${message.id}-${citation.turnId}`}
                          type="button"
                          title={`${citation.speakerName}: ${citation.snippet}`}
                          onClick={() => onOpenCitation(citation)}
                        >
                          {formatDuration(citation.startMs, false)}
                        </button>
                      ))}
                    </div>
                  ) : null}
                </div>
              </article>
            ))}
            {loading ? (
              <article className="assistant-message assistant-message--assistant assistant-message--loading">
                <span className="assistant-message__icon" aria-hidden="true">
                  <Bot size={15} />
                </span>
                <div>
                  <p>{provider === "local" ? "Thinking locally" : "Asking ChatGPT"}</p>
                  <span aria-hidden="true"><i /><i /><i /></span>
                </div>
              </article>
            ) : null}
          </div>

          {error ? <div className="assistant-error" role="alert">{error}</div> : null}
          {!transcriptReady ? (
            <div className="assistant-unavailable">
              Finish processing this meeting before asking questions.
            </div>
          ) : provider === "local" && status.state !== "ready" ? (
            <div className="assistant-unavailable">
              {status.state === "checking"
                ? "Checking the local AI runtime…"
                : status.state === "no_models"
                  ? "Ollama is running, but no fully local model is installed."
                  : "Ollama is not available on this device."}
              <button type="button" onClick={() => void onRefreshStatus()}>
                <RefreshCw size={13} /> Check again
              </button>
            </div>
          ) : null}
        </div>

        <form
          className="assistant-composer"
          onSubmit={(event) => {
            event.preventDefault();
            void submit(draft);
          }}
        >
          <textarea
            aria-label="Ask about this transcript"
            placeholder="Ask about this transcript"
            rows={2}
            maxLength={500}
            value={draft}
            disabled={!transcriptReady || loading}
            onChange={(event) => setDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && !event.shiftKey) {
                event.preventDefault();
                void submit(draft);
              }
            }}
          />
          <button
            type="submit"
            aria-label="Send transcript question"
            disabled={!canAsk || !draft.trim()}
          >
            <Send size={17} />
          </button>
        </form>

        <div className={`assistant-model assistant-model--${provider === "local" ? status.state : cloudStatus.signedIn ? "ready" : "unavailable"}`}>
          <span aria-hidden="true" />
          {provider === "chatgpt" ? <select aria-label="ChatGPT model" value={cloudModel} disabled={loading || !cloudStatus.signedIn}
            onChange={(event) => setCloudModel(event.target.value)}>
            {!cloudStatus.models.length ? <option value="">Sign in to see models</option> : null}
            {cloudStatus.models.map((candidate) => <option key={candidate.id} value={candidate.id}>{candidate.displayName} · ChatGPT</option>)}
          </select> : status.models.length ? (
            <select
              aria-label="Local AI model"
              value={model}
              disabled={loading}
              onChange={(event) => setModel(event.target.value)}
            >
              {status.models.map((candidate) => (
                <option key={candidate.name} value={candidate.name}>
                  {candidate.name} · Local
                </option>
              ))}
            </select>
          ) : (
            <strong>{status.backend || "Local AI"}</strong>
          )}
          <button
            type="button"
            aria-label="Refresh AI status"
            title="Refresh AI status"
            onClick={() => void (provider === "local" ? onRefreshStatus() : refreshCloudStatus())}
          >
            <RefreshCw size={14} />
          </button>
        </div>
      </section>
    </>
  );
}
