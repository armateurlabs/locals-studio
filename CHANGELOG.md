# Changelog

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
