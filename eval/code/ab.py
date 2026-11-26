#!/usr/bin/env python3
"""Does code_search help coding agents? Runs Claude Code headless on each question in
three arms and records time, tool calls, tokens, cost and the answer:

  baseline   no Accio tool
  tool       accio-mcp available
  tool_hint  accio-mcp plus one system-prompt line pointing at it

All arms run with --strict-mcp-config (only the MCP servers given here), read-only
tools, and the repository's own CLAUDE.md as usual.

    python3 eval/code/ab.py run    --repo ~/code/myrepo --mcp target/release/accio-mcp [--model sonnet] [--repeats 1] [--only q01,q02] [--arms baseline,tool]
    python3 eval/code/ab.py grade  eval/code/runs/<run>.jsonl [--model sonnet]
    python3 eval/code/ab.py report eval/code/runs/<run>.jsonl

`run` and `grade` call Claude and cost money/quota; nothing runs without being asked.
"""
import argparse
import json
import re
import statistics
import subprocess
import sys
import tempfile
import time
from collections import Counter, defaultdict
from pathlib import Path

HERE = Path(__file__).parent
ARMS = ("baseline", "tool", "tool_hint")
HINT = (
    "This repository is indexed for semantic code search. For questions about how the code "
    "works or where something lives, call the code_search tool first, then read the files it points to."
)
PROMPT_SUFFIX = (
    "\n\nAnswer from the actual code in this repository: name the concrete classes, methods and "
    "files involved. Do not modify anything."
)
ALLOWED = [
    "Read", "Grep", "Glob", "Bash(git ls-files:*)", "Bash(git grep:*)", "Bash(grep:*)",
    "Bash(rg:*)", "Bash(find:*)", "Bash(ls:*)", "mcp__accio__code_search",
]
DISALLOWED = ["Edit", "Write", "NotebookEdit"]


def load_questions(path: Path, only: set[str] | None):
    rows = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
    return [row for row in rows if not only or row["id"] in only]


def mcp_config(directory: Path, binary: Path | None) -> Path:
    servers = {"accio": {"command": str(binary.resolve()), "args": []}} if binary else {}
    path = directory / ("with-accio.json" if binary else "empty.json")
    path.write_text(json.dumps({"mcpServers": servers}))
    return path


def run_once(question: dict, arm: str, args, configs: dict) -> dict:
    command = [
        "claude", "-p", question["q"] + PROMPT_SUFFIX,
        "--output-format", "stream-json", "--verbose",
        "--model", args.model,
        "--strict-mcp-config", "--mcp-config", str(configs[arm != "baseline"]),
        "--allowedTools", *ALLOWED,
        "--disallowedTools", *DISALLOWED,
    ]
    if arm == "tool_hint":
        command += ["--append-system-prompt", HINT]
    started = time.time()
    try:
        process = subprocess.run(
            command, cwd=args.repo, capture_output=True, text=True, timeout=args.timeout
        )
        lines = process.stdout.splitlines()
        error = process.stderr.strip()[-500:] if process.returncode else None
    except subprocess.TimeoutExpired as timeout:
        lines = (timeout.stdout or b"").decode(errors="replace").splitlines() if isinstance(timeout.stdout, bytes) else (timeout.stdout or "").splitlines()
        error = f"timeout after {args.timeout}s"
    tools: Counter = Counter()
    final: dict = {}
    for line in lines:
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") == "assistant":
            for block in event.get("message", {}).get("content", []):
                if block.get("type") == "tool_use":
                    tools[block.get("name", "?")] += 1
        elif event.get("type") == "result":
            final = event
    usage = final.get("usage", {})
    return {
        "id": question["id"], "kind": question["kind"], "arm": arm,
        "wall_s": round(time.time() - started, 1),
        "duration_ms": final.get("duration_ms"),
        "turns": final.get("num_turns"),
        "cost_usd": final.get("total_cost_usd"),
        "input_tokens": usage.get("input_tokens", 0)
        + usage.get("cache_read_input_tokens", 0)
        + usage.get("cache_creation_input_tokens", 0),
        "output_tokens": usage.get("output_tokens", 0),
        "tools": dict(tools),
        "tool_calls": sum(tools.values()),
        "code_search_calls": tools.get("mcp__accio__code_search", 0),
        "answer": final.get("result", ""),
        "error": error or (None if final else "no result event"),
    }


def command_run(args):
    questions = load_questions(args.questions, set(args.only.split(",")) if args.only else None)
    arms = args.arms.split(",")
    unknown = set(arms) - set(ARMS)
    if unknown:
        sys.exit(f"unknown arms: {', '.join(sorted(unknown))}")
    output = args.out or HERE / "runs" / f"{time.strftime('%Y%m%d-%H%M%S')}-{args.model}.jsonl"
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as directory:
        configs = {False: mcp_config(Path(directory), None), True: mcp_config(Path(directory), args.mcp)}
        total = len(questions) * len(arms) * args.repeats
        done = 0
        with output.open("a") as sink:
            for repeat in range(args.repeats):
                for question in questions:
                    # Alternate arm order so cache warmth does not favour one arm.
                    order = arms if (repeat + int(question["id"][1:])) % 2 == 0 else arms[::-1]
                    for arm in order:
                        result = run_once(question, arm, args, configs)
                        result["repeat"] = repeat
                        sink.write(json.dumps(result, ensure_ascii=False) + "\n")
                        sink.flush()
                        done += 1
                        print(
                            f"[{done}/{total}] {question['id']} {arm:<9} {result['wall_s']:6.1f}s "
                            f"tools {result['tool_calls']:>2} (code_search {result['code_search_calls']}) "
                            f"{result['error'] or ''}",
                            flush=True,
                        )
    print(f"\nwrote {output}")


GRADER = """You grade answers to questions about a codebase against a reference answer written by someone who read the code.

Question: {q}

Reference answer: {reference}

Answer to grade:
<answer>
{answer}
</answer>

Score 3 if the answer names the same essential classes/methods/flow as the reference and contains no substantial errors; 2 if it is partly right (main element found but important parts missing or one clear error); 1 if it is wrong, mostly missing, or only speculative. Extra correct detail beyond the reference is fine.
Reply with only a JSON object: {{"score": 1|2|3, "reason": "<one sentence>"}}"""


def command_grade(args):
    questions = {row["id"]: row for row in load_questions(args.questions, None)}
    rows = [json.loads(line) for line in args.run.read_text().splitlines() if line.strip()]
    for row in rows:
        if "score" in row:
            continue
        if not row.get("answer"):
            row["score"], row["reason"] = 1, "no answer"
            continue
        question = questions[row["id"]]
        prompt = GRADER.format(q=question["q"], reference=question["reference"], answer=row["answer"])
        process = subprocess.run(
            ["claude", "-p", prompt, "--output-format", "json", "--model", args.model,
             "--strict-mcp-config", "--tools", ""],
            capture_output=True, text=True, timeout=300,
        )
        text = json.loads(process.stdout or "{}").get("result", "")
        match = re.search(r"\{.*\}", text, re.S)
        verdict = json.loads(match.group(0)) if match else {"score": None, "reason": text[:200]}
        row["score"], row["reason"] = verdict.get("score"), verdict.get("reason")
        print(f"{row['id']} {row['arm']:<9} score {row['score']}  {row['reason']}", flush=True)
    args.run.write_text("".join(json.dumps(row, ensure_ascii=False) + "\n" for row in rows))


def command_report(args):
    rows = [json.loads(line) for line in args.run.read_text().splitlines() if line.strip()]
    by_arm = defaultdict(list)
    for row in rows:
        by_arm[row["arm"]].append(row)

    def mean(values):
        values = [value for value in values if value is not None]
        return statistics.mean(values) if values else float("nan")

    def median(values):
        values = [value for value in values if value is not None]
        return statistics.median(values) if values else float("nan")

    print("| arm | runs | median time | mean tool calls | code_search | input tokens | cost | score (1-3) | errors |")
    print("|---|---|---|---|---|---|---|---|---|")
    for arm in ARMS:
        group = by_arm.get(arm)
        if not group:
            continue
        print(
            f"| {arm} | {len(group)} | {median([r['wall_s'] for r in group]):.0f} s | "
            f"{mean([r['tool_calls'] for r in group]):.1f} | {mean([r['code_search_calls'] for r in group]):.1f} | "
            f"{mean([r['input_tokens'] for r in group]):,.0f} | ${sum(r['cost_usd'] or 0 for r in group):.2f} | "
            f"{mean([r.get('score') for r in group]):.2f} | {sum(1 for r in group if r['error'])} |"
        )
    print("\nPer question (median seconds / mean score):\n")
    ids = sorted({row["id"] for row in rows})
    print("| id | kind | " + " | ".join(arm for arm in ARMS if arm in by_arm) + " |")
    print("|---|---|" + "---|" * len(by_arm))
    for question_id in ids:
        cells = []
        kind = next(row["kind"] for row in rows if row["id"] == question_id)
        for arm in ARMS:
            group = [row for row in by_arm.get(arm, []) if row["id"] == question_id]
            if arm in by_arm:
                cells.append(
                    f"{median([r['wall_s'] for r in group]):.0f} s / {mean([r.get('score') for r in group]):.1f}"
                    if group else "-"
                )
        print(f"| {question_id} | {kind} | " + " | ".join(cells) + " |")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    run = commands.add_parser("run")
    run.add_argument("--repo", type=Path, required=True)
    run.add_argument("--mcp", type=Path, required=True, help="path to the accio-mcp binary")
    run.add_argument("--questions", type=Path, default=HERE / "questions.jsonl")
    run.add_argument("--model", default="sonnet")
    run.add_argument("--repeats", type=int, default=1)
    run.add_argument("--arms", default=",".join(ARMS))
    run.add_argument("--only", help="comma-separated question ids")
    run.add_argument("--timeout", type=int, default=900)
    run.add_argument("--out", type=Path)
    run.set_defaults(handler=command_run)
    grade = commands.add_parser("grade")
    grade.add_argument("run", type=Path)
    grade.add_argument("--questions", type=Path, default=HERE / "questions.jsonl")
    grade.add_argument("--model", default="sonnet")
    grade.set_defaults(handler=command_grade)
    report = commands.add_parser("report")
    report.add_argument("run", type=Path)
    report.set_defaults(handler=command_report)
    args = parser.parse_args()
    args.handler(args)


if __name__ == "__main__":
    main()
