import { expect, test } from "bun:test";
import {
  checkPublishedAt,
  minimumAgeMs,
  registryPath,
} from "../scripts/check-dependency-age.mjs";

test("the seven-day cutoff includes the boundary and rejects recent or missing dates", () => {
  const now = Date.parse("2026-11-10T00:00:00Z");
  const cutoff = now - minimumAgeMs;
  expect(() =>
    checkPublishedAt("fixture@1", "2026-11-03T00:00:00Z", cutoff),
  ).not.toThrow();
  expect(() =>
    checkPublishedAt("fixture@2", "2026-11-03T00:00:01Z", cutoff),
  ).toThrow("less than seven days");
  expect(() => checkPublishedAt("fixture@3", undefined, cutoff)).toThrow(
    "valid publication date",
  );
});

test("crate registry paths cover all supported name lengths", () => {
  expect(["a", "ab", "abc", "tokio"].map(registryPath)).toEqual([
    "1/a",
    "2/ab",
    "3/a/abc",
    "to/ki/tokio",
  ]);
});
