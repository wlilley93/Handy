/**
 * The guards: which source each one removes, and the test that must fail
 * without it.
 *
 * A module rather than a list inside `red.ts`, so `guard-coverage.ts` and
 * anything else that needs this data imports it instead of parsing the
 * source. The sibling repo's hand-rolled parser for exactly that was wrong
 * four times — escaped backticks, `\$`, `\"`, `\\`, then double-processing
 * once the fixes were chained — and this file carries Rust source, which is
 * more likely to contain escapes, not less.
 */

type Guard = {
  name: string;
  file: string;
  from: string;
  to: string;
  /** The test that must fail — not merely "something failed". */
  expect: string;
};


export const GUARDS: Guard[] = [
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
  {
    name: "the overlay is hidden on every path, not just success",
    file: "src-tauri/src/server/routes.rs",
    from: `    if state.show_overlay {\n        state.host.hide_recording_overlay();\n    }\n\n    match result {\n        Ok(Ok(text)) => Ok(text),`,
    to: `    match result {\n        Ok(Ok(text)) => {\n            if state.show_overlay {\n                state.host.hide_recording_overlay();\n            }\n            Ok(text)\n        }`,
    expect: "the_overlay_is_hidden_even_when_the_engine_fails",
  },
  {
    name: "an engine failure is a 500, not an empty 200",
    file: "src-tauri/src/server/routes.rs",
    from: `        Ok(Err(e)) => Err(ApiError::new(`,
    to: `        Ok(Err(_e)) => Ok(String::new()),\n        #[allow(unreachable_patterns)]\n        Ok(Err(e)) => Err(ApiError::new(`,
    expect: "an_engine_failure_is_a_500",
  },
  {
    name: "an unknown response_format is refused, not silently json",
    file: "src-tauri/src/server/routes.rs",
    // Single-quoted: the Rust source contains `${other}` and backticks, both
    // of which a template literal would try to interpret.
    from: '        other => {\n            return Err(ApiError::bad_request(format!(\n                "unsupported response_format `{other}` (use json, text or verbose_json)"\n            )))\n        }',
    to: '        _other => Json(json!({ "text": text })).into_response(),',
    expect: "an_unknown_response_format_is_refused",
  },
  {
    name: "a multipart read error is reported, not swallowed",
    file: "src-tauri/src/server/routes.rs",
    from: '            Err(e) => {\n                return Err(ApiError::bad_request(format!(\n                    "could not read the multipart body: {e}"\n                )))\n            }',
    to: '            Err(_e) => break,',
    expect: "a_malformed_multipart_body_is_refused",
  },
];
