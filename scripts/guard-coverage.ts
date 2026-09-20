/**
 * List every refusal in `src-tauri/src/server/` and say whether a test reaches it.
 *
 * `scripts/red.ts` proves the guards it lists are tested. It cannot say
 * anything about the guards nobody listed — and that is how the 24-bit scaling
 * constant went untested: not because a check failed, but because no check was
 * ever written for it. A hand-written list of what to verify has the same flaw
 * as a hand-written list of what to expose.
 *
 * The heuristic: a refusal carries a distinctive message, and a test that
 * exercises it almost always asserts on part of that message or on the
 * condition by name. A refusal whose text appears nowhere in a test module is
 * not proof of absence, but it is the right place to look. Treat the output as
 * a worklist, not a score.
 *
 * Run: bun scripts/guard-coverage.ts
 */

import { readdirSync } from "node:fs";
import { join } from "node:path";

const DIR = "src-tauri/src/server";

type Site = { file: string; line: number; kind: string; text: string };

function refusals(file: string, source: string): Site[] {
  const out: Site[] = [];
  const lines = source.split("\n");
  lines.forEach((line, index) => {
    const next = lines[index + 1] ?? "";
    // `bail!(` often wraps, putting the message on the next line — matching
    // only the same line missed a third of them.
    const bail = line.match(/bail!\(\s*"([^"]{6,})"/)
      ?? (/bail!\($/.test(line.trim()) ? next.match(/^\s*"([^"]{6,})"/) : null);
    if (bail) out.push({ file, line: index + 1, kind: "bail!", text: bail[1]! });
    const status = line.match(/StatusCode::([A-Z_]+)/);
    if (status && !line.trim().startsWith("//")) {
      out.push({ file, line: index + 1, kind: "status", text: status[1]! });
    }
    const guard = line.match(/^\s*(?:return (?:false|None);|.*\breturn Err\()/);
    if (guard && !line.includes("//")) {
      out.push({ file, line: index + 1, kind: "return", text: line.trim().slice(0, 60) });
    }
  });
  return out;
}

/** Everything inside `#[cfg(test)] mod tests { ... }`, across the module. */
function testBodies(sources: Map<string, string>): string {
  let all = "";
  for (const source of sources.values()) {
    const start = source.indexOf("#[cfg(test)]");
    if (start !== -1) all += source.slice(start);
  }
  return all;
}

const sources = new Map<string, string>();
for (const name of readdirSync(DIR)) {
  if (name.endsWith(".rs")) sources.set(name, await Bun.file(join(DIR, name)).text());
}

/** Both sides lowercased and stripped of punctuation: the message says
 *  "without a token:" and the assertion says "without a token". */
function normalise(text: string): string {
  return text.toLowerCase().replace(/[^a-z0-9]+/g, " ").trim();
}

/** Format specifiers are not words. Only ever applied to a message — applying
 *  it to Rust source swallows whole blocks, because `{` and `}` are code. */
function withoutSpecifiers(message: string): string {
  return normalise(message.replace(/\{[^}]*\}/g, " "));
}

const tests = testBodies(sources);
const normalisedTests = normalise(tests);
const sites: Site[] = [];
for (const [name, source] of sources) sites.push(...refusals(name, source));

// A distinctive fragment of the message: the first few words, which is what a
// test assertion tends to quote.
function reached(site: Site): boolean {
  if (site.kind === "status") return tests.includes(site.text);
  // Any three-word window, not the opening words: a test asserts on the
  // distinctive fragment ("without a token", "longer than the"), which is
  // rarely the start of the sentence. Taking only the first words scored
  // every guard in this module as unreached, including five that red.ts
  // proves are tested — a result too extreme to be true, which is what gave
  // the heuristic away.
  const words = withoutSpecifiers(site.text).split(" ").filter(Boolean);
  for (const size of [3, 2]) {
    for (let i = 0; i + size <= words.length; i++) {
      const window = words.slice(i, i + size).join(" ");
      if (window.length > 8 && normalisedTests.includes(window)) return true;
    }
  }
  return false;
}

const unreached = sites.filter(site => !reached(site));
for (const site of unreached) {
  console.log(`  ?     ${site.file}:${site.line}  ${site.kind}  ${site.text}`);
}
console.log(
  `\n${sites.length - unreached.length}/${sites.length} refusal sites are mentioned by a test`,
);
