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
      // The message on the following line, when there is one. Two sites can
      // share a status — `PAYLOAD_TOO_LARGE` is raised both by the declared-
      // length check and by the read backstop — and without the message they
      // cannot be told apart, so a waiver aimed at one would silence both.
      const message = next.match(/"([^"]{6,})"/)?.[1];
      out.push({
        file,
        line: index + 1,
        kind: "status",
        text: message ? `${status[1]}: ${message}` : status[1]!,
      });
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
import { GUARDS } from "./red-guards";

function blockSpan(source: string, at: number, from: string): number {
  let depth = 0;
  let opened = false;
  for (let i = at; i < source.length; i++) {
    const c = source[i];
    if (c === "{") {
      depth++;
      opened = true;
    } else if (c === "}") {
      depth--;
      if (opened && depth <= 0) return i;
    }
  }
  return at + from.length;
}

const provenLines = new Map<string, Set<number>>();
const unlocated: string[] = [];
for (const entry of GUARDS) {
  const source = await Bun.file(entry.file).text().catch(() => "");
  const at = source.indexOf(entry.from);
  if (at === -1) {
    // Not "this guard covers nothing" — the tool cannot see. Skipping these
    // silently reported 0/22 at exit 0 when the lookup was broken, which is
    // the clean sheet this floor exists to refuse.
    unlocated.push(`${entry.file}: ${entry.from.split("\n")[0]!.trim()}`);
    continue;
  }
  const startLine = source.slice(0, at).split("\n").length;
  const endLine = source.slice(0, blockSpan(source, at, entry.from)).split("\n").length;
  const base = entry.file.split("/").pop()!;
  if (!provenLines.has(base)) provenLines.set(base, new Set());
  const lines = provenLines.get(base)!;
  for (let line = startLine; line <= endLine; line++) lines.add(line);
}

function proven(site: Site): boolean {
  return provenLines.get(site.file)?.has(site.line) ?? false;
}

/**
 * Sites that are not worth a guard, each with the reason.
 *
 * Without this the count mixes real work with noise, and a figure that reads
 * worse than the situation gets ignored. The sibling repo waives its request
 * lifecycle the same way; these are the two kinds here.
 *
 * `constructors`: the status codes inside `ApiError`'s own `bad_request` and
 * `unauthorized`. They are where an error is *built*, not where one is
 * *decided* — every call site that decides is counted separately, and
 * guarding these would test a constructor.
 *
 * `unreachable`: refusals with no reachable input, kept as defence. The
 * zero-channel bail never fires because hound rejects such a file while
 * parsing; it stays because `downmix` divides by the channel count. Proven
 * by red.ts: removing it made nothing fail.
 */
const UNREACHABLE: Record<string, string> = {
  "PAYLOAD_TOO_LARGE: could not read `file`: {e}":
    "the read backstop: DefaultBodyLimit's rejection usually surfaces at next_field() instead, " +
    "so reaching this one depends on where chunk boundaries fall (see 093bdf2)",
  "WAV declares zero channels":
    "hound refuses a zero-channel file while parsing; kept as defence for downmix",
};

/** The line range of `impl ApiError { ... }`, if the file has one. */
function constructorRange(source: string): [number, number] | null {
  const at = source.indexOf("impl ApiError {");
  if (at === -1) return null;
  const start = source.slice(0, at).split("\n").length;
  const end = source.slice(0, blockSpan(source, at, "impl ApiError {")).split("\n").length;
  return [start, end];
}

const allSites: Site[] = [];
const waived: Array<{ site: Site; reason: string }> = [];
for (const [name, source] of sources) {
  const production = productionOnly(name, source);
  if (production === null) continue;
  const ctor = constructorRange(production);
  for (const site of refusals(name, production)) {
    const unreachable = UNREACHABLE[site.text];
    if (unreachable) {
      waived.push({ site, reason: unreachable });
    } else if (ctor && site.line >= ctor[0] && site.line <= ctor[1]) {
      waived.push({ site, reason: "inside ApiError's own constructors" });
    } else {
      allSites.push(site);
    }
  }
}
const sites = allSites;

// A waiver keyed on message text stops matching the moment the message is
// reworded, and the site then vanishes from the denominator instead of
// reappearing as work. An unused waiver is a stale one.
const unusedWaivers = Object.keys(UNREACHABLE).filter(
  text => !waived.some(w => w.site.text === text),
);
if (unusedWaivers.length) {
  console.error("waivers that no longer match any site — reworded, or removed:");
  for (const key of unusedWaivers) console.error(`  ${key}`);
  process.exit(2);
}

const unproven = sites.filter(site => !proven(site));

// The floor. These are `from:` sources in red.ts, not messages — the earlier
// version of this check used message fragments and fired immediately, which
// is the check working: it refused to score against a parse it could not
// justify.
const MUST_PARSE = ["token.is_none()", "8_388_607", "header.or(query)"];
const blind = MUST_PARSE.filter(
  fragment => !GUARDS.some(entry => entry.from.includes(fragment)),
);
if (blind.length || GUARDS.length < 10 || unlocated.length) {
  console.error(
    `guard-coverage cannot use the guard list: parsed ${GUARDS.length} entries` +
    (blind.length ? `, missing ${blind.join(", ")}` : ""),
  );
  for (const entry of unlocated) console.error(`  could not locate  ${entry}`);
  process.exit(2);
}

for (const site of unproven) {
  console.log(`  ?     ${site.file}:${site.line}  ${site.kind}  ${site.text}`);
}
console.log(
  `\n${sites.length - unproven.length}/${sites.length} refusal sites have a red.ts guard` +
  `\n${unproven.length} are a worklist, not a failure — a site may well be tested without one.` +
  `\n${waived.length} waived: ` +
  [...new Set(waived.map(w => w.reason))].join("; ") + ".",
);
