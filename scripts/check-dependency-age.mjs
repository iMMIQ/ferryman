import { resolve } from "node:path";

export const minimumAgeMs = 7 * 24 * 60 * 60 * 1000;

export function checkPublishedAt(label, publishedAt, cutoff) {
  const timestamp = Date.parse(publishedAt);
  if (!Number.isFinite(timestamp)) {
    throw new Error(
      `${label}: registry did not provide a valid publication date`,
    );
  }
  if (timestamp > cutoff) {
    throw new Error(
      `${label}: published ${publishedAt}, less than seven days ago`,
    );
  }
}

export function registryPath(name) {
  if (name.length === 1) return `1/${name}`;
  if (name.length === 2) return `2/${name}`;
  if (name.length === 3) return `3/${name[0]}/${name}`;
  return `${name.slice(0, 2)}/${name.slice(2, 4)}/${name}`;
}

async function fetchText(url) {
  // curl preserves standard proxy/NO_PROXY settings and bounds the entire body
  // download, including retries, rather than just waiting for response headers.
  const child = Bun.spawn(
    [
      "curl",
      "--fail",
      "--location",
      "--silent",
      "--show-error",
      "--compressed",
      "--max-time",
      "60",
      "--retry",
      "2",
      "--retry-all-errors",
      url,
    ],
    { stdout: "pipe", stderr: "pipe" },
  );
  const [body, errors, status] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ]);
  if (status !== 0)
    throw new Error(`${url}: ${errors.trim() || `curl exited ${status}`}`);
  return body;
}

export async function checkLocks(now = Date.now()) {
  const cutoff = now - minimumAgeMs;
  if (!Number.isFinite(cutoff)) throw new Error("Invalid audit date");
  const cargo = Bun.TOML.parse(await Bun.file("Cargo.lock").text());
  const bun = Bun.JSONC.parse(await Bun.file("bun.lock").text());
  const groups = new Map();
  const failures = [];
  let count = 0;
  function add(ecosystem, name, version, integrity) {
    const key = `${ecosystem}:${name}`;
    if (!groups.has(key))
      groups.set(key, { ecosystem, name, versions: new Map() });
    groups.get(key).versions.set(version, integrity);
    count++;
  }
  for (const entry of cargo.package) {
    if (!entry.source) continue;
    if (
      entry.source !== "registry+https://github.com/rust-lang/crates.io-index"
    ) {
      throw new Error(`Cannot verify publication date for ${entry.source}`);
    }
    add("cargo", entry.name, entry.version, entry.checksum);
  }
  for (const entry of Object.values(bun.packages)) {
    const match = /^(@[^/]+\/[^@]+|[^@]+)@([0-9][^]*)$/.exec(entry[0]);
    if (!match)
      throw new Error(`Cannot verify publication date for ${entry[0]}`);
    add("bun", match[1], match[2], entry.at(-1));
  }
  const queue = [...groups.values()];
  let completed = 0;
  console.log(
    `Checking ${count} locked dependencies in ${queue.length} registry packages…`,
  );
  await Promise.all(
    Array.from({ length: 6 }, async () => {
      while (queue.length) {
        const { ecosystem, name, versions } = queue.shift();
        try {
          if (ecosystem === "cargo") {
            const lines = await fetchText(
              `https://rsproxy.cn/index/${registryPath(name)}`,
            );
            const releases = new Map(
              lines
                .trim()
                .split("\n")
                .map((line) => {
                  const entry = JSON.parse(line);
                  return [entry.vers, entry];
                }),
            );
            let dates;
            for (const [version, checksum] of versions) {
              const label = `${name}@${version}`;
              const entry = releases.get(version);
              if (!entry || entry.yanked)
                throw new Error(`${label}: missing or yanked`);
              if (entry.cksum !== checksum)
                throw new Error(`${label}: checksum mismatch`);
              let publishedAt = entry.pubtime;
              if (!publishedAt) {
                dates ??= JSON.parse(
                  await fetchText(`https://rsproxy.cn/api/v1/crates/${name}`),
                );
                publishedAt = dates.versions.find(
                  (release) => release.num === version,
                )?.created_at;
              }
              checkPublishedAt(label, publishedAt, cutoff);
            }
          } else {
            const releases = JSON.parse(
              await fetchText(
                `https://mirrors.cloud.tencent.com/npm/${name.replace("/", "%2f")}`,
              ),
            );
            for (const [version, integrity] of versions) {
              const label = `${name}@${version}`;
              const entry = releases.versions[version];
              if (!entry || entry.dist.integrity !== integrity) {
                throw new Error(`${label}: missing or integrity mismatch`);
              }
              checkPublishedAt(label, releases.time?.[version], cutoff);
            }
          }
        } catch (error) {
          failures.push(`${ecosystem}:${name}: ${error.message}`);
          console.error(failures.at(-1));
        } finally {
          completed++;
          if (completed % 100 === 0)
            console.log(
              `Checked ${completed}/${groups.size} registry packages.`,
            );
        }
      }
    }),
  );
  if (failures.length) throw new Error(failures.sort().join("\n"));
  console.log(
    `Verified ${count} locked dependencies: published on or before ${new Date(cutoff).toISOString()}, checksums match registries.`,
  );
}

if (resolve(process.argv[1] ?? "") === import.meta.filename) {
  await checkLocks(process.argv[2] ? Date.parse(process.argv[2]) : Date.now());
}
