# Locals Studio

An Apple Silicon macOS app and CLI for configuring, downloading, and serving MLX models with [Locals](https://github.com/Generated-Computers/Locals).

Locals Studio is an independent frontend developed by Armateur Labs for Locals, the inference backend by [Generated Computers](https://generatedcomputers.com/). It is not affiliated with, endorsed by, or maintained by Generated Computers or the Locals project.

Tauri 2 + React/Vite power the interface; Rust powers the persistent service, CLI, and HTTP router. Each active profile runs its own Locals Python process. Closing the window keeps models and downloads running.

## Getting started

Development requirements: macOS 14+, Apple Silicon, Xcode build tools, Rust, Node/npm, and [uv](https://docs.astral.sh/uv/). Installing the runtime downloads Python 3.12 and the locked MLX dependencies.

```sh
npm ci
npm run desktop
```

The script builds the service and prepares it as a Tauri sidecar. You can work on the interface with the frontend alone (`npm run dev`); it shows the connection as unavailable in a regular browser because the management bridge runs through Tauri.

To build the app and DMG:

```sh
npm run desktop:build
```

Build artifacts are generated in `target/release/bundle/`. Distribution signing and notarization use the standard Tauri settings; local builds can use ad hoc signing.

Local builds use ad hoc signing by default. After placing the app in Applications, you can add the bundled CLI to your PATH:

```sh
mkdir -p "$HOME/.local/bin"
ln -s "/Applications/Locals Studio.app/Contents/MacOS/locals-studio" "$HOME/.local/bin/locals-studio"
```

The `~/.local/bin` directory must be in your terminal's PATH.

## Tag builds on GitHub Actions

The **Build DMG** workflow runs when a Git tag is pushed. It builds the app and bundled service on the `macos-15` Apple Silicon runner with `npm ci` and `npm run desktop:build -- --ci -- --locked`, then verifies the app signature and DMG integrity.

Download the DMG from **Actions → Build DMG → the tag's run → Artifacts**. The artifact is named `locals-studio-macos-arm64-<commit SHA>` and is retained for 30 days. Builds use the project's existing ad hoc signing configuration.

The application version comes from the project manifests; update those versions before tagging a release. The tag name itself does not change the application version.

## Features

- **Chat**: served models, streaming and cancellation, folders, conversations, and generation settings.
- **Model server**: saved model selection, global or individual start/stop controls, memory and CPU usage, MLX allocations, and per-worker logs.
- **My models**: HF/LM Studio discovery, importing folders without copying them, server profiles, and advanced configuration.
- **Discover**: MLX-filtered HF search, published model details and variants, and persistent downloads with pause/resume support.

Chat prefers a model that is already ready when starting a new conversation. The model selector sits below the message field. Manual selections and the model assigned to an existing conversation are preserved. The list shows model status; when sending is unavailable, the composer explains why and offers to open **Model server** to start the relevant server.

Drag a conversation onto a folder to move it; a collapsed folder opens after a successful drop. Use the **No folder** drop area to move it out of a folder, or use the conversation's folder selector with the keyboard. Moving preserves its messages, an active response, and the composer draft. Folder headers reserve space for the **+**, rename, and remove buttons; rename and remove appear on hover or keyboard focus without shifting **+**. Each chat row also has a delete button, highlighted on hover or keyboard focus. Both sidebar and composer deletion ask for confirmation with the chat title. Deleting another chat preserves the active chat and its draft; a running reply must be stopped first.

**Discover** offers grid and list views, with the view, search, and sorting preferences saved locally. Sort the Hub's returned result set, up to 50 models, by downloads, Safetensors weight size, quantization bits, or disk size for the complete Studio download bundle. Sorting is numeric in either ascending or descending order. Complete sizes come first, partial sizes follow, and unknown values stay last.

Weight and bundle sizes use decimal GB (**1 GB = 1,000,000,000 bytes**). The bundle includes the files selected by Studio: weights, tokenizer, and configuration. A size prefixed with **≥** is partial; unknown sizes remain unavailable, with no estimate based on parameter count. Visible results load metadata for their immutable HF commit; sorting by either size also enriches results outside the visible area, with at most three concurrent metadata requests. Changing the revision in the detail view may change the download size.

Up to eight recent searches are cached locally across app restarts, within a roughly 1 MiB limit. Results stay fresh for five minutes; older cached rows remain visible during a background refresh and if the network request fails. Recognized model-family or publisher avatars come from Hugging Face's official avatar endpoint, are shared across the publisher's models, and are cached on disk. A generic chip icon appears when an avatar is unavailable.

Profiles retain Locals defaults for any options left unset. Profile changes take effect the next time the worker starts. The worker fields `host`, `port`, and `model` are reserved for Studio.

In **Model server**, **Selected only** filters the list to checked models while preserving alphabetical order. The filter is saved locally. Checked profiles have a stronger accent background and stripe; the profile inspected in the log panel remains independent. Filtering leaves the saved serving selection and global start/stop scope unchanged. If no models are selected, **Show all models** restores the full list.

In **Model server**, **Start server** opens the shared API and starts the saved selection; with no selection, it only opens the API. **Stop server** stops all models and closes the HTTP port. The management service remains available to the GUI, CLI, and downloads. The API's stopped state is preserved when Studio is reopened.

The **Selected model memory** card refers to the model selected in the list or log panel. This selection and the log selection are preserved when switching views. The card distinguishes active MLX allocations, reusable cache, current RSS, and peaks since the worker started. **0.8B** in a model name means approximately 800 million parameters; its memory usage depends on factors including quantization and caches. Weights on disk, RSS, and MLX allocations are separate measurements.

## Appearance and branding

**Settings → Appearance** offers **Dark** and **Light** modes, five palettes (**Green**, **Sapphire**, **Violet**, **Amber**, **Rose**), and **Standard** and **Large** text sizes. Preferences are saved locally and restored when the app reopens. Default font sizes have been increased for readability.

The **General** and **About** tabs stay in the same position when switching sections. Each panel's body scrolls independently, keeping the tabs and footer controls accessible in smaller windows.

The in-app logo, macOS Dock icon, and menu bar symbol share the shapes defined in `src/brand.json`. After changing this source, run `npm run icons:generate` to regenerate the macOS icons and the transparent monochrome version used as the menu bar template icon.

## Menu bar and About

In **Settings**, **Show menu bar icon** enables an optional icon in the macOS menu bar. It is disabled by default. The menu lets you start the saved selection, stop the API and workers, and select available profiles. Profiles remain sorted by name; their selection is shared with the GUI and CLI. A memory warning asks for confirmation before a start that requires explicitly allowing memory overcommit.

When the icon is enabled, closing the window hides Studio and keeps the menu available. **Show Locals Studio** reopens the window; **Quit Locals Studio** exits the interface and removes its icon. The service, running models, and downloads continue until explicitly stopped.

You can also change this preference with `locals-studio config set desktop.menu_bar_enabled true` (or `false`). The icon appears while the desktop app is open; this option does not launch Studio at login.

After upgrading from a version without this option, the management service must also be restarted with the new binary. Studio detects an outdated service before saving the enabled setting; it does not restart the service or models automatically.

The **About Locals Studio** section, available in the app and menu, shows the version, the full MIT license available offline, and a link to [Armateur Labs](https://armateurlabs.fr). Studio's license is stored in [LICENSE](LICENSE); models retain their own licenses.

## CLI

```sh
cargo build --locked -p studio-cli
./target/debug/locals-studio runtime install
./target/debug/locals-studio models scan
./target/debug/locals-studio config profiles
./target/debug/locals-studio server start PROFILE_NAME
./target/debug/locals-studio server status --json
./target/debug/locals-studio server logs PROFILE_NAME --follow
./target/debug/locals-studio server stop --all
```

The service starts automatically on the first call. `locals-studio daemon` runs it in the foreground. `locals-studio events` follows JSON Lines events. Read commands accept `--json`; errors are written to stderr with a nonzero exit code.

`locals-studio server start --all` starts the API and selected profiles; `locals-studio server stop --all` stops all workers and closes the API while leaving the management service running. Starting an individual profile reactivates the API if needed.
`locals-studio status --json` exposes the shared API's status alongside other service data; `server status` lists workers.

```sh
locals-studio search "Qwen MLX 4bit"
locals-studio download add ORGANIZATION/REPOSITORY --revision main
locals-studio download list
locals-studio download pause ID
locals-studio download resume ID
locals-studio models import /path/to/a/model
locals-studio config set selected true --profile PROFILE_NAME
locals-studio config set options.max_context 8192 --profile PROFILE_NAME
locals-studio config set options.cache_gb 1 --profile PROFILE_NAME
locals-studio server start --all
```

If the memory estimate requires confirmation, adjust the profiles or explicitly pass `--allow-memory-overcommit`. This decision applies to the requested start and does not enable automatic unloading.

## Shared API and LAN

Default address: `http://127.0.0.1:8321/v1`. Studio aggregates ready models and routes each request using the `model` field, which contains the profile alias.

On macOS, Studio checks for existing TCP listeners on its public port across IPv4, IPv6, loopback, and LAN addresses before starting the API. A conflict reports the listening process, PID, and addresses before any new model is loaded. Keep a standalone Locals server on a separate port, for example `locals start --port 8322`, while Studio uses 8321. The check is a startup preflight; separate ports also avoid ambiguity if another program starts later.

If saving network settings cannot start the API, Settings displays the error and keeps the form open. After freeing the port, save again to retry the same configuration. A terminated API listener is reported offline, and a new chat is rejected until the Studio API is available again. `GET /health` identifies the Studio API with `"service":"locals-studio"`.

To test a model that is already running:

```sh
curl --fail-with-body -sS http://127.0.0.1:8321/health
curl --fail-with-body -sS http://127.0.0.1:8321/v1/models

# Replace MODEL_NAME with an id returned by /v1/models.
curl --fail-with-body -N http://127.0.0.1:8321/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"MODEL_NAME","messages":[{"role":"user","content":"Reply only with OK."}],"max_tokens":16,"temperature":0,"stream":true}'
```

`-N` displays SSE events as they arrive. To receive a complete JSON response, set `stream` to `false`. `Ctrl-C` closes the stream and cancels the current request. An empty model list means no worker is ready; start one from Model Server or with `locals-studio server start ALIAS`.

After **Stop server**, the HTTP port is closed: `curl` reports a connection refusal. Use **Start server** or `locals-studio server start --all` to reopen it.

```sh
locals-studio config set api.host 0.0.0.0
locals-studio config set api.port 8321
```

From another machine: `http://MAC_IP:8321/v1`. Workers remain bound to `127.0.0.1` on their internal ports. Management, history, the runtime, and downloads remain accessible only through the local Unix socket.

OpenAI, Anthropic, and Ollama inference routes are proxied. Streaming remains incremental; closing the client connection cancels the upstream connection. Download management uses the Studio service rather than Locals' stub Ollama download endpoint.

The `api.allowed_hosts` and `api.allowed_origins` lists are optional: an empty list allows all hosts or browser origins, respectively. A populated list applies the corresponding filter while preserving automatic local access. Workers receive the allowed origins, or `LOCALS_ORIGINS=*` when Studio's list is empty. After changing origins, restart the affected workers. Host/Origin checks do not authenticate LAN clients.

```sh
locals-studio config set api.allowed_hosts '["my-mac.local"]'
locals-studio config set api.allowed_origins '["http://192.168.1.20:3000"]'
```

## Storage, runtime, and memory

- Management data: the user's `Locals Studio` folder in Application Support on macOS; a SQLite database and local logs.
- New models: `HF_HOME/locals-studio`, defaulting to `~/.cache/huggingface/locals-studio`.
- HF and LM Studio libraries are referenced without copying. Removing a registration preserves the weights.
- Downloads pin an HF commit, verify available hashes, checkpoint 64 MiB segments, and publish the complete folder with an atomic rename. An abrupt shutdown may require resuming the current segment. Canceling removes the job's staging data; pausing preserves it.
- The HTTP transport checks returned ranges. If the server or a proxy returns the entire file, resuming preserves segments on disk but rereads and compares the prefix over the network before continuing. This can transfer previously received bytes again. Certificates are validated against the system trust store.
- Existing HF credentials (`HF_TOKEN`, the HF token file) are used for private/gated repositories; licenses must be accepted on HF.
- The runtime uses the neighboring Locals checkout during development or the official archive pinned in `resources/runtime/source.lock.json` for distribution. Dependencies are locked with hashes; updates are explicit.
- Current RAM and CPU usage come from the system; active, cached, and peak GPU allocations come from MLX. The RSS reported by Locals is a peak. RSS and MLX measurements can overlap in unified memory and are not added together. GPU usage percentages are not fabricated when no reliable counter is available.
- The pre-load estimate is partial for unknown architectures or Auto settings. It compares weights and estimated memory envelopes with the Metal budget, allowing a system reserve, without guaranteeing a total process limit.

Development variables: `LOCALS_STUDIO_DATA_DIR`, `LOCALS_STUDIO_MODELS_DIR`, `LOCALS_STUDIO_LOCALS_SOURCE`, `LOCALS_STUDIO_CLI_BIN`. These also allow tests to be completely isolated from personal data.

## Validation

```sh
cargo test --locked -p studio-core -- --test-threads=1
cargo test --locked -p studio-desktop
cargo fmt --all -- --check
npm test
npm run build
```

Core tests use real small HTTP servers and mock Python processes, without downloading or loading LLMs. They cover download ranges, checkpoints, integrity, discovery, process ownership, start/stop behavior, routing, streaming, and network policies. Actual MLX validation and testing from a second machine on the LAN are separate; see `docs/validation.md` for the results actually obtained.

MIT license; see [LICENSE](LICENSE). Locals is a separate project licensed under Apache-2.0; Studio does not modify its code.
