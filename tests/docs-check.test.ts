import { describe, expect, test } from "bun:test";
import {
  checkDocs,
  extractCliFlags,
  extractLocalLinks,
  extractRuntimeEnvVars,
  findLegacyReferences,
} from "../scripts/check-docs.mjs";

describe("documentation drift checker", () => {
  test("extracts local links while ignoring URLs, anchors, and code blocks", () => {
    const md = [
      "See [CLI](docs/cli.md#options) and [Web](https://example.com) and [Top](#top).",
      "Inline `[fake](missing.md)` should be ignored.",
      "```md",
      "[also-fake](missing2.md)",
      "```",
      "![icon](icon.png)",
    ].join("\n");
    expect(extractLocalLinks(md)).toEqual(["docs/cli.md", "icon.png"]);
  });

  test("extracts CLI long flags from clap Cli struct", () => {
    const src = `
#[derive(Parser)]
struct Cli {
    /// Input path
    #[arg(long, short = 'i')]
    input: PathBuf,
    #[arg(long)]
    in_place: bool,
    #[arg(long = "custom-name")]
    aliased: String,
}
`;
    expect(extractCliFlags(src)).toEqual([
      "--input",
      "--in-place",
      "--custom-name",
    ]);
  });

  test("extracts runtime FERRYMAN_* env vars outside test modules", () => {
    const src = `
#[cfg(test)]
use std::path::Path;
fn main() {
    let _ = std::env::var("FERRYMAN_SECRET_KEY");
    let _ = env::var_os("FERRYMAN_PDF_DEBUG");
    let _ = std::env::var("FERRYMAN_TYPES_DIR");
}
#[cfg(test)]
mod tests {
    fn helper() {
        let _ = std::env::var("FERRYMAN_TEST_ONLY");
    }
}
`;
    expect(extractRuntimeEnvVars(src)).toEqual([
      "FERRYMAN_PDF_DEBUG",
      "FERRYMAN_SECRET_KEY",
    ]);
  });

  test("detects removed legacy references", () => {
    expect(
      findLegacyReferences("Run with `--preset foo` and `ferryman-agent`."),
    ).toEqual(["--preset", "ferryman-agent"]);
    expect(findLegacyReferences("Use `--adapter index-translate`.")).toEqual(
      [],
    );
  });

  test("passes on the current repository documentation", async () => {
    const result = await checkDocs();
    expect(result.failures).toEqual([]);
    expect(result.checkedDocs).toBeGreaterThanOrEqual(9);
    expect(result.checkedFlags).toBeGreaterThanOrEqual(15);
    expect(result.checkedEnvVars).toBeGreaterThanOrEqual(10);
  });
});
