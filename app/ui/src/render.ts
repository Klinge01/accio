import hljs from "highlight.js/lib/core";
import bash from "highlight.js/lib/languages/bash";
import javascript from "highlight.js/lib/languages/javascript";
import json from "highlight.js/lib/languages/json";
import python from "highlight.js/lib/languages/python";
import rust from "highlight.js/lib/languages/rust";
import sql from "highlight.js/lib/languages/sql";
import typescript from "highlight.js/lib/languages/typescript";
import katex from "katex";
import MarkdownIt from "markdown-it";
import texmath from "markdown-it-texmath";

for (const [name, language] of Object.entries({ bash, javascript, json, python, rust, sql, typescript })) {
  hljs.registerLanguage(name, language);
}

const render: MarkdownIt = new MarkdownIt({
  html: false,
  highlight(code, language) {
    const value = language && hljs.getLanguage(language)
      ? hljs.highlight(code, { language, ignoreIllegals: true }).value
      : render.utils.escapeHtml(code);
    return `<pre class="hljs"><code>${value}</code></pre>`;
  },
});

render.use(texmath, {
  engine: katex,
  delimiters: "dollars",
  katexOptions: { throwOnError: false },
});

const fence = render.renderer.rules.fence!;
render.renderer.rules.fence = (tokens, index, options, env, self) => {
  if (tokens[index].info.trim() === "mermaid") {
    return `<div class="mermaid">${render.utils.escapeHtml(tokens[index].content)}</div>\n`;
  }
  return fence(tokens, index, options, env, self);
};

render.core.ruler.push("citations", (state) => {
  const count: number = state.env.citeCount;
  if (!count) return;
  for (const token of state.tokens) {
    if (!token.children) continue;
    let linkDepth = 0;
    token.children = token.children.flatMap((child) => {
      if (child.type === "link_open") linkDepth++;
      if (child.type === "link_close") linkDepth--;
      if (child.type !== "text" || linkDepth > 0) return [child];

      const parts = [];
      let cursor = 0;
      for (const match of child.content.matchAll(/[[【]([1-9]\d*)[\]】]/g)) {
        const n = Number(match[1]);
        if (n > count) continue;
        const index = match.index!;
        if (index > cursor) {
          const text = new state.Token("text", "", 0);
          text.content = child.content.slice(cursor, index);
          parts.push(text);
        }
        const cite = new state.Token("note_citation", "a", 0);
        cite.content = match[1];
        parts.push(cite);
        cursor = index + match[0].length;
      }
      if (cursor === 0) return [child];
      if (cursor < child.content.length) {
        const text = new state.Token("text", "", 0);
        text.content = child.content.slice(cursor);
        parts.push(text);
      }
      return parts;
    });
  }
});

render.renderer.rules.note_citation = (tokens, index) => {
  const n = tokens[index].content;
  return `<a class="cite" data-n="${n}" href="#citation-${n}">[${n}]</a>`;
};

export function renderMarkdown(markdown: string, citeCount: number): string {
  return render.render(markdown, { citeCount });
}
