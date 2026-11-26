import { describe, expect, it } from "vitest";
import { renderMarkdown } from "./render";

describe("renderMarkdown", () => {
  it("renders inline math", () => {
    expect(renderMarkdown("$x^2$", 0)).toContain('class="katex"');
  });

  it("renders code highlight", () => {
    const html = renderMarkdown("```ts\nconst x = 1;\n```", 0);
    expect(html).toContain('class="hljs"');
    expect(html).toContain('class="hljs-keyword"');
  });

  it("linkifies citations in range and outside code or links", () => {
    const html = renderMarkdown("See [1] and [9]. `[2]` [reference [2]](https://example.com)", 2);
    expect(html.match(/class="cite"/g)).toHaveLength(1);
    expect(html).toContain('data-n="1"');
    expect(html).toContain("[9]");
    expect(html).toContain("<code>[2]</code>");
  });

  it("renders an escaped mermaid placeholder", () => {
    const html = renderMarkdown("```mermaid\ngraph TD\nA[<b>] --> B\n```", 0);
    expect(html).toContain('<div class="mermaid">');
    expect(html).toContain("A[&lt;b&gt;]");
  });
});

it("linkifies full-width citation brackets that gpt-oss emits", () => {
  const html = renderMarkdown("Das Gitter 【1】【2】 und 【9】.", 2);
  expect(html.match(/class="cite"/g)).toHaveLength(2);
  expect(html).toContain('data-n="1"');
  expect(html).toContain("【9】");
});
