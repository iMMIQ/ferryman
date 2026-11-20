import { access, readdir, readFile } from "node:fs/promises";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const REQUIRED_DOCS = [
  "README.md",
  "README.zh-CN.md",
  "README.ja.md",
  "CHANGELOG.md",
  "docs/cli.md",
  "docs/formats.md",
  "docs/deployment.md",
  "docs/packaging.md",
  "docs/development.md",
];

export const EVERGREEN_DOCS = REQUIRED_DOCS.filter(
  (path) => path !== "CHANGELOG.md",
);

export const BANNED_LEGACY_PATTERNS = [
  { label: "--preset", pattern: /(^|[\s`'"(])--preset\b/ },
  { label: "--serve", pattern: /(^|[\s`'"(])--serve\b/ },
  { label: "ferryman-agent", pattern: /\bferryman-agent\b/ },
  { label: "Dockerfile.web", pattern: /\bDockerfile\.web\b/ },
  { label: "strip-ansi-escapes", pattern: /\bstrip-ansi-escapes\b/ },
  { label: "rustix", pattern: /\brustix\b/ },
];

const NON_RUNTIME_ENV_VARS = new Set([
  "FERRYMAN_TYPES_DIR",
  "FERRYMAN_PDF_PROBE",
  "FERRYMAN_PDF_TEST_OUTPUT",
]);

function stripCode(markdown) {
  return markdown
    .replace(/^```[\s\S]*?^```/gm, "")
    .replace(/^~~~[\s\S]*?^~~~/gm, "")
    .replace(/`[^`\n]*`/g, "");
}

export function extractLocalLinks(markdown) {
  const cleaned = stripCode(markdown);
  const links = [];
  const pattern = /!?\[[^\]]*\]\(([^)\s]+)(?:\s+"[^"]*")?\)/g;
  for (const match of cleaned.matchAll(pattern)) {
    const raw = match[1].replace(/^<|>$/g, "");
    if (!raw || raw.startsWith("#") || /^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(raw)) {
      continue;
    }
    const target = raw.split("#")[0].split("?")[0];
    if (target) links.push(target);
  }
  return links;
}

export function extractCliFlags(cliSource) {
  const structMatch = /struct\s+Cli\s*\{([\s\S]*?)^\}/m.exec(cliSource);
  if (!structMatch) {
    throw new Error("Could not locate `struct Cli` in CLI source");
  }
  const body = structMatch[1];
  const flags = [];
  const lines = body.split("\n");
  let sawArg = false;
  let explicitLong = null;
  for (const line of lines) {
    const trimmed = line.trim();
    if (trimmed.startsWith("#[arg(")) {
      sawArg = true;
      const custom = /long\s*=\s*"([^"]+)"/.exec(trimmed);
      if (custom) explicitLong = custom[1];
      continue;
    }
    if (
      !sawArg ||
      trimmed.startsWith("///") ||
      trimmed.startsWith("//") ||
      !trimmed
    ) {
      continue;
    }
    const field = /^([a-z_][a-z0-9_]*)\s*:/.exec(trimmed);
    if (field) {
      const flag = explicitLong ?? field[1].replaceAll("_", "-");
      flags.push(`--${flag}`);
      sawArg = false;
      explicitLong = null;
    }
  }
  return flags;
}

export function stripTestModule(rustSource) {
  let out = "";
  let cursor = 0;
  const pattern =
    /#\[cfg\(test\)\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+[a-zA-Z0-9_]+\s*\{/g;
  for (const match of rustSource.matchAll(pattern)) {
    const start = match.index ?? 0;
    if (start < cursor) continue;
    out += rustSource.slice(cursor, start);
    let depth = 0;
    let i = start + match[0].length - 1;
    for (; i < rustSource.length; i++) {
      if (rustSource[i] === "{") depth++;
      else if (rustSource[i] === "}") {
        depth--;
        if (depth === 0) {
          i++;
          break;
        }
      }
    }
    cursor = i;
  }
  out += rustSource.slice(cursor);
  return out;
}

export function extractRuntimeEnvVars(rustSource) {
  const nonTest = stripTestModule(rustSource);
  const vars = new Set();
  const pattern = /env::var(?:_os)?\s*\(\s*"(FERRYMAN_[A-Z0-9_]+)"/g;
  for (const match of nonTest.matchAll(pattern)) {
    if (!NON_RUNTIME_ENV_VARS.has(match[1])) {
      vars.add(match[1]);
    }
  }
  return [...vars].sort();
}

export function findLegacyReferences(markdown) {
  const hits = [];
  for (const { label, pattern } of BANNED_LEGACY_PATTERNS) {
    if (pattern.test(markdown)) {
      hits.push(label);
    }
  }
  return hits;
}

async function listRustSourceFiles(dir) {
  const out = [];
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (entry.name === "tests") continue;
      out.push(...(await listRustSourceFiles(full)));
    } else if (
      entry.isFile() &&
      entry.name.endsWith(".rs") &&
      entry.name !== "tests.rs" &&
      entry.name !== "regression_tests.rs"
    ) {
      out.push(full);
    }
  }
  return out;
}

async function exists(path) {
  try {
    await access(path);
    return true;
  } catch {
    return false;
  }
}

export async function checkDocs(repoRoot = process.cwd()) {
  const failures = [];
  const docContents = new Map();

  for (const rel of REQUIRED_DOCS) {
    const full = join(repoRoot, rel);
    if (!(await exists(full))) {
      failures.push(`Missing required documentation file: ${rel}`);
      continue;
    }
    docContents.set(rel, await readFile(full, "utf8"));
  }

  for (const [rel, content] of docContents) {
    const full = join(repoRoot, rel);
    for (const target of extractLocalLinks(content)) {
      const resolved = resolve(dirname(full), target);
      if (!(await exists(resolved))) {
        failures.push(
          `${rel}: broken local link -> ${target} (${relative(repoRoot, resolved)})`,
        );
      }
    }
  }

  for (const rel of EVERGREEN_DOCS) {
    const content = docContents.get(rel);
    if (!content) continue;
    for (const legacy of findLegacyReferences(content)) {
      failures.push(`${rel}: stale legacy reference "${legacy}"`);
    }
  }

  const cliSource = await readFile(
    join(repoRoot, "crates/ferryman-cli/src/main.rs"),
    "utf8",
  );
  const cliDoc = docContents.get("docs/cli.md") ?? "";
  for (const flag of extractCliFlags(cliSource)) {
    if (!cliDoc.includes(flag)) {
      failures.push(`docs/cli.md: missing CLI flag ${flag}`);
    }
  }

  const rustFiles = await listRustSourceFiles(join(repoRoot, "crates"));
  const envVars = new Set();
  for (const file of rustFiles) {
    const src = await readFile(file, "utf8");
    for (const name of extractRuntimeEnvVars(src)) {
      envVars.add(name);
    }
  }
  const envDocCorpus = [
    docContents.get("docs/cli.md") ?? "",
    docContents.get("docs/deployment.md") ?? "",
    docContents.get("docs/packaging.md") ?? "",
  ].join("\n");
  for (const envVar of [...envVars].sort()) {
    if (!envDocCorpus.includes(envVar)) {
      failures.push(
        `Missing documentation for runtime environment variable ${envVar}`,
      );
    }
  }

  return {
    failures,
    checkedDocs: docContents.size,
    checkedFlags: extractCliFlags(cliSource).length,
    checkedEnvVars: envVars.size,
  };
}

if (resolve(process.argv[1] ?? "") === fileURLToPath(import.meta.url)) {
  const result = await checkDocs();
  if (result.failures.length > 0) {
    for (const failure of result.failures) {
      console.error(`ERROR: ${failure}`);
    }
    process.exit(1);
  }
  console.log(
    `Documentation check passed (${result.checkedDocs} docs, ${result.checkedFlags} CLI flags, ${result.checkedEnvVars} env vars).`,
  );
}
