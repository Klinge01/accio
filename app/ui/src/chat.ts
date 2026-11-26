export interface Turn {
  role: "user" | "assistant";
  content: string;
}

/** Last 6 question/answer pairs; mirrors HISTORY_MESSAGES in crates/core/src/prompt.rs. */
const HISTORY_MESSAGES = 12;

/**
 * Ephemeral conversation state. Lives only in this page's memory: nothing is
 * persisted, and it is cleared by New chat (⌘N) or after 10 minutes hidden.
 */
export class Chat {
  history: Turn[] = [];
  private carried: number[] = [];

  /** Record a finished answer and the note chunks it was given. */
  push(question: string, answer: string, chunkIds: number[] = []): void {
    this.history.push({ role: "user", content: question }, { role: "assistant", content: answer });
    this.history = this.recentTurns();
    this.carried = shouldShowCitations(answer, chunkIds.length) ? chunkIds : [];
  }

  recentTurns(): Turn[] {
    return this.history.slice(-HISTORY_MESSAGES);
  }

  /** Chunk ids from the previous answer, so a follow-up keeps the same notes in view. */
  carry(): number[] {
    return [...this.carried];
  }

  reset(): void {
    this.history = [];
    this.carried = [];
  }

  shouldReset(hiddenMs: number): boolean {
    return hiddenMs > 600_000;
  }
}

/** Citation markers the renderer links: [n] and the full-width 【n】 gpt-oss emits. */
const CITATION = /[[【]([1-9]\d*)[\]】]/g;

/**
 * Whether an answer's sources should be shown. An answer that cites an existing note
 * counts as grounded even if the model also wrote the "(not in notes)" prefix.
 */
export function shouldShowCitations(answer: string, noteCount: number): boolean {
  if (noteCount === 0) return false;
  for (const match of answer.matchAll(CITATION)) {
    if (Number(match[1]) <= noteCount) return true;
  }
  const text = answer.trimStart().toLowerCase();
  const general = "(not in notes)";
  return !text.startsWith(general) && !general.startsWith(text);
}

export function debounce<Arguments extends unknown[]>(fn: (...args: Arguments) => void, ms: number) {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const call = (...args: Arguments) => {
    clearTimeout(timer);
    timer = setTimeout(() => fn(...args), ms);
  };
  call.cancel = () => clearTimeout(timer);
  return call;
}
