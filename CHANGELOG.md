# Changelog

## Unreleased

## 0.2.0 — 2026-11-22

- Add a **Usage** view with cumulative local token usage: totals, requests, prompt cache share, a yearly activity heatmap, comparisons with well-known books, an estimated cloud API cost, and breakdowns by model and by app or agent. Counts stay on this Mac; `locals-studio usage` prints them from the terminal.
- Record usage from every inference API client, streamed or not, in the OpenAI, Anthropic and Ollama formats. Requests are attributed to the calling app from `X-Locals-Client` or its User-Agent.
- Add a GitHub repository link in About.

## 0.1.5 — 2026-11-04

- Add persistent shared configuration presets, with explicit Apply/Save actions, custom preset management, and built-in Locals defaults and Qwen non-thinking presets.
- Remove universal Qwen tuning from server and chat defaults. Unset options inherit Locals; explicit chat overrides are scoped to their conversation.
- Load Gemma 4 checkpoints containing auxiliary MTP weights with the pinned mlx-lm runtime by filtering only the unsupported head, while retaining strict decoder loading and standard decoding.

- Offer optional local model file deletion when removing a model from the library, with an explicit directory confirmation and CLI `--delete-files` flag. Keep files by default and protect shared cache snapshots, active models and downloads.
- Collapse Discover's Downloads panel by default and remember its open or closed state across visits and app restarts.
- Add a persistent option to skip chat deletion confirmations, available in the deletion dialog and reversible in General settings.
- Reduce modal background dimming and remove the backdrop blur to make opening dialogs less visually disruptive.
- Add model aliases backed by one or more server profiles, with persistent session affinity and automatic failover to another ready member when the assigned server becomes unavailable.
- Allow renaming model aliases while preserving their case, internal identity, saved chat selections, and session assignments.
- Search member servers by server name, model or repository when creating or editing an alias, without losing selections hidden by the filter.
- Manage alias members in Model server, select aliases in Chat, and identify the actual server on each reply. API clients can provide `X-Locals-Session-Id`; requests without it share a stable default assignment.

- Relay each running worker's advertised context limits through model list and detail endpoints, including thinking aliases, without extra worker polls or stale limits after restart.

- Keep system prompts scoped to their conversation: new chats start without another chat's instructions, and reopening a chat displays its own saved system prompt. Verify separate histories and streamed replies when chats share a running model.
- Preserve the model's finish reason and show an actionable notice when a response stops at a token limit, including after reopening the conversation.
- Add a visible **Copy response** action below assistant replies, with native clipboard support, success feedback, and retryable errors.
- Keep copy feedback within its control and preserve button dimensions so copying does not shift or scroll the chat layout.
- Match conversation row actions to folders: show rename and a removal cross on hover or keyboard focus.
- Add optional Prometheus metrics on the existing API's `/metrics` route, with a persistent Settings toggle, gateway request timing, worker statistics, and documented scrape configuration.
- Label per-profile Prometheus measurements with the library model name, model ID, and server alias for readable filtering and aggregation.
- Detect an older running metrics service and apply the deferred update before reporting model-label support.

## 0.1.4 — 2026-10-27

- Add a chat shortcut before the removal cross in each server row. Ready or running profiles open a new chat with the clicked model selected and the prompt focused.

## 0.1.3 — 2026-10-27

- Stabilize the global-stop integration check against the existing worker shutdown grace period, with explicit timeout diagnostics and exact worker-launch tracking.
- Add named **Logs**, direct server configuration, and profile removal controls to server rows. Confirm stopping a running profile before removing it, while keeping the model in the library.
- Add **Reload** and **Stop** to model details, and persistent **Favorites only** and **Running only** filters in **Model server**.

## 0.1.2 — 2026-10-27

- Add model favorites and a persistent **Favorites only** filter in **My models**.
- Add **Start model** and **Use in new chat** to model details, with profile selection and the existing memory review before starting a model.
- Show the exact release tag or short commit in the app, with build metadata embedded by GitHub Actions. Keep the package version separately in About.

## 0.1.1 — 2026-10-27

- Add **Test API** next to the endpoint, with formatted JSON responses, HTTP status and timing, copy, retry, and readable connection errors.
- Add saved chronological/reverse log ordering, open server logs at the latest output, and follow new lines with pause/resume while browsing earlier output.
- Publish the standard and `-thinking` model IDs by default when Locals advertises both. Both IDs use the same loaded model. Preserve exact aliases and model names that already end in `-thinking`.
- Add per-conversation **Thinking: Default / On / Off** controls that apply to the next reply without restarting the model. Clarify the server's startup default and the shared token budget for thinking and the answer.
- Show **Preparing**, **Thinking**, and **Generating** according to the response stream, rather than showing Thinking while waiting for any output.
- Update older management services to support thinking variants while retaining their running model workers.
- Attach the DMG and PKG to GitHub tag releases in addition to uploading them as Actions artifacts.

## 0.1.0 — 2026-10-26

Initial macOS release with local chat, MLX model discovery and downloads, server management, a CLI, and DMG/PKG installers.
