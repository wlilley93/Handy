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

type Site = { file: string; line: number; kind: string; text: string; matchable?: boolean };

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
      // The message only, never the whole line: the line carries the
      // constructor's name, and `ApiError::bad_request(...)` normalises to
      // "bad request", which any test mentioning StatusCode::BAD_REQUEST
      // matches. That scored every refusal in this family as reached for
      // free, and moved the figure from 6/22 to 13/22 on four tests.
      const message = (line.match(/"([^"]{6,})"/) ?? next.match(/"([^"]{6,})"/))?.[1];
      out.push({
        file,
        line: index + 1,
        kind: "return",
        text: message ?? line.trim().slice(0, 60),
        matchable: message !== undefined,
      });
    }
  });
  return out;
}

/** Everything inside `#[cfg(test)] mod tests { ... }`, across the module. */
function testBodies(sources: Map<string, string>): string {
  let all = "";
  for (const [name, source] of sources) {
    // A whole file may be the test module (`mod http_tests;`), in which case
    // there is no `#[cfg(test)]` inside it to find.
    if (name.endsWith("_tests.rs")) {
      all += source;
      continue;
    }
    const start = source.indexOf("#[cfg(test)]");
    if (start !== -1) all += source.slice(start);
  }
  return all;
}

const sources = new Map<string, string>();
for (const name of readdirSync(DIR)) {
  if (name.endsWith(".rs")) sources.set(name, await Bun.file(join(DIR, name)).text());
}

/** Production code only: everything before `#[cfg(test)]`, and no test-only
 *  module at all. Counting refusals inside tests inflates the denominator with
 *  the very code that is supposed to reduce it — adding six request-level
 *  tests moved the figure from 5/22 to 5/28 without changing the module. */
function productionOnly(name: string, source: string): string | null {
  if (name.endsWith("_tests.rs")) return null;
  const testMod = source.indexOf("#[cfg(test)]");
  return testMod === -1 ? source : source.slice(0, testMod);
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

/**
 * A site is *proven* when `red.ts` has an entry that removes it: that entry has
 * been watched to make a named test fail, which is evidence rather than a
 * proxy. Everything else is a worklist.
 *
 * The text heuristic that used to score this was wrong in both directions. It
 * counted `ApiError::bad_request(...)` as reached because the constructor's
 * own name normalises to "bad request", which any test mentioning
 * StatusCode::BAD_REQUEST matched — 6/22 became 13/22 on four tests. Narrowing
 * it to the message then under-counted, because those same tests assert on
 * status codes and never quote the message. A proxy that has been wrong in
 * both directions should not be printing a score.
 */
const redSource = await Bun.file("scripts/red.ts").text();
const provenFragments = [...redSource.matchAll(/from:\s*(?:`([\s\S]*?)`|'([^']*)'|"([^"]*)")/g)]
  .map(match => (match[1] ?? match[2] ?? match[3] ?? "").replace(/\\n/g, "\n"))
  .filter(Boolean);

function proven(site: Site): boolean {
  return provenFragments.some(fragment =>
    site.text.length > 6 && fragment.includes(site.text)
  );
}

const sites: Site[] = [];
for (const [name, source] of sources) {
  const production = productionOnly(name, source);
  if (production !== null) sites.push(...refusals(name, production));
}

const unproven = sites.filter(site => !proven(site));

// The floor. These are `from:` sources in red.ts, not messages — the earlier
// version of this check used message fragments and fired immediately, which
// is the check working: it refused to score against a parse it could not
// justify.
const MUST_PARSE = ["token.is_none()", "8_388_607", "header.or(query)"];
const blind = MUST_PARSE.filter(
  fragment => !provenFragments.some(source => source.includes(fragment)),
);
if (blind.length || provenFragments.length < 5) {
  console.error(
    `guard-coverage cannot read red.ts: parsed ${provenFragments.length} entries` +
    (blind.length ? `, missing ${blind.join(", ")}` : ""),
  );
  process.exit(2);
}

for (const site of unproven) {
  console.log(`  ?     ${site.file}:${site.line}  ${site.kind}  ${site.text}`);
}
console.log(
  `\n${sites.length - unproven.length}/${sites.length} refusal sites have a red.ts guard` +
  `\n${unproven.length} are a worklist, not a failure — a site may well be tested without one.`,
);
