import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Chat, debounce, shouldShowCitations } from "./chat";
import { renderMarkdown } from "./render";
import "katex/dist/katex.min.css";
import "../styles.css";

interface Citation {
  n: number;
  chunk_id: number;
  path: string;
  header: string;
  page: number | null;
  stale: boolean;
  /** Code hits: first line, and `path:line` relative to the repository. */
  line: number | null;
  location: string | null;
}

/** One question/answer pair in the visible (ephemeral) thread. */
interface TurnView {
  id: number;
  query: string;
  text: string;
  hits: Citation[];
  retrievalMs: number;
  root: HTMLElement;
  answer: HTMLElement;
  error: HTMLElement;
  sources: HTMLElement;
  sourceList: HTMLOListElement;
}

interface RequestEvent {
  request_id: number;
}

const form = document.querySelector<HTMLFormElement>("#ask-form")!;
const input = document.querySelector<HTMLInputElement>("#question")!;
const submit = document.querySelector<HTMLButtonElement>("#submit")!;
const conversation = document.querySelector<HTMLElement>("#conversation")!;
const thread = document.querySelector<HTMLElement>("#thread")!;
const errorLine = document.querySelector<HTMLElement>("#error")!;
const timings = document.querySelector<HTMLElement>("#timings")!;

const chat = new Chat();
const turns = new Map<HTMLElement, TurnView>();
interface PanelOptions {
  active: number;
  total: number;
  reset_minutes: number;
  profiles: { id: number; name: string }[];
  repos: { id: number; name: string }[];
  code_scope: number | null;
}

let options: PanelOptions = { active: 0, total: 0, reset_minutes: 10, profiles: [], repos: [], code_scope: null };
async function refreshOptions() {
  options = await invoke<PanelOptions>("panel_options");
  const repo = options.repos.find(({ id }) => id === options.code_scope);
  document.querySelector("#scope")!.textContent = repo
    ? `Code: ${repo.name} · @notes to go back`
    : `${options.active} of ${options.total} sources`;
  input.placeholder = repo
    ? `Ask about the ${repo.name} code…`
    : "Ask about your notes, or anything…";
}
void refreshOptions().catch(console.error);

function openSettings() {
  void invoke("open_settings").catch((error: unknown) => showError(String(error)));
}

let nextId = Date.now();
let current: TurnView | undefined;
let frame = 0;
let mermaidLoaded: Promise<typeof import("mermaid")> | undefined;

function showError(message = "") {
  errorLine.textContent = message;
  errorLine.hidden = !message;
}

function setStreaming(streaming: boolean) {
  submit.type = streaming ? "button" : "submit";
  submit.textContent = streaming ? "■" : "↵";
  submit.title = streaming ? "Stop answer" : "Send (Return)";
  submit.setAttribute("aria-label", submit.title);
  current?.answer.setAttribute("aria-busy", String(streaming));
}

const prefetch = debounce(() => {
  const query = input.value.trim();
  if (query && !query.startsWith("@") && !current) {
    void invoke("prefetch", { query, history: chat.recentTurns() }).catch(console.error);
  }
}, 150);

/** Retrieval that already ran while typing (prefetch cache hit) takes ~0 ms at send time. */
function retrievalLabel(ms: number) {
  return ms < 1 ? "retrieval prefetched" : `retrieval ${Math.round(ms)}ms`;
}

function element<K extends keyof HTMLElementTagNameMap>(tag: K, className: string, text = "") {
  const node = document.createElement(tag);
  node.className = className;
  node.textContent = text;
  return node;
}

function createTurn(id: number, query: string): TurnView {
  for (const past of thread.querySelectorAll(".turn")) past.classList.add("past");
  const root = element("article", "turn");
  const question = element("div", "question", query);
  const answer = element("div", "markdown answer");
  const error = element("div", "turn-error");
  error.setAttribute("role", "alert");
  error.hidden = true;
  const sources = element("section", "sources");
  sources.setAttribute("aria-label", "Sources");
  sources.hidden = true;
  const sourceList = element("ol", "source-list");
  sources.append(element("h2", "", "Sources"), sourceList);
  root.append(question, answer, error, sources);
  thread.append(root);
  const turn = { id, query, text: "", hits: [], retrievalMs: 0, root, answer, error, sources, sourceList };
  turns.set(root, turn);
  root.scrollIntoView({ block: "start" });
  return turn;
}

function renderTurn(turn: TurnView) {
  const nearBottom = conversation.scrollHeight - conversation.scrollTop - conversation.clientHeight < 65;
  const hits = shouldShowCitations(turn.text, turn.hits.length) ? turn.hits : [];
  turn.answer.innerHTML = renderMarkdown(turn.text, hits.length);
  if (turn.sources.hidden !== (hits.length === 0) || turn.sourceList.childElementCount !== hits.length) {
    listCitations(turn, hits);
  }
  if (nearBottom) conversation.scrollTop = conversation.scrollHeight;
}

function renderCurrent() {
  frame = 0;
  if (current) renderTurn(current);
}

function flush() {
  if (frame) {
    cancelAnimationFrame(frame);
    renderCurrent();
  }
}

async function renderDiagrams(turn: TurnView) {
  if (!turn.answer.querySelector(".mermaid")) return;
  mermaidLoaded ??= import("mermaid").then((module) => {
    module.default.initialize({
      startOnLoad: false,
      securityLevel: "strict",
      theme: matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "default",
    });
    return module;
  });
  try {
    const { default: mermaid } = await mermaidLoaded;
    if (turn.root.isConnected) {
      await mermaid.run({ nodes: turn.answer.querySelectorAll<HTMLElement>(".mermaid"), suppressErrors: true });
    }
  } catch (error) {
    console.error("Rendering diagram", error);
  }
}

function listCitations(turn: TurnView, hits: Citation[]) {
  turn.sourceList.replaceChildren(...hits.map((hit) => {
    const item = document.createElement("li");
    const label = hit.location ? `[${hit.n}] ${hit.location} · ${hit.header}` : `[${hit.n}] ${hit.header}`;
    const button = element("button", "citation", label);
    button.type = "button";
    button.dataset.n = String(hit.n);
    button.title = hit.location ?? hit.path;
    item.append(button);
    if (hit.stale) item.append(element("span", "stale", "changed since ingest"));
    return item;
  }));
  turn.sources.hidden = hits.length === 0;
}

function finish(turn: TurnView) {
  if (current !== turn) return;
  flush();
  setStreaming(false);
  current = undefined;
}

function cancelAnswer() {
  if (!current) return;
  const turn = current;
  finish(turn);
  turn.root.classList.add("stopped");
  timings.textContent = "Stopped";
  void invoke("cancel_answer", { requestId: turn.id }).catch(console.error);
}

/** Switches the panel to a code repository, or back to notes with `null`. */
async function useCodeScope(id: number | null) {
  await invoke("use_code_scope", { id });
  await refreshOptions();
}

/** New chat. `toNotes`: an explicit ⌘N also leaves a code repository. */
function reset(toNotes = false) {
  if (toNotes && options.code_scope !== null) {
    void useCodeScope(null).catch((error: unknown) => showError(String(error)));
  }
  cancelAnswer();
  prefetch.cancel();
  chat.reset();
  turns.clear();
  thread.replaceChildren();
  input.value = "";
  timings.textContent = "";
  showError();
  input.focus();
}

function handleError(event: RequestEvent & { message: string }) {
  const turn = current;
  if (!turn || event.request_id !== turn.id) return;
  finish(turn);
  timings.textContent = "";
  turn.error.textContent = event.message;
  turn.error.hidden = false;
}

type Target = { kind: "profile" | "repo"; id: number | null; name: string };

/** `@name` at the start of a question: a profile, a code repository, or `@notes`. */
function scopePrefix(query: string): Target | undefined {
  const lower = query.toLowerCase();
  const targets: Target[] = [
    ...options.repos.map(({ id, name }) => ({ kind: "repo" as const, id, name })),
    ...options.profiles.map(({ id, name }) => ({ kind: "profile" as const, id, name })),
  ];
  if (options.code_scope !== null) targets.push({ kind: "repo", id: null, name: "notes" });
  return targets.filter(({ name }) => {
    const prefix = `@${name.toLowerCase()}`;
    return lower === prefix || lower.startsWith(`${prefix} `);
  }).sort((a, b) => b.name.length - a.name.length)[0];
}

let switchingProfile = false;
form.addEventListener("submit", async (event) => {
  event.preventDefault();
  let query = input.value.trim();
  if (!query || switchingProfile) return;
  const target = scopePrefix(query);
  if (target) {
    switchingProfile = true;
    cancelAnswer();
    try {
      if (target.kind === "profile") {
        if (options.code_scope !== null) await useCodeScope(null);
        await invoke("use_profile", { id: target.id });
      } else {
        await useCodeScope(target.id);
      }
      chat.reset();
      query = query.slice(target.name.length + 1).trim();
      input.value = query;
      if (!query) {
        timings.textContent = target.id === null ? "Back to notes" : `Using ${target.name}`;
        return;
      }
    } catch (error) {
      showError(String(error));
      return;
    } finally {
      switchingProfile = false;
    }
  }
  cancelAnswer();
  prefetch.cancel();
  showError();
  const id = ++nextId;
  const history = chat.recentTurns();
  const carry = chat.carry();
  current = createTurn(id, query);
  input.value = "";
  input.focus();
  timings.textContent = "Searching…";
  setStreaming(true);
  void invoke("ask", { query, history, requestId: id, carry })
    .catch((error: unknown) => handleError({ request_id: id, message: String(error) }));
});

submit.addEventListener("click", (event) => {
  if (current && submit.type === "button") {
    event.preventDefault();
    cancelAnswer();
    input.focus();
  }
});
input.addEventListener("input", prefetch);
input.addEventListener("keydown", (event) => {
  if (event.key === "Enter" && !event.isComposing && current && input.value.trim()) {
    event.preventDefault();
    cancelAnswer();
    form.requestSubmit();
  }
});
document.querySelector("#settings")!.addEventListener("click", openSettings);
document.querySelector("#reset")!.addEventListener("click", () => reset(true));
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") {
    event.preventDefault();
    void invoke("hide_panel").catch((error: unknown) => showError(String(error)));
  }
  if (event.metaKey && event.key === ",") {
    event.preventDefault();
    openSettings();
  }
  if (event.metaKey && event.key.toLowerCase() === "n") {
    event.preventDefault();
    reset(true);
  }
});

conversation.addEventListener("click", (event) => {
  if (!(event.target instanceof Element)) return;
  const root = event.target.closest<HTMLElement>(".turn");
  const turn = root ? turns.get(root) : undefined;
  const citation = event.target.closest<HTMLElement>("[data-n]");
  const hit = turn?.hits.find(({ n }) => n === Number(citation?.dataset.n));
  // Keep generated markdown links from navigating the panel away from the chat.
  if (citation || event.target.closest("a")) event.preventDefault();
  if (hit) {
    void invoke("open_citation", { path: hit.path, page: hit.page, line: hit.line })
      .catch((error: unknown) => showError(String(error)));
  }
});

void Promise.all([
  listen("sources://changed", () => void refreshOptions().catch(console.error)),
  listen<number | null>("scope://changed", ({ payload }) => {
    if (current?.id === payload) cancelAnswer();
    prefetch.cancel();
    chat.reset();
    void refreshOptions().catch(console.error);
  }),
  listen<boolean>("drop://hover", ({ payload }) => {
    document.querySelector<HTMLElement>("#drop-overlay")!.hidden = !payload;
    if (payload) showError();
  }),
  listen<string>("drop://error", ({ payload }) => showError(payload)),
  listen<number>("panel://shown", ({ payload: hiddenMs }) => {
    document.querySelector<HTMLElement>("#drop-overlay")!.hidden = true;
    if (options.reset_minutes > 0 && hiddenMs > options.reset_minutes * 60_000) reset();
    input.focus();
  }),
  listen<RequestEvent & { hits: Citation[]; retrieval_ms: number; provider: string }>("answer://meta", ({ payload }) => {
    if (!current || payload.request_id !== current.id) return;
    current.hits = payload.hits;
    current.retrievalMs = payload.retrieval_ms;
    timings.textContent = `${retrievalLabel(current.retrievalMs)} · Waiting for ${payload.provider}…`;
  }),
  listen<RequestEvent & { text: string }>("answer://token", ({ payload }) => {
    if (!current || payload.request_id !== current.id) return;
    current.text += payload.text;
    if (!frame) frame = requestAnimationFrame(renderCurrent);
  }),
  listen<RequestEvent & { ttft_ms: number; tok_per_s: number }>("answer://done", ({ payload }) => {
    const turn = current;
    if (!turn || payload.request_id !== turn.id) return;
    finish(turn);
    renderTurn(turn);
    if (turn.text) {
      chat.push(turn.query, turn.text, turn.hits.map(({ chunk_id }) => chunk_id));
    } else {
      turn.error.textContent = "The model returned an empty answer";
      turn.error.hidden = false;
    }
    timings.textContent = `${retrievalLabel(turn.retrievalMs)} · ttft ${Math.round(payload.ttft_ms)}ms · ${Math.round(payload.tok_per_s)} tok/s`;
    void renderDiagrams(turn);
    prefetch();
  }),
  listen<RequestEvent & { message: string }>("answer://error", ({ payload }) => handleError(payload)),
]).catch((error: unknown) => showError(String(error)));
