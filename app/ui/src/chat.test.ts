import { expect, it, vi } from "vitest";
import { Chat, debounce, shouldShowCitations } from "./chat";

it("keeps the last six question and answer pairs", () => {
  const chat = new Chat();
  for (let i = 0; i < 8; i++) chat.push(`q${i}`, `a${i}`);
  const turns = chat.recentTurns().map(({ content }) => content);
  expect(turns).toHaveLength(12);
  expect(turns.slice(0, 2)).toEqual(["q2", "a2"]);
  expect(turns.slice(-2)).toEqual(["q7", "a7"]);
  chat.reset();
  expect(chat.recentTurns()).toEqual([]);
});

it("carries the previous answer's note chunks into a follow-up", () => {
  const chat = new Chat();
  expect(chat.carry()).toEqual([]);
  chat.push("q1", "Laut Notiz [1] ...", [11, 12]);
  expect(chat.carry()).toEqual([11, 12]);
  chat.push("q2", "(not in notes) Canberra", [13]);
  expect(chat.carry()).toEqual([]);
  chat.push("q3", "Siehe [2]", [21]);
  chat.reset();
  expect(chat.carry()).toEqual([]);
});

it("resets only after more than ten minutes hidden", () => {
  const chat = new Chat();
  expect(chat.shouldReset(600_000)).toBe(false);
  expect(chat.shouldReset(600_001)).toBe(true);
});

it("omits citations when the answer uses general knowledge", () => {
  expect(shouldShowCitations("", 3)).toBe(false);
  expect(shouldShowCitations(" (not in", 3)).toBe(false);
  expect(shouldShowCitations("(not in notes) Canberra", 3)).toBe(false);
  expect(shouldShowCitations("The plan says [1].", 3)).toBe(true);
});

it("shows citations when a general-prefixed answer still cites an existing note", () => {
  expect(shouldShowCitations("(not in notes)\n\nIm Projekt ... 【7】.", 8)).toBe(true);
  expect(shouldShowCitations("(not in notes) siehe [2]", 8)).toBe(true);
  expect(shouldShowCitations("(not in notes) siehe [9]", 8)).toBe(false); // no such note
  expect(shouldShowCitations("(not in notes) siehe [1]", 0)).toBe(false);
});

it("carries chunks when a general-prefixed answer cites notes", () => {
  const chat = new Chat();
  chat.push("q", "(not in notes) Laut Notiz 【1】", [5, 6]);
  expect(chat.carry()).toEqual([5, 6]);
});

it("prefetches the last input and can cancel the timer", () => {
  vi.useFakeTimers();
  try {
    const fetch = vi.fn();
    const typing = debounce(fetch, 150);
    typing("first");
    typing("second");
    vi.advanceTimersByTime(150);
    expect(fetch).toHaveBeenCalledExactlyOnceWith("second");
    typing("third");
    typing.cancel();
    vi.advanceTimersByTime(150);
    expect(fetch).toHaveBeenCalledTimes(1);
  } finally {
    vi.useRealTimers();
  }
});
