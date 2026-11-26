declare module "markdown-it-texmath" {
  import type MarkdownIt from "markdown-it";
  import type katex from "katex";

  const plugin: MarkdownIt.PluginWithOptions<{
    engine: typeof katex;
    delimiters: string;
    katexOptions?: { throwOnError?: boolean };
  }>;
  export default plugin;
}
