/**
 * Break each server guard, and require the named test to fail.
 *
 * A green suite proves the tests pass. It does not prove they would notice if
 * the code were wrong. The guards here are the ones where being wrong is a
 * security failure rather than an inconvenience — the refusal to bind every
 * interface without a token, the constant-time compare, the rule that a wrong
 * Authorization header must not fall through to `?token=` — plus the 24-bit
 * scaling constant, which fails silently rather than loudly: audio that still
 * decodes and is simply 256x too quiet.
 *
 * Run: bun scripts/red.ts
 */

import { $ } from "bun";

import { GUARDS, type Guard } from "./red-guards";



/**
 * A suite run with a wall-clock limit.
 *
 * A break can make the code loop rather than fail — removing the deadline
 * check in `wait_for_model` does exactly that — and without a timeout the
 * runner waits forever holding the lock, with the guard still removed. That
 * happened once while writing these. A timeout counts as a failure, which is
 * the right answer: the guard was noticed, just not by returning.
 */
const SUITE_TIMEOUT_MS = 120_000;

async function runSuite(): Promise<{ ok: boolean; output: string }> {
  const proc = Bun.spawn(["cargo", "test", "--lib", "server::"], {
    cwd: "src-tauri",
    stdout: "pipe",
    stderr: "pipe",
  });
  const timer = setTimeout(() => proc.kill(), SUITE_TIMEOUT_MS);
  try {
    const [out, err] = await Promise.all([
      new Response(proc.stdout).text(),
      new Response(proc.stderr).text(),
    ]);
    const code = await proc.exited;
    return { ok: code === 0, output: out + err + (code === null ? "\n(timed out)" : "") };
  } finally {
    clearTimeout(timer);
  }
}

/**
 * A lock, held while any guard is removed.
 *
 * This tool disables production code for a few seconds at a time, and a
 * `git add -A` during that window commits the disabled guard. That is not
 * hypothetical: commit 68251d9 shipped `if false` in place of the audio
 * length limit, because a `bun run check` was running in the background
 * while the commit was made. A check must not be able to do the damage it
 * is looking for.
 */
const LOCK = ".red-running";

function releaseLock() {
  try {
    require("node:fs").unlinkSync(LOCK);
  } catch {
    // already gone
  }
}

if (await Bun.file(LOCK).exists()) {
  console.error(`${LOCK} exists — another red run is in flight, or one died mid-break.`);
  console.error("Check `git status` before deleting it: a guard may still be disabled.");
  process.exit(2);
}
await Bun.write(LOCK, `${process.pid}\n`);
// Every exit, not just the ones remembered: an early return added later has
// no reason to know it is holding a lock.
process.on("exit", releaseLock);
for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"] as const) {
  process.on(signal, () => {
    releaseLock();
    process.exit(130);
  });
}

/**
 * The names cargo actually runs.
 *
 * Without this, a guard pointing at a renamed or deleted test reports MISS —
 * the same word as a guard whose break nothing noticed — and the two want
 * opposite fixes: one is a stale entry, the other is a missing test.
 */
async function listedTests(): Promise<Set<string>> {
  const proc = Bun.spawn(["cargo", "test", "--lib", "--", "--list"], {
    cwd: "src-tauri",
    stdout: "pipe",
    stderr: "ignore",
  });
  const out = await new Response(proc.stdout).text();
  await proc.exited;
  return new Set(
    out
      .split("\n")
      .filter(line => line.endsWith(": test"))
      .map(line => line.slice(0, -": test".length).split("::").pop()!),
  );
}

const listed = await listedTests();

const misses: string[] = [];

for (const guard of GUARDS) {
  const original = await Bun.file(guard.file).text();
  const occurrences = original.split(guard.from).length - 1;
  if (occurrences !== 1) {
    misses.push(`${guard.name}: its source appears ${occurrences} times in ${guard.file}`);
    continue;
  }
  if (!listed.has(guard.expect)) {
    misses.push(`${guard.name}: no test named ${guard.expect}`);
    continue;
  }
  await Bun.write(guard.file, original.replace(guard.from, guard.to));
  try {
    const { ok, output } = await runSuite();
    if (ok) misses.push(`${guard.name}: nothing failed — ${guard.expect} did not notice`);
    else if (!output.includes(guard.expect)) {
      misses.push(`${guard.name}: something failed, but not ${guard.expect}`);
    } else console.log(`  red   ${guard.name}`);
  } finally {
    await Bun.write(guard.file, original);
  }
}

const { ok, output } = await runSuite();
releaseLock();
if (!ok) {
  console.log("restore failed — the suite is red with the original source\n" + output);
  process.exit(1);
}
for (const miss of misses) console.log(`  MISS  ${miss}`);
console.log(`\n${GUARDS.length - misses.length}/${GUARDS.length} guards are actually tested`);
process.exit(misses.length ? 1 : 0);
