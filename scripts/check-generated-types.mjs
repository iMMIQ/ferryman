import { execFileSync } from "node:child_process";
import { mkdtemp, readdir, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
const output = await mkdtemp(join(tmpdir(), "ferryman-types-"));
try {
  execFileSync(
    "cargo",
    [
      "test",
      "--locked",
      "--all-targets",
      "export_frontend_types",
      "--",
      "--ignored",
    ],
    {
      stdio: "inherit",
      env: { ...process.env, FERRYMAN_TYPES_DIR: output },
    },
  );
  const checkedIn = "web/src/lib/generated";
  const expected = (await readdir(output)).sort();
  const actual = (await readdir(checkedIn)).sort();
  const differences = [];
  for (const file of new Set([...expected, ...actual])) {
    if (
      !expected.includes(file) ||
      !actual.includes(file) ||
      !(await readFile(join(output, file))).equals(
        await readFile(join(checkedIn, file)),
      )
    )
      differences.push(file);
  }
  if (differences.length) {
    throw new Error(
      `Generated API types differ: ${differences.join(", ")}. Run npm run types:generate.`,
    );
  }
  console.log(`API contracts match Rust (${expected.length} files).`);
} finally {
  await rm(output, { recursive: true, force: true });
}
