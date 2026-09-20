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

type Guard = {
  name: string;
  file: string;
  from: string;
  to: string;
  /** The test that must fail — not merely "something failed". */
  expect: string;
};

const GUARDS: Guard[] = [
  {
    name: "binding every interface without a token is refused",
    file: "src-tauri/src/server/mod.rs",
    from: `            if token.is_none() {`,
    to: `            if false {`,
    expect: "lan_without_a_token_is_refused",
  },
  {
    name: "a blank token is not a token",
    file: "src-tauri/src/server/mod.rs",
    from: `.filter(|t| !t.is_empty())`,
    to: `.filter(|_| true)`,
    expect: "a_blank_token_does_not_count_as_a_token",
  },
  {
    name: "a wrong header does not fall through to the query",
    file: "src-tauri/src/server/routes.rs",
    from: `    match header.or(query) {`,
    to: `    match header.filter(|t| secret_eq(t, expected)).or(query) {`,
    expect: "a_wrong_header_does_not_fall_through_to_the_query",
  },
  {
    name: "the token compare rejects a prefix",
    file: "src-tauri/src/server/routes.rs",
    from: `    if a.len() != b.len() {\n        return false;\n    }`,
    to: `    let n = a.len().min(b.len());\n    let (a, b) = (&a[..n], &b[..n]);`,
    expect: "secret_eq_matches_only_the_exact_token",
  },
  {
    name: "24-bit scales by the sample width, not the container",
    file: "src-tauri/src/server/audio.rs",
    from: `            let scale = 8_388_607.0_f32; // 2^23 - 1`,
    to: `            let scale = i32::MAX as f32;`,
    expect: "scales_24_bit_by_the_sample_width_not_the_container",
  },
  {
    name: "the router enforces auth on every route but healthz",
    file: "src-tauri/src/server/routes.rs",
    from: `    match header.or(query) {\n        Some(t) => secret_eq(t, expected),\n        None => false,\n    }`,
    to: `    let _ = (header, query, expected);\n    true`,
    expect: "a_missing_token_is_refused_on_the_model_list",
  },
  {
    name: "an empty file part is refused before decoding",
    file: "src-tauri/src/server/routes.rs",
    // Single-quoted: the Rust source contains backticks, which a template
    // literal would end.
    from: '    if bytes.is_empty() {\n        return Err(ApiError::bad_request("`file` is empty"));\n    }\n',
    to: "",
    expect: "an_empty_file_is_a_bad_request",
  },
  {
    name: "a non-WAV upload is 415, not 400",
    file: "src-tauri/src/server/routes.rs",
    from: `            StatusCode::UNSUPPORTED_MEDIA_TYPE,`,
    to: `            StatusCode::BAD_REQUEST,`,
    expect: "a_non_wav_upload_is_unsupported_media_type",
  },
  {
    name: "audio that decodes to nothing is refused",
    file: "src-tauri/src/server/routes.rs",
    from: `    if samples.is_empty() {\n        return Err(ApiError::bad_request("decoded audio contains no samples"));\n    }\n`,
    to: "",
    expect: "a_wav_with_no_audio_is_a_bad_request",
  },
  {
    name: "audio longer than the limit is refused",
    file: "src-tauri/src/server/audio.rs",
    from: "    if secs > MAX_AUDIO_SECS {",
    to: "    if false {",
    expect: "rejects_audio_over_the_length_limit",
  },
  {
    name: "a PCM16 payload with an odd byte count is refused",
    file: "src-tauri/src/server/audio.rs",
    from: "    if !bytes.len().is_multiple_of(2) {",
    to: "    if false {",
    expect: "rejects_odd_length_pcm16",
  },
  {
    name: "a model that cannot stream is refused with 409",
    file: "src-tauri/src/server/routes.rs",
    from: "    if !supports_streaming {",
    to: "    if false {",
    expect: "a_model_that_cannot_stream_is_refused_with_conflict",
  },
  {
    name: "a zero sample rate is refused",
    file: "src-tauri/src/server/routes.rs",
    from: "    if rate == 0 {",
    to: "    if false {",
    expect: "a_zero_sample_rate_is_refused",
  },
  {
    name: "an unknown dialect is refused",
    file: "src-tauri/src/server/routes.rs",
    from: `        Some("codex") => Dialect::Codex,`,
    to: `        Some("codex") | Some(_) => Dialect::Codex,`,
    expect: "an_unknown_dialect_is_refused",
  },
  {
    name: "port zero is refused",
    file: "src-tauri/src/server/mod.rs",
    from: "        if port == 0 {",
    to: "        if false {",
    expect: "port_zero_is_refused",
  },
];

async function runSuite(): Promise<{ ok: boolean; output: string }> {
  const done = await $`cargo test --lib server::`.cwd("src-tauri").nothrow().quiet();
  return { ok: done.exitCode === 0, output: done.stdout.toString() + done.stderr.toString() };
}

const misses: string[] = [];

for (const guard of GUARDS) {
  const original = await Bun.file(guard.file).text();
  const occurrences = original.split(guard.from).length - 1;
  if (occurrences !== 1) {
    misses.push(`${guard.name}: its source appears ${occurrences} times in ${guard.file}`);
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
if (!ok) {
  console.log("restore failed — the suite is red with the original source\n" + output);
  process.exit(1);
}
for (const miss of misses) console.log(`  MISS  ${miss}`);
console.log(`\n${GUARDS.length - misses.length}/${GUARDS.length} guards are actually tested`);
process.exit(misses.length ? 1 : 0);
