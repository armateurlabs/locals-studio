# Locals Studio 0.1.0 validation

Local validation on October 25, 2026, on an Apple Silicon Mac with 24 GB of unified memory. Tests with fake workers, real MLX tests, and installable package checks are distinguished below.

## Automated tests and builds

**140 tests passed**: 60 core unit tests, 14 integration tests, 3 desktop tests, 9 helper tests, 12 catalogue tests, and 42 React component tests (37 application tests and 5 logo tests).

| Check | Result | Coverage |
| --- | --- | --- |
| Rust core | **74 tests passed: 60 unit tests and 14 integration tests.** | Fake HTTP servers and Python processes, without loading an LLM. The menu bar preference is disabled by default and persists after reloading without reactivating inference. |
| Downloads | Passed | Pinned revisions and manifests, 64 MiB checkpoints, pause/resume/cancel, recovery after interruption, 206 responses, 200 responses with prefix replay, sizes, SHA-256 and Git hashes, rejection of corrupted files. |
| Discover sizes | Passed | Sum of Safetensors files and a separate total for files allowed by Studio, in decimal GB (10⁹ bytes). Metadata tied to the immutable HF SHA, distinction between unknown sizes and partial sums, handling of missing sizes and overflows; no calculation from parameter counts or historical repository storage. Visible cards load their metadata with at most three concurrent requests and reuse the cache for each revision. |
| Discover catalogue | Passed | Grid/list/search/sort preferences persist. Numeric downloads, weights, quantization bits, and bundle disk-size sorting keep unknown values last in either direction. Complete sizes precede partial totals. The bounded persistent search cache retains stale rows during refresh and failures; same-SHA refresh preserves off-screen sizes and late metadata cannot overwrite newer popularity. |
| Model avatars | Passed | Persistent publisher cache, negative-cache backoff and offline reuse, official HTTPS redirect validation, bounded raster images, shared requests, family-to-publisher-to-chip fallback, decode failures, and at most four concurrent logo requests. |
| Libraries and persistence | Passed | HF and LM Studio, custom folders, references without copying, weights retained after unregistering, SQLite. |
| Supervision | Passed | Separate ports, process ownership, reconciliation, stopping during loading, active requests, bounded logs, unavailable counters, and stable CPU measurements. Global shutdown of the listener and workers, cancellation of queued starts, persisted OFF state with local control maintained; shutdown resumed after interruption. |
| Deleted profiles | Passed | No recreation after scanning or restarting, cleanup of old stopped workers, and alias reuse without intercepting the new ready worker. |
| Shared API | Passed | Two workers, exact aliases including `-thinking`, progressive streaming, cancellation on disconnect, Host/Origin, preflights, and public port conflicts. Empty lists allow all valid hosts and origins; populated lists filter them. Switching to an empty list is checked after restarting the worker, with `LOCALS_ORIGINS=*`. |
| Chat | Passed | Cancellation with the partial response retained; deleting a folder during a stream without losing the response. Atomic `conversations.move` updates `folder_id` and its timestamp without replacing messages or other conversation data, including during an active generation. Invalid destinations and malformed move requests leave stored data unchanged. |
| Frontend | **63 tests passed: 9 helper tests, 12 catalogue tests, and 42 React component tests.** | Drafts, navigation, events, stream completion, automatic and boolean settings, logs, resizing, folders, stopping during startup, and retaining content during CPU/RAM, log, and memory estimate refreshes; saving empty optional network lists, globally stopping the API/models, and selecting the memory card with current measurements and separate peaks. Folder regressions cover the separate create/rename/remove actions, dragging between folders and the No folder area, opening collapsed drop targets, the keyboard folder selector, ignored unrelated/duplicate drops, and failures preserving the source. Moves retain streaming responses, the model, and drafts; guards prevent races with Send and pending title saves, and stale polls cannot undo a completed move. The focused model and its logs are retained across view changes. Menu bar preference saved without changing server state, opening About through a native event, license text identical to LICENSE, and external link validation. Chat favors a ready model even after a stopped profile, tracks a worker becoming available without losing the draft, and preserves explicit choices and models in restored conversations; an old stopped worker does not hide the ready profile. Its model selector is below the input area. The Tauri transport is mocked only in tests. |
| Appearance and icons | Passed | Local Dark/Light preferences, Green/Sapphire/Violet/Amber/Rose palettes, and Standard/Large text, with larger default fonts. Shared shapes defined in `src/brand.json` for the interface logo, Dock, and menu bar; `npm run icons:generate` produces the macOS icons and monochrome template symbol. |
| Native menu | **3 tests passed.** | Natural sorting, stability during telemetry changes, unchecking an unavailable selected profile, menu identifiers, memory error detection, and validation of both HTTPS destinations. These tests do not operate the macOS menu on screen. |
| Builds | Passed | TypeScript, Vite, Rust formatting, and the Tauri release build with the embedded CLI. |

```sh
cargo test --locked -p studio-core -- --test-threads=1
cargo test --locked -p studio-desktop
cargo fmt --all -- --check
npm test
npm run build
npm run desktop:build
```

After translating the repository text into English, `cargo test --locked --workspace -- --test-threads=1` passed all **66 Rust tests**. Rust formatting, TypeScript/Vite, the release app/DMG build, signature verification, and DMG integrity checks also passed again. No additional real inference or LAN tests were run for this text-only update; those results are recorded below.

For the bounded inference tests described below, compilation was kept separate from managed MLX model loading to avoid distorting the memory budget.

The port-conflict investigation reproduced macOS allowing separate wildcard and specific-address TCP listeners on the same port with `SO_REUSEADDR`. New regressions reject an occupied public port across loopback/wildcard bindings in both directions and across IPv6/IPv4, report the listener PID, preserve the existing socket, and recover after saving the same configuration once the port is freed. Unit tests cover malformed/failed listener inspection and a terminated gateway; status reads remain responsive while the gateway lock is held. The Settings regression covers an API bind error, preserved origin-restart guidance, and a successful save retry. The source of the reported alternating APIs at one exact LAN URL has not been conclusively established; preflight cannot prevent a different program starting afterward.

After these checks, the main management service was updated with no active inference requests. It recovered in approximately **0.96 seconds**, retaining worker PID **41841**, private port **60777**, start time, configuration, profiles, and downloads. Studio health and the worker model list were checked without generating a response. The existing **127.0.0.1:8321** configuration was retained, with LAN mode disabled. Local evidence: `target/qa/port-guard-service-handoff.json`. The rebuilt app signature and DMG integrity were also verified.

The menu bar preference was also enabled and then disabled through the CLI with an isolated service, then read back after restarting that service: the state was retained, the API remained stopped, and no workers were loaded. Local evidence: `target/qa/menu-bar-cli.json`.

The chat-folder changes were checked in a separate native QA app with inference disabled and no loaded workers. Pointer gestures moved a saved conversation from Writing to Projects, back into a collapsed Writing folder (which expanded), and into No folder. Its messages and model were preserved. A folder created through the interface retained its separate plus button, which opened a new chat in that folder. Saved folders and conversation placement survived closing and reopening the QA window. The QA app and management service were then closed; the production worker **41841** remained ready. Screenshots and the report are under `target/qa/chat-folders/`, including `new-folder-plus-native.jpg`, `drag-to-folder-native.jpg`, `drag-to-unfiled-native.jpg`, and `final-validation.json`. Pointer threshold, cancellation, native-export cancellation, and duplicate-drop handling are also covered by component tests; moving during generation is covered by automated tests.

The main service was updated to make `conversations.move` available, recovering in approximately **0.91 seconds** without changing user folders, conversations, profiles, downloads, API configuration, or workers. Worker PID **41841**, port **60777**, and start time were retained. The new RPC was probed with a nonexistent ID, confirming validation without writing user data. Evidence: `target/qa/chat-folders/service-handoff.json`.

After unlocking the Mac, **About** was checked from Settings and the application's native menu, including the full offline MIT license and the Armateur Labs link. With an isolated QA copy and service (port 8391, no workers loaded initially), enabling the icon was saved in Settings and read back through the CLI; closing the window preserved the application PID with the option enabled, and the window could then be reopened. The user visually confirmed the “L” icon and its **Start selected**, **Stop server**, **qa-menu-*** menu. They then confirmed “Done, without errors” after manually completing this sequence: no models checked → **Start selected (API only)** → **Stop server** → check and then uncheck a model. This validation relies on their confirmation; clicks on these system menu bar items were not automated through CUA because this surface is not exposed by the control tool. Detection of an outdated service, before writing any settings, was also checked in the main application. The user's 0.8B worker remained ready with the same PID, 41841.

After that confirmation, the QA copy and its isolated service were closed; the test socket disappeared and no QA workers remained active. A QA 0.8B worker had also been loaded and then stopped during the tests, as shown by its logs and final state. The main service on **8321** and its worker **41841** remained available. Local evidence: `target/qa/menu-native/final-validation.json`.

The **Send** button fix was checked in the rebuilt native package: automatic selection of the ready 0.8B, **Stopped** status and an explanation that sending was unavailable after manually selecting the stopped 4B, then automatic return to the 0.8B with **New conversation**. Clicking **Send** produced **OK** (18 prompt tokens, 1 response token), with the test output capped at 8 tokens. The interface limit was then restored to 1024 tokens; no workers were restarted. Local evidence: `target/qa/chat-send-fixed-native.json` and `target/qa/chat-send-fixed-native.png`.

The main management service was then updated with no active inference requests, retaining the loaded worker: PID **41841**, private port **60777**, and start time unchanged after reconciliation. The API configuration and profiles were preserved, port **8321** became healthy again, and `config.desktop.menu_bar_enabled` is now available in the main service, disabled by default. Recovery and its checks took approximately **0.53 seconds**; this scenario involves a brief router interruption. Local evidence: `target/qa/menu-service-handoff.json`.

The rebuilt package was also checked in **Light / Sapphire / Large** and then **Dark / Violet / Large**: readable settings, accessible composer and server controls, and the model selector below the message field. Selection of the **stopped 4B** and its logs was retained after navigating to Chat and back, without starting the model or changing the selection checkboxes. The initial **Light / Green / Standard** preferences were then restored. Local screenshots: `target/qa/appearance-sapphire-large.png`, `target/qa/chat-violet-large.png`, and `target/qa/server-navigation-violet-large.png`.

In Discover, the cards and details for **mlx-community/Qwen3.5-0.8B-MLX-4bit** display **0.63 GB** of weights and **0.65 GB** total download at commit `5d894f8cc4ef3e6c88537bf3746ed262f549da6a`. The service was updated for this test in approximately **0.51 seconds**, with worker PID **41841**, port **60777**, start time, configuration, profiles, and downloads unchanged; the public and private HTTP model lists returned 200. No new downloads or generations were started during this check. Local evidence: `target/qa/discover-size-cards.png`, `target/qa/discover-size-details.png`, and `target/qa/discover-service-handoff.json`. The new logo assets and template symbol were generated from the same geometry; the new system menu bar silhouette was not clicked through automation.

Chat deletion was checked in an isolated native app with inference disabled: the sidebar delete button opened a confirmation naming the unselected target; Cancel preserved both saved chats. The composer opened the same confirmation for the selected chat, and Escape dismissed it. Both saved fixtures remained byte-for-byte unchanged. The QA app and its daemon were closed afterward. Actual deletion, failure/retry, duplicate-click protection, a reply starting during confirmation, preserving another chat's draft, and a pre-delete snapshot or queued Send attempting to restore a removed chat are covered by component tests. Evidence: `target/qa/chat-deletion/sidebar-confirmation-native.jpg` and `native-validation.json`. The frontend suite, release app build, app signature and DMG integrity passed; Rust sources were unchanged for this update.

The Discover changes were visually checked in an isolated native app with inference disabled: list layout, decimal GB columns, Qwen avatars in Discover and My models, and returning to Discover with cached rows and the saved list mode. Only public metadata and avatars were fetched. Settings now has a viewport-bounded fixed height and independently scrolling panels; its existing component tests pass. The final native General/About pointer-position comparison was not completed because the QA window received user interactions during the check. The QA app was left open for the user. Evidence is under `target/qa/discover-catalogue/`.

The management service was then updated from PID **17370** to **50165** in approximately **1.8 seconds**, preserving worker **41841**, its process identity, all server state, configuration, profiles, conversations, folders, and downloads. Health and the new logo RPC passed. No inference request was generated. Evidence: `target/qa/discover-catalogue/service-handoff.json`. The final app signature and DMG integrity passed again.

## Real runtime, download, and inference tests

- Runtime installed through `uv`: Python **3.12**, Locals **0.2.1**, dependencies locked with hashes. Distribution source pinned to commit `4f4d03f10232c9f2adeb1e96e2a7bd79a9e7fe19`.
- Complete download of **mlx-community/Qwen3.5-0.8B-MLX-4bit**, commit `5d894f8cc4ef3e6c88537bf3746ed262f549da6a`: **652,027,143 bytes**, weights verified with SHA-256 before publication. Continuous HTTP transport, `replayed_bytes=0`: this test does not establish real network resume behavior; that is covered by automated tests.
- **Qwen3.5 4B** inference through Studio port **8321**: HTTP 200, **OK** response, 18 prompt tokens and 1 response token. Bounded settings: context 512, output 16, cache 0.125 GB, 1 slot. Worker stopped after the test.
- **0.8B inference in the GUI**: started from Model Server, **OK** response, 18 prompt tokens and 1 response token. A second test of the final package, with context 512 and output explicitly capped at **64 tokens**, was interrupted with the Stop button: partial response retained, `cancelled=true`, **Stopped** status, and worker log confirming the client disconnection.
- Open network policy with empty lists: on the rebuilt service, arbitrary Host accepted (**200**), preflight from an arbitrary origin (**204**), SSE generations with an arbitrary origin and origin `null` (**200**, content followed by `[DONE]`). The direct Locals worker retains its Host check (**403** for that arbitrary host) and listens on loopback. Test profile capped at 512 context tokens and 8 output tokens, then deleted; user profile configurations and stopped state preserved. Local evidence: `target/qa/unrestricted-api.json`.
- Native closure of the final package: **GUI PID absent**, model still ready on **8321**, HTTP listing and CLI control operational. After reopening, the partial conversation, folder, selection, and profile configuration were restored.
- Global controls in the native application: **Stop server** closes **8321** and stops the worker, while `ping` on the management socket and the CLI remain operational. The OFF state is restored after reopening the GUI. Starting the test profile individually reactivates the API and allows a real `OK` generation capped at 8 tokens; the two selected user profiles are retained without loading automatically. The temporary profile is removed and the API is left stopped. Local evidence: `target/qa/global-controls-native.json`.
- The **Selected model memory** card was checked with the full names **Qwen3.5-0.8B-MLX-4bit** and **Qwen3.5-4B-MLX-4bit**, distinct aliases, weights on disk, and measurements from the corresponding worker. Unavailable values remain `—` after stopping, and peaks are grouped in a separate panel. Screenshot: `target/qa/model-memory-card.png`.
- Light/dark themes, profiles, logs, active/cache/peak MLX allocations, peak Locals RSS, and the estimate of memory that can be freed were examined in the native application. RSS and MLX allocations are not added together.

The independent Locals server already running on **8322** was preserved: PID unchanged and health checked. Profiles, conversations, and workers created only for these tests were cleaned up; downloaded models remain available.

## API from a second LAN machine

**12 of 12 checks passed** from another Mac, accessed over SSH, to the LAN address of the Mac hosting Studio on **8321**.

| Check | Result |
| --- | --- |
| Models | HTTP 200 before and after restarting the worker. |
| SSE generation without Origin | HTTP 200, content followed by `[DONE]`, output capped at 8 tokens. |
| Host | Configured name accepted; unknown host rejected with 403. |
| Origin | Allowed preflight 204; POST and preflight from an unknown origin rejected with 403. |
| Newly allowed browser | Worker already running: expected 403. After an explicit restart: SSE 200 with the allowed origin and Authorization header. This header does not establish LAN authentication. |
| Isolation | Both internal ports used successively by the worker were inaccessible from the other Mac. |
| Restoration | Original configuration restored exactly to **127.0.0.1:8321**, test workers stopped, independent server preserved. |

These were remote HTTP requests with the specified headers, not an interactive test in a remote browser.

## Artifacts and limitations

| Artifact | Result |
| --- | --- |
| `target/release/bundle/macos/Locals Studio.app` | **approximately 22.46 MiB**, embedded frontend at `tauri://localhost`, Vite stopped. |
| `target/release/bundle/dmg/Locals Studio_0.1.0_aarch64.dmg` | **approximately 7.84 MiB**, integrity validated with `hdiutil verify`. |
| Signature | **Ad hoc**, `codesign --verify --deep --strict` passed. Not notarized by Apple. |

The Python runtime and weights are installed separately and are not included in these sizes. The delivered source revision is the Git commit containing this report; artifacts are generated locally and are not versioned.

Unversioned local evidence is kept under `target/qa/`: `real-inference.json`, `lan.json`, `final-native.json`, and GUI screenshots. This report does not establish prolonged load testing, two MLX LLMs loaded simultaneously by Studio, an active real download while closing the GUI, or real resume after a network interruption. Resume and concurrency scenarios are covered by automated tests. The memory estimate remains partial for hybrid architectures and Auto settings; it is not a strict process limit.
