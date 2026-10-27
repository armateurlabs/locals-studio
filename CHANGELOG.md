# Changelog

## 0.1.1 — Unreleased

- Add **Reload** and **Stop** to model details, and persistent **Favorites only** and **Running only** filters in **Model server**.
- Add model favorites and a persistent **Favorites only** filter in **My models**.
- Add **Start model** and **Use in new chat** to model details, with profile selection and the existing memory review before starting a model.
- Show the exact release tag or short commit in the app, with build metadata embedded by GitHub Actions. Keep the package version separately in About.
- Add **Test API** next to the endpoint, with formatted JSON responses, HTTP status and timing, copy, retry, and readable connection errors.
- Add saved chronological/reverse log ordering, open server logs at the latest output, and follow new lines with pause/resume while browsing earlier output.
- Publish the standard and `-thinking` model IDs by default when Locals advertises both. Both IDs use the same loaded model. Preserve exact aliases and model names that already end in `-thinking`.
- Add per-conversation **Thinking: Default / On / Off** controls that apply to the next reply without restarting the model. Clarify the server's startup default and the shared token budget for thinking and the answer.
- Show **Preparing**, **Thinking**, and **Generating** according to the response stream, rather than showing Thinking while waiting for any output.
- Update older management services to support thinking variants while retaining their running model workers.
- Attach the DMG and PKG to GitHub tag releases in addition to uploading them as Actions artifacts.

## 0.1.0

Initial macOS release with local chat, MLX model discovery and downloads, server management, a CLI, and DMG/PKG installers.
