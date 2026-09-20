# Local API

Handy can serve transcription over HTTP on your own machine, so anything else
you run — an editor, a shell script, a coding agent — can use the model Handy
already has loaded.

It is **off by default**. Turn it on under **Settings → Advanced → Local API**,
or run the app headlessly with `--serve`.

There is only ever one loaded model. The API, the hotkey and the app's model
picker all share it: switching models in the app switches them for the API too,
and the model-unload timeout still applies. That is deliberate — a separate
server process would hold a second copy of the weights and compete for the same
accelerator.

## Endpoints

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/healthz` | Liveness, selected model, whether an engine is resident. Never requires a token. |
| `GET` | `/v1/models` | The model registry, with a `downloaded` flag per model. |
| `POST` | `/v1/audio/transcriptions` | OpenAI-compatible batch transcription. |
| `GET` | `/v1/audio/stream` | WebSocket: live partial transcripts. Streaming-capable models only. |

### `POST /v1/audio/transcriptions`

`multipart/form-data`, the fields OpenAI's API uses:

- `file` — **WAV only**, any sample rate, any channel count, 8/16/24/32-bit int
  or 32-bit float. Downmixed to mono and resampled to 16 kHz for you. Up to ten
  minutes of audio.
- `model` — optional model id (see `/v1/models`). Omit it to use the model
  selected in the app. Naming one loads it, which changes the model the hotkey
  uses too.
- `response_format` — `json` (default), `text`, or `verbose_json`.
- `language`, `prompt`, `temperature` — accepted and ignored, so an OpenAI
  client's default payload does not fail. Handy takes the language from its own
  settings.

```bash
curl http://127.0.0.1:8915/v1/audio/transcriptions \
  -F "file=@speech.wav" \
  -F "response_format=json"
# {"text":"The quick brown fox jumps over the lazy dog."}
```

Compressed audio (mp3, m4a, ogg) is **not** accepted and returns `415`. Handy
ships no decoder for it; convert first:

```bash
ffmpeg -i clip.m4a -ac 1 -ar 16000 clip.wav
```

### `GET /v1/audio/stream` (WebSocket)

For live dictation, where you want text as the person speaks rather than after
they stop. Only works when the selected model advertises streaming — otherwise
the upgrade is refused with `409` and a message saying to use the batch
endpoint.

- Connect to `ws://127.0.0.1:8915/v1/audio/stream?sample_rate=16000`.
- The server sends `{"type":"ready","sample_rate":N}`.
- Send **raw little-endian PCM16 mono** as binary frames, at `sample_rate`.
- Receive `{"type":"partial","committed":"…","tentative":"…"}` as the model
  decodes. `committed` only ever grows; `tentative` is the part it may still
  rewrite.
- Send `{"type":"commit"}` (or just close) to finish. The server replies
  `{"type":"final","text":"…"}` and closes.

## Settings

| Setting | Default | Notes |
| --- | --- | --- |
| Local API server | off | |
| Port | `8915` | |
| Bearer token | none | Sent as `Authorization: Bearer <token>`. |
| Allow network access | off | Binds `0.0.0.0` instead of `127.0.0.1`. |

**Network access requires a token, and the app refuses to bind `0.0.0.0`
without one.** Transcription is a capability, not just a read: whoever can
reach it can spend this machine's GPU and read back the text. On `127.0.0.1` a
token is optional, because the OS already limits callers to processes on this
machine.

The WebSocket also accepts `?token=…`, because a browser cannot set a header on
an upgrade request. The plain HTTP routes do not — URLs end up in logs and
shell history in a way headers do not.

## Headless

`--serve` runs the API with no window, no tray, no microphone and no overlay —
for a login item, a container, or a supervisor that keeps it up:

```bash
handy --serve                 # port from settings (8915)
handy --serve --serve-port 9000
```

It uses the model selected in the app's settings, which must already be
downloaded — the headless path does not fetch models.
