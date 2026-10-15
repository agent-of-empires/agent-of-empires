/// Code-fence language for a path, or "" when unknown.

const MAP: Record<string, string> = {
  rs: "rust",
  ts: "ts",
  tsx: "tsx",
  js: "js",
  jsx: "jsx",
  py: "python",
  rb: "ruby",
  go: "go",
  java: "java",
  kt: "kotlin",
  swift: "swift",
  c: "c",
  h: "c",
  cc: "cpp",
  cpp: "cpp",
  cs: "csharp",
  php: "php",
  pl: "perl",
  lua: "lua",
  sh: "bash",
  bash: "bash",
  zsh: "bash",
  fish: "bash",
  ps1: "powershell",
  toml: "toml",
  yaml: "yaml",
  yml: "yaml",
  json: "json",
  md: "markdown",
  html: "html",
  css: "css",
  scss: "scss",
  sql: "sql",
  proto: "proto",
  dockerfile: "dockerfile",
};

export function extensionToLanguage(filePath: string): string {
  const lower = filePath.toLowerCase();
  const base = lower.split("/").pop() ?? lower;
  if (base === "dockerfile") return "dockerfile";
  const dot = base.lastIndexOf(".");
  if (dot < 0) return "";
  const ext = base.slice(dot + 1);
  return MAP[ext] ?? "";
}
