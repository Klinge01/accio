import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./settings.css";

const providers = {
  groq: "Groq API",
  anthropic: "Anthropic API",
  open_ai: "OpenAI API",
  claude_code: "Claude Code on this Mac",
  codex: "Codex on this Mac",
  mlx: "Local model (MLX)",
} as const;
type Provider = keyof typeof providers;

interface Preferences {
  provider: Provider;
  models: Record<string, string>;
  min_similarity: number;
  excerpts: number;
  memory: number;
  prefetch: boolean;
  shortcut: string;
  panel_width: number;
  panel_height: number;
  reset_minutes: number;
  default_profile: number | null;
}

interface Source {
  id: number;
  kind: "Notes" | "Code";
  path: string;
  enabled: boolean;
  is_file: boolean;
  captions: boolean | null;
  pdf_mode: "Text" | "Markdown";
  status: string | { Failed: string };
  last_ingested_at: number | null;
  file_count: number;
  failed_count: number;
  size: number;
}

interface Profile { id: number; name: string; sources: number[] }
interface Snapshot {
  preferences: Preferences;
  sources: Source[];
  profiles: Profile[];
  index_path: string;
  index_size: number;
  embedder: string;
  launch_status: number;
}

const sections = ["Model", "Sources", "Profiles", "Search", "Shortcut & panel", "Privacy", "Data"] as const;
type Section = typeof sections[number];

const nav = document.querySelector("nav")!;
const heading = document.querySelector("h1")!;
const notice = document.querySelector<HTMLParagraphElement>("#notice")!;
const content = document.querySelector<HTMLElement>("#content")!;
let snapshot: Snapshot;
let current: Section = "Model";
let version = 0;

function node<K extends keyof HTMLElementTagNameMap>(tag: K, text?: string) {
  const element = document.createElement(tag);
  if (text !== undefined) element.textContent = text;
  return element;
}

function status(text = "", error = false) {
  notice.textContent = text;
  notice.hidden = !text;
  notice.classList.toggle("error", error);
}

function fail(error: unknown) { status(String(error), true); }

function button(text: string, action: () => void | Promise<void>) {
  const control = node("button", text);
  control.type = "button";
  control.onclick = async () => {
    control.disabled = true;
    status();
    try { await action(); } catch (error) { fail(error); }
    finally { control.disabled = false; }
  };
  return control;
}

function field(title: string, control: HTMLElement) {
  const label = node("label");
  label.className = "field";
  label.append(node("span", title), control);
  return label;
}

function checkbox(text: string, checked: boolean, change: (checked: boolean) => Promise<void>) {
  const input = node("input");
  input.type = "checkbox";
  input.checked = checked;
  input.onchange = async () => {
    input.disabled = true;
    status();
    try { await change(input.checked); }
    catch (error) { input.checked = !input.checked; fail(error); }
    finally { input.disabled = false; }
  };
  const label = node("label");
  label.className = "checkbox";
  label.append(input, node("span", text));
  return label;
}

function select(value: string, options: [string, string][]) {
  const control = node("select");
  for (const [key, title] of options) {
    const option = node("option", title);
    option.value = key;
    control.append(option);
  }
  control.value = value;
  return control;
}

function number(value: number, min: number, max: number) {
  const input = node("input");
  input.type = "number";
  input.min = String(min);
  input.max = String(max);
  input.value = String(value);
  return input;
}

function row(...children: Node[]) {
  const result = node("div");
  result.className = "row";
  result.append(...children);
  return result;
}

function hint(text: string) {
  const paragraph = node("p", text);
  paragraph.className = "hint";
  return paragraph;
}

async function load() {
  snapshot = await invoke<Snapshot>("settings_snapshot");
}

async function update(change: Record<string, unknown>) {
  await invoke("change_preference", { change });
}

function render(section = current, clearStatus = true) {
  current = section;
  version += 1;
  if (clearStatus) status();
  heading.textContent = section;
  content.replaceChildren();
  for (const control of nav.querySelectorAll("button")) {
    if (control.textContent === section) control.setAttribute("aria-current", "page");
    else control.removeAttribute("aria-current");
  }
  switch (section) {
    case "Model": renderModel(); break;
    case "Sources": renderSources(); break;
    case "Profiles": renderProfiles(); break;
    case "Search": renderSearch(); break;
    case "Shortcut & panel": renderPanel(); break;
    case "Privacy": renderPrivacy(); break;
    case "Data": renderData(); break;
  }
}

function renderModel() {
  const revision = version;
  const prefs = snapshot.preferences;
  const provider = prefs.provider;
  const model = prefs.models[provider] ?? "";
  const providerPicker = select(provider, Object.entries(providers));
  providerPicker.onchange = async () => {
    try {
      await update({ kind: "provider", provider: providerPicker.value });
      await load();
      if (current === "Model") render();
    } catch (error) { providerPicker.value = provider; fail(error); }
  };
  const controls = node("div");
  content.append(field("Provider", providerPicker), controls);
  const cli = provider === "claude_code" || provider === "codex";
  const keyProvider = !cli && provider !== "mlx";
  if (cli) {
    controls.append(hint("For personal use on this Mac. Starts your signed-in CLI for each question."));
  }
  if (provider === "mlx") {
    controls.append(hint("Uses an MLX server on 127.0.0.1:8080. Start it with mlx_lm.server --host 127.0.0.1."));
  }

  const modelInput = node("input");
  modelInput.value = model;
  modelInput.placeholder = cli ? "CLI default" : "Select or enter a model ID";
  modelInput.setAttribute("list", "models");
  const modelList = node("datalist");
  modelList.id = "models";
  const setModels = (models: string[]) => {
    modelList.replaceChildren(...models.map((id) => {
      const option = node("option");
      option.value = id;
      return option;
    }));
  };
  modelInput.onchange = async () => {
    const value = modelInput.value.trim();
    try {
      await update({ kind: "model", provider, model: value });
      prefs.models[provider] = value;
    } catch (error) { fail(error); }
  };
  controls.append(field("Model", modelInput), modelList);
  {
    const refresh = async () => {
      const models = await invoke<string[]>("provider_models", { provider });
      if (version === revision) setModels(models);
    };
    controls.append(button("Refresh model list", refresh));
    void refresh().catch(() => {});
  }

  if (keyProvider) {
    const key = node("input");
    key.type = "password";
    key.autocomplete = "off";
    key.placeholder = "Enter a new key";
    const result = node("p");
    result.className = "hint";
    result.setAttribute("role", "status");
    const test = button("Test & store key", async () => {
      modelInput.disabled = true;
      key.disabled = true;
      result.textContent = "Testing…";
      try {
        const response = await invoke<{ ttft_ms: number; models: string[]; model: string }>(
          "test_api_key", { provider, key: key.value, model: modelInput.value },
        );
        setModels(response.models);
        modelInput.value = response.model;
        prefs.models[provider] = response.model;
        key.value = "";
        result.textContent = `Stored in Keychain · first token in ${Math.round(response.ttft_ms)} ms`;
      } catch (error) {
        result.textContent = "";
        throw error;
      } finally {
        modelInput.disabled = false;
        key.disabled = false;
      }
    });
    controls.append(field("API key", key), test, result);
  }
}

const nameOf = (path: string) => path.split("/").filter(Boolean).at(-1) ?? path;
const bytes = (size: number) => size < 1024 * 1024
  ? `${Math.round(size / 1024)} KB`
  : `${(size / (1024 * 1024)).toFixed(1)} MB`;

function sourceStatus(source: Source) {
  if (typeof source.status !== "string") return source.status.Failed;
  if (source.status !== "Idle") return source.status === "Running" ? "Indexing…" : source.status;
  if (!source.last_ingested_at) return "Never indexed";
  const minutes = Math.max(0, Math.floor((Date.now() / 1000 - source.last_ingested_at) / 60));
  const when = minutes < 1 ? "just now" : minutes < 60 ? `${minutes} min ago`
    : minutes < 1440 ? `${Math.floor(minutes / 60)} h ago` : `${Math.floor(minutes / 1440)} d ago`;
  return `Indexed ${when} · ${source.file_count} files · ${bytes(source.size)}`;
}

async function sourceChange(id: number, change: Record<string, unknown>) {
  await invoke("change_source", { id, change });
  await refreshSources();
}

async function folders(parent: HTMLElement, source: Source, path = "") {
  const entries = await invoke<[string, string, boolean][]>("source_folders", { id: source.id, path });
  parent.replaceChildren();
  if (!entries.length) parent.append(hint("No subfolders"));
  for (const [name, childPath, excluded] of entries) {
    const details = node("details");
    const summary = node("summary", name);
    const nested = node("div");
    nested.className = "folder";
    details.append(summary, nested);
    details.ontoggle = async () => {
      if (!details.open || nested.childElementCount) return;
      const include = checkbox("Include folder", !excluded, async (checked) => {
        await sourceChange(source.id, { kind: "exclude", path: childPath, excluded: !checked });
      });
      const children = node("div");
      nested.append(include, children);
      try { await folders(children, source, childPath); } catch (error) { fail(error); }
    };
    parent.append(details);
  }
}

const sourceCards = new Map<number, HTMLElement>();

function syncSourceList(list: HTMLElement) {
  const present = new Set(snapshot.sources.map(({ id }) => id));
  for (const [id, card] of sourceCards) {
    if (!present.has(id)) { card.remove(); sourceCards.delete(id); }
  }
  list.querySelector(".empty")?.remove();
  if (!snapshot.sources.length) {
    const empty = hint("Add a folder or a file to start indexing your notes.");
    empty.classList.add("empty");
    list.append(empty);
  }
  for (const source of snapshot.sources) {
    let card = sourceCards.get(source.id);
    if (!card) {
      card = createSourceCard(source);
      list.append(card);
      sourceCards.set(source.id, card);
    }
    card.classList.toggle("disabled-source", !source.enabled);
    card.querySelector<HTMLElement>(".source-status")!.textContent = sourceStatus(source);
    const enabled = card.querySelector<HTMLInputElement>(".use-source input")!;
    if (!enabled.disabled) enabled.checked = source.enabled;
    const active = source.status === "Running" || source.status === "Queued";
    card.querySelectorAll<HTMLButtonElement>("[data-idle]").forEach((control) => { control.hidden = active; });
    card.querySelector<HTMLButtonElement>("[data-cancel]")!.hidden = !active;
    const failed = card.querySelector<HTMLButtonElement>("[data-failed]")!;
    failed.hidden = source.failed_count === 0;
    failed.textContent = `Failed files (${source.failed_count})`;
  }
}

function createSourceCard(source: Source) {
  const card = node("section");
  card.className = "source-card";
  const title = node("h2", nameOf(source.path));
  const path = hint(source.path);
  const sourceState = hint("");
  sourceState.classList.add("source-status");
  const enabled = checkbox("Use in answers", source.enabled, (value) =>
    sourceChange(source.id, { kind: "enabled", enabled: value }));
  enabled.classList.add("use-source");
  const isCode = source.kind === "Code";
  if (isCode) {
    // Code repositories never join note answers; they are asked with @name.
    enabled.hidden = true;
    enabled.querySelector("input")!.disabled = true;
    title.textContent = `${nameOf(source.path)} · code`;
  }
  const reingest = button("Re-ingest", () => sourceChange(source.id, { kind: "reingest", force: false }));
  const force = button("Force rebuild", () => sourceChange(source.id, { kind: "reingest", force: true }));
  reingest.dataset.idle = "";
  force.dataset.idle = "";
  const cancel = button("Cancel ingestion", () => sourceChange(source.id, { kind: "cancel" }));
  cancel.dataset.cancel = "";
  const remove = button("Remove", async () => {
    if (await confirmAction(`Remove ${nameOf(source.path)} and its index?`)) {
      await sourceChange(source.id, { kind: "remove" });
    }
  });
  const failedList = node("ul");
  failedList.hidden = true;
  const failures = button("", async () => {
    if (!failedList.hidden) { failedList.hidden = true; return; }
    const files = await invoke<{ path: string; error: string }[]>("failed_files", { id: source.id });
    failedList.replaceChildren(...files.map(({ path, error }) => node("li", `${nameOf(path)} · ${error}`)));
    failedList.hidden = false;
  });
  failures.dataset.failed = "";
  card.append(title, path, sourceState, enabled, row(reingest, force, cancel, remove, failures), failedList);
  if (isCode) {
    card.append(hint(`Ask about it in the panel with @${nameOf(source.path)}. Coding agents search it through accio-mcp. Tests, build output and dependencies are excluded; adjust under Subfolders.`));
  }
  if (!isCode && (!source.is_file || source.path.toLowerCase().endsWith(".pdf"))) {
    const mode = select(source.pdf_mode, [["Text", "Plain text"], ["Markdown", "Markdown"]]);
    mode.onchange = async () => {
      try { await sourceChange(source.id, { kind: "pdf", markdown: mode.value === "Markdown" }); }
      catch (error) { mode.value = source.pdf_mode; fail(error); }
    };
    card.append(field("PDF extraction", mode));
  }
  if (!isCode && (!source.is_file || /\.(png|jpe?g|heic|webp)$/i.test(source.path))) {
    card.append(checkbox("Generate image captions", source.captions ?? false, (value) =>
      sourceChange(source.id, { kind: "captions", enabled: value })));
  }
  if (!source.is_file) {
    const tree = node("details");
    const children = node("div");
    children.className = "folder";
    tree.append(node("summary", "Subfolders"), children);
    tree.ontoggle = () => {
      if (tree.open && !children.childElementCount) void folders(children, source).catch(fail);
    };
    card.append(tree);
  }
  return card;
}

async function confirmAction(message: string) {
  const dialog = node("dialog");
  const cancel = button("Cancel", () => dialog.close());
  cancel.autofocus = true;
  dialog.append(node("p", message), row(cancel, button("Confirm", () => dialog.close("yes"))));
  document.body.append(dialog);
  return new Promise<boolean>((resolve) => {
    dialog.addEventListener("close", () => {
      resolve(dialog.returnValue === "yes");
      dialog.remove();
    }, { once: true });
    dialog.showModal();
  });
}

function renderSources() {
  sourceCards.clear();
  const list = node("div");
  list.id = "source-list";
  content.append(row(
    button("Add folder…", () => invoke("pick_source", { files: false })),
    button("Add files…", () => invoke("pick_source", { files: true })),
    button("Add code repository…", () => invoke("pick_code_repository")),
  ), list);
  syncSourceList(list);
}

async function refreshSources() {
  await load();
  const list = document.querySelector<HTMLElement>("#source-list");
  if (list) syncSourceList(list);
}

function renderProfiles() {
  const prefs = snapshot.preferences;
  const profileOptions: [string, string][] = snapshot.profiles.map(({ id, name }) => [String(id), name]);
  const defaultProfile = select(String(prefs.default_profile ?? ""), [["", "None"], ...profileOptions]);
  defaultProfile.onchange = () => {
    void update({ kind: "default_profile", id: Number(defaultProfile.value) || null })
      .then(load).catch(fail);
  };
  const add = node("input");
  add.placeholder = "Profile name";
  add.setAttribute("aria-label", "Profile name");
  const create = button("Create from enabled sources", async () => {
    await invoke("save_profile", { id: null, name: add.value,
      sources: snapshot.sources.filter(({ enabled }) => enabled).map(({ id }) => id) });
    await load();
    render();
  });
  content.append(field("Default at launch", defaultProfile), row(add, create),
    hint("Type @ followed by a profile name in the chat panel to switch to it."));
  for (const profile of snapshot.profiles) {
    const card = node("section");
    card.className = "source-card";
    const name = node("input");
    name.value = profile.name;
    const members = new Set(profile.sources);
    async function save() {
      await invoke("save_profile", { id: profile.id, name: name.value, sources: [...members] });
      profile.name = name.value.trim();
      const option = [...defaultProfile.options].find(({ value }) => value === String(profile.id));
      if (option) option.textContent = profile.name;
    }
    name.onchange = () => { void save().catch(fail); };
    const details = node("details");
    details.append(node("summary", "Sources in this profile"));
    for (const source of snapshot.sources) {
      details.append(checkbox(nameOf(source.path), members.has(source.id), async (checked) => {
        if (checked) members.add(source.id);
        else members.delete(source.id);
        try { await save(); }
        catch (error) {
          if (checked) members.delete(source.id);
          else members.add(source.id);
          throw error;
        }
      }));
    }
    card.append(field("Name", name), details, row(button("Use profile", async () => {
      await invoke("use_profile", { id: profile.id });
      status(`Using ${profile.name}`);
    }), button("Delete", async () => {
      if (!await confirmAction(`Delete ${profile.name}?`)) return;
      await invoke("delete_profile", { id: profile.id });
      await load();
      render();
    })));
    content.append(card);
  }
}

function renderSearch() {
  const prefs = snapshot.preferences;
  const slider = node("input");
  slider.type = "range";
  slider.min = "0";
  slider.max = "100";
  slider.value = String(Math.round((1 - prefs.min_similarity) * 100));
  const excerpts = number(prefs.excerpts, 1, 10);
  const memory = number(prefs.memory, 0, 6);
  let prefetch = prefs.prefetch;
  async function save() {
    if (!excerpts.checkValidity() || !memory.checkValidity()) throw new Error("Check the number of excerpts and turns.");
    await update({ kind: "search", similarity: 1 - slider.valueAsNumber / 100,
      excerpts: excerpts.valueAsNumber, memory: memory.valueAsNumber, prefetch });
  }
  for (const control of [slider, excerpts, memory]) control.onchange = () => { void save().catch(fail); };
  content.append(field("Match notes", row(node("span", "Stricter"), slider, node("span", "Looser"))),
    field("Excerpts sent", excerpts), field("Conversation memory (turns)", memory),
    checkbox("Retrieve while typing", prefetch, async (value) => {
      prefetch = value;
      try { await save(); } catch (error) { prefetch = !value; throw error; }
    }));
}

function renderPanel() {
  const prefs = snapshot.preferences;
  const shortcut = node("input");
  shortcut.value = prefs.shortcut;
  shortcut.onchange = async () => {
    try { await update({ kind: "shortcut", value: shortcut.value }); }
    catch (error) { fail(error); }
  };
  const width = number(prefs.panel_width, 480, 1600);
  const height = number(prefs.panel_height, 320, 1000);
  const reset = number(prefs.reset_minutes, 0, 120);
  async function save() {
    if ([width, height, reset].some((input) => !input.checkValidity())) throw new Error("Check the panel dimensions and reset time.");
    await update({ kind: "panel", width: width.valueAsNumber, height: height.valueAsNumber,
      reset_minutes: reset.valueAsNumber });
  }
  for (const control of [width, height, reset]) control.onchange = () => { void save().catch(fail); };
  const login = checkbox("Launch at login", [1, 2].includes(snapshot.launch_status), async (enabled) => {
    const value = await invoke<number>("launch_at_login", { enabled });
    if (value === 2) status("Allow Accio in System Settings under Login Items.");
  });
  if (snapshot.launch_status < 0 || snapshot.launch_status === 3) {
    login.querySelector("input")!.disabled = true;
    login.title = "Launch at login requires an installed Accio.app bundle.";
  }
  content.append(field("Global shortcut", shortcut), hint("For example, Alt+Space or Control+Shift+A."),
    field("Width (points)", width), field("Height (points)", height), field("Reset after hidden (minutes)", reset),
    hint("Set reset time to 0 to keep the conversation until you clear it."), login);
}

function renderPrivacy() {
  const prompt = node("pre");
  prompt.hidden = true;
  content.append(button("Inspect last prompt", async () => {
    const messages = await invoke<{ role: string; content: string }[]>("last_prompt");
    prompt.textContent = messages.length
      ? messages.map(({ role, content }) => `[${role}]\n${content}`).join("\n\n")
      : "No question has been sent in this session.";
    prompt.hidden = false;
  }), prompt);
}

function renderData() {
  const helper = hint(snapshot.embedder);
  void invoke<{ running: boolean; memory: number | null }>("embedding_status").then(({ running, memory }) => {
    helper.textContent = `${snapshot.embedder} · ${running && memory !== null ? `running · ${bytes(memory)} resident memory` : "memory unavailable"}`;
  }).catch(fail);
  content.append(node("h2", "Index"), hint(snapshot.index_path), hint(bytes(snapshot.index_size)),
    row(button("Reveal index", () => invoke("reveal_index")), button("Clear index…", async () => {
      if (!await confirmAction("Clear indexed excerpts for all sources? You can re-ingest them from Sources.")) return;
      await invoke("clear_index");
      await load();
      render();
    })),
    node("h2", "Embedding helper"), helper,
    button("Reveal logs", () => invoke("reveal_logs")));
}

for (const section of sections) nav.append(button(section, async () => {
  await load();
  render(section);
}));

let refreshTimer: ReturnType<typeof setTimeout>;
void listen("sources://changed", () => {
  if (current !== "Sources") return;
  clearTimeout(refreshTimer);
  refreshTimer = setTimeout(() => void refreshSources().catch(fail), 100);
});
void load().then(() => render()).catch(fail);
