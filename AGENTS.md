# AGENTS.md — Worktable

Worktable is a native macOS notes app (Rust + GPUI + GPUI Component). This file
is the contract for anyone — human or AI — changing the UI or app code. It is
derived from the instructions that shaped this codebase, and it points at the
normative upstream guides rather than restating them.

## Required reading

Before touching UI code, read the GPUI Kit guides this project follows:

| Topic | Guide |
| --- | --- |
| Design (hierarchy, spacing, color, copy, states) | https://gpui-kit.com/docs/design-guides/ |
| Coding (architecture, state, naming, layers) | https://gpui-kit.com/docs/coding-guides/ |
| Testing (context tests, stable targets, UI integration) | https://gpui-kit.com/docs/test/ |
| Context (Window / App / Context / Entity conventions) | https://gpui-kit.com/docs/context/ |

They are normative here. **Must** is a constraint; **should** is the default and
needs a written reason to override. The app targets the pinned GPUI Component
revision in `Cargo.toml`; check the actual source/API docs before using a method
from memory, and never translate an example from another UI framework by
analogy.

The `karpathy-guidelines` agent skill is installed globally. Apply it while
working here: think before coding, surface assumptions, make surgical changes,
prefer the simplest code that solves the problem, and loop until a verifiable
goal is met instead of guessing.

## What "done" means

Every change must satisfy all of the following:

1. **Texts for everything.** Every control has visible text, or (for icon-only
   controls) a tooltip and an accessible name. Empty, loading, error, offline,
   disabled, and read-only states all have copy that says what happened and,
   when useful, how to recover.
2. **Guide-compliant styling.** Semantic theme tokens only (no raw hex/rgb/
   hsla), radii from the active theme, spacing/type/control sizes from GPUI's
   rem helpers or the component `Size` API, and no shadow on every card.
3. **Clean code.** `cargo fmt` and `cargo clippy` are clean; the workspace
   builds without warnings in both normal and `visual-tests` configurations.
   Dead code found in touched areas is removed, not parked.
4. **Tests.** Behavior is covered at the lowest layer that proves it; UI
   behavior is exercised through the real view with stable targets (see
   Testing below).
5. **A smooth debug app.** `cargo run -p worktable-app` is the expected
   developer loop and must stay responsive (see Debug builds below).

## Repository map

```
crates/
├── worktable-app/     # Shell + the single WorktableView feature
│   ├── src/main.rs            # Bootstrap, window, menus/status item, keybindings
│   ├── src/worktable_view.rs  # The whole main view (entries, assistant, settings)
│   ├── src/worktable_view_tests.rs  # UI integration tests
│   ├── src/design.rs          # Design tokens (radii, rem metrics, window chrome)
│   ├── src/assistant.rs       # Chat message model + rendering (TextView markdown)
│   ├── src/service.rs         # Tokio-side orchestration + event bus
│   ├── src/status_item.rs     # macOS NSStatusItem / global capture
│   └── src/bin/worktable_visual_test.rs  # Real Metal off-screen capture runner
├── worktable-ui/      # Motion kit + agent UI components (orbs, thinking state, streaming text, citations)
├── worktable-ai/      # rig-backed agent runtime, provider catalog, opencode-go provider
├── worktable-db/      # SQLite store (native) + in-memory store (wasm)
├── worktable-events/  # Cross-thread event types
└── worktable-helix/   # Embedded JSON knowledge graph + topic extraction
```

Keep one feature's model, view, commands, and dialogs together. The shell
composes features; it does not absorb their logic.

## Context conventions (GPUI)

- `Window` owns window-level state; `App` owns application/global state;
  `Context<Self>` owns the current entity; `Entity<T>` is the retained handle.
- Always name them `window`, `cx`, `view`/`entity` as appropriate. Never retain
  `&mut Window`, `&mut App`, or `&mut Context<_>` beyond the call that provided
  them; retain typed handles (`Entity`, `WeakEntity`, `FocusHandle`, scroll
  handles, ids) instead.
- `render` is declarative: read state, derive presentation, compose elements.
  Do not mutate state or send/start work from `render`. `cx.notify()` belongs
  after real mutations, never unconditionally.
- Work that outlives a frame goes through `cx.spawn`, captures a weak handle
  (`view.update(cx, ...)`), and reports through `Context` when it completes.
- Async work has explicit idle/loading/loaded/failed states; stale results are
  rejected by identity/revision rather than applied to new state.

## Navigation

The Library header owns the shell: the search field, the circular icon-only
Entries ⇄ Agent toggle, and the library menu. There is no separate Ask agent
button — the search field doubles as the prompt box: Enter sends the query to
the agent (switching pages). The menu carries Settings, the sort modes (the
checked item shows the active mode and direction; choosing it again flips
direction), and Quit. A native macOS menu bar mirrors the keymap: the app menu
(About, Settings ⌘,, Services, Hide/Hide Others/Show All, Quit), File
(New Note), Edit (standard text actions), View (Entries, Assistant, Toggle
Theme), and Window (Minimize, Zoom).

List keyboard commands are **mode-scoped**: `open_selected`, `delete_selected`,
copy/link commands, and arrow navigation only act on the Library page with no
detail overlay (`list_actions_allowed`). While one of the view's text fields
(search, note bar, assistant prompt) owns the keyboard, the list commands also
yield (`text_input_focused`), so Enter adds the note and Backspace edits the
field instead of opening or deleting a selected entry. This is what keeps
Enter in the agent, the note bar, or a dialog from acting on an entry the user
cannot even see.

Entry cards open their full content in a morphing overlay
(`EntryModalState`) that tweens between the card's **real prepainted rect**
and the middle of the UI over `MORPH_OPEN`/`MORPH_CLOSE`, with the content
laid out at its target size and revealed by the growing panel; closing shrinks
back into the same card. Triggers: double click, force click
(`MousePressureEvent`), the card context menu's *View entry*, and Enter/Open.
Escape, the backdrop, or Close dismiss it.

Agent answers that cite the knowledge base render through
`worktable_ui::InlineCitations`: aiCSS-style `[n]` marker chips in the prose
plus a sources footer. Hovering a chip shows the source preview (label,
content snippet, host). Every tool citation is a library entry, so clicking a
chip or footer row opens the same morphing entry view (from the card when the
Library is mounted, else from the click point); the entry view's own Link chip
opens the external URL. GPUI selects the scroll owner:
the detail body uses `TextView::scrollable(true)` because the component's root
takes the container height, so an outer scroll container cannot measure
overflow.

Tab moves focus between controls (GPUI Component `Root::on_action_tab` →
`window.focus_next`); the search field no longer intercepts Tab. Up/Down move
the entry selection, Enter opens the selected entry on the Library page.

The note bar under the list is always the input itself — no first click opens
a composer. It is `[＋ note input ✓]`: the plus opens the system image picker
(`App::prompt_for_paths`) and the tick (or Enter/⌘⏎) adds the note. ⌘N focuses
the input. Entry content has **no category attribute**: the legacy `kind`
column is dropped on migration, and images are recognized by content
(`content_is_image`/`image_source_for`). Image entries show a thumbnail in the
card; the detail shows the image itself, with a hover download button
(`prompt_for_new_path` → copy/fetch) and double click opening a borderless
fullscreen `WindowKind::PopUp` viewer above every other app that fades in with
`MODAL_OPEN` and closes on click or ⎋.

User message bubbles shrink to their text with an 85% max width; assistant
bubbles keep a definite 85% measure because their selectable rich text needs a
bounded frame.

The assistant transcript owns an explicit edge scrollbar and follows the
stream. Reasoning streams into a **collapsible thinking block**: long thoughts
render in a capped, scrollable body, and the run folds every block back to its
header when it finishes (the header stays clickable). The thinking cap exists
for correctness, not only looks — laying out unbounded reasoning on the UI
thread stalled the worker event channel and made later turns look hung.

While a run is active the send button is the S1 orb; clicking it cancels the
run (`WorkerRequest::Cancel`, handled synchronously by the agent runtime).
Cancellation also fires a `Notify` that the stream loop selects on, so an
in-flight provider call is dropped instead of waiting for its next chunk.
"Build knowledge" toggles its own icon to the G2 orb while it runs (one
button, not a swapped element) and a second click stops the pass between
phases/batches. Provider history is sanitized before replay: assistant
reasoning is stripped (providers reject or stall on reasoning as input), which
keeps the second and later turns healthy. The worker event channel is
**unbounded** and the pump drains every queued event per tick: a bounded
channel plus one-event-per-tick handling throttled long answers to a stall.

## Motion

Every transition rides one curve: transitions.dev's
`cubic-bezier(0.22, 1, 0.36, 1)` (`worktable_ui::EASE_TRANSITIONS`). Use the
existing `MotionSpec` constants instead of hand-rolled timings; modal dialogs
use `MODAL_OPEN`/`MODAL_CLOSE` (250ms/150ms, the CSS scale approximated with a
small rise because GPUI divs have no scale transform), and page switches use
`PAGE_SLIDE`. GPUI cannot blur element content, so blur-bearing specs
(transitions.dev's page/modal blur) fall back to the fade.

## Styling and tokens

`worktable-app/src/design.rs` is the single owner of derived values:

- **Radii** come from `theme.radius_tokens()`: `sm` for chips/badges/inline
  code, `md` for rows/cards/controls, `lg` for menus/popovers/floating bars.
- **Spacing, type, icon, and control sizes** use GPUI rem helpers at the call
  site (`gap_2`, `p_4`, `h_9`, `size_4`, `text_sm`) or a component `Size` tier
  (`.small()`, `.xsmall()`, `.large()`). Do not invent one-off heights.
- **Measured geometry** (virtual-list row metrics, reading column, animation
  offsets) is declared as `Rems` in `design.rs` and resolved with
  `design::to_pixels(value, window)` so it follows interface zoom.
  `design::content_column_width(window)` owns the responsive reading column:
  full viewport width on small windows, then 72% of the viewport clamped
  between `CONTENT_MAX_WIDTH` (45rem) and `CONTENT_MAX_WIDTH_WIDE` (62rem), so
  a fullscreen window widens the column instead of leaving a phone-width
  sliver in the middle.
- **Colors** come from `cx.theme()` by semantic role (`background`,
  `foreground`, `muted`, `primary`, `danger`, `border`, `popover`, ...). Never
  write a raw color in app code; if a role is missing, add it to the theme
  layer.
- **Window chrome** (macOS traffic-light inset, window min size, AppKit point
  sizes) is a documented physical/platform boundary and may use `px(...)`. Raw
  `px(...)` for product spacing, type, or ordinary control geometry is a review
  finding.

State is visible: hover, focus, selected, disabled, loading, and destructive
treatments come from the component's semantic API, not ad-hoc colors. Primary
buttons mark the one default commitment in a decision area, not the only or
most frequent action. Links (`gpui_component::link::Link`) are only for
external resources; in-app navigation uses Buttons/tabs.

### Agent UI components

`worktable-ui` owns the chat/agent presentation components, grouped by what
they do:

- `loading::Orb` — animated activity indicators (`OrbVariant::S1` thinking
  lattice, `OrbVariant::G2` globe), used for assistant waiting and long-running
  builds. Defaults to 1.5rem; the G2 globe fills its stage with the same dot
  weight as S1 so it reads at button sizes;
- `streaming::StreamingText` — typewriter reveal with a caret for live answers;
- `citations::InlineCitations` — `[n]` chips with hover previews and a
  source footer for answers with references.

They are ports of the MIT-licensed [aiCSS](https://www.aicss.dev) components
and stay theme-agnostic: callers pass colors from `cx.theme()` and rems, and
animated components take `.view(cx.entity_id())` so they can lease the shared
animation clock. Reduced motion renders a static frame and schedules no
frames. Keep new loaders/text effects in these modules rather than re-creating
them inline in a view.

## AI runtime (rig)

`worktable-ai` is the only LLM layer. It runs on [rig](https://rig.rs)
(`rig = "0.42"`, Tokio-native) — the embedded `pi_agent_rust` dependency was
removed entirely and must not be reintroduced. There is no second executor and
no sidecar.

Module ownership:

| File | Owns |
| --- | --- |
| `providers.rs` | the provider catalog: id, display name, `ProviderKind`, and the curated model list the UI may offer |
| `opencode_go.rs` | the custom `opencode-go` provider (OpenCode Go's OpenAI-compatible Chat Completions gateway; sets the required `x-opencode-session` routing header) |
| `agent_runtime.rs` | request dispatch, rig agent construction, streaming → `WorkerEvent` translation, session history, cancellation |
| `helix_tool.rs` | the `search_knowledge` tool for the Helix graph (native only) |
| `worker_protocol.rs` | the JSON request/event contract shared with the app |
| `runtime.rs` | sessions, leases, run lifecycle, and the event pump |

Provider rules:

- **One catalog owner.** `providers::PROVIDERS` is authoritative. The UI must
  never offer a model id the runtime cannot send; keep ids in sync with the
  provider's own endpoint docs.
- **Custom providers follow the rig docs.** `opencode-go` is built on rig's
  documented path for OpenAI-compatible endpoints: an
  `openai::CompletionsClient` with `base_url("https://opencode.ai/zen/go/v1")`
  and a stable per-conversation `x-opencode-session` header (the gateway
  rejects requests without it).
  Models served only through other API dialects (OpenAI Responses,
  Anthropic Messages) stay out of `MODELS` until the provider routes per model;
  listing them would fail at the first prompt.
- **Adding a provider** means: a `ProviderSpec` in `providers.rs`, a client arm
  in `AgentRuntime::build_agent`, and a credential handled through
  `SqliteStore` (Worktable's database owns credentials; env vars are only a
  fallback). Read the rig provider docs and the source before writing the arm —
  do not infer signatures from examples.
- **Keys and models stay consistent.** Saving a key activates the provider and
  auto-selects the catalog's first model when none valid is selected, so a
  configured provider can prompt immediately.
- **Builds are self-healing.** A build re-syncs entries whose graph copy is
  missing or stale (imports can add a description after the first mirror;
  edits change content) and drops their AI enrichment so the next pass renames
  them. This is what makes a GitHub star's description searchable even if the
  graph predates it.
- **Knowledge is built with AI when a provider is set up.** "Build knowledge"
  first mirrors entries locally (keyword topics + semantic links), then asks
  the active model for 2–5 topic phrases per entry in batches
  (`enrich_topics` in `agent_runtime.rs`, JSON-only responses). The graph
  records which entries the AI named so later builds only ask about new ones;
  without a provider the local pass still works and the UI says how to enable
  AI naming. Graph topics are exported to the view and drive topic grouping
  and entry search (`primary_topic` prefers them). The GitHub "Fetch stars"
  path lists the repositories the account **starred** (`/users/{user}/starred`
  with the star+json accept), matching what the import adds.
- **Streaming maps to the app's events.** `MultiTurnStreamItem` text becomes
  `AgentMessageDelta`, reasoning becomes `AgentThoughtDelta`, committed tool
  calls become `ToolStarted`, executed results become `ToolFinished`, and the
  run ends with `RunCompleted`/`RunFailed`. `PromptResponse::messages` seeds
  the per-session transcript so tool results survive into the next turn.
  Cancellation is an `AtomicBool` checked between chunks; `Cancel` never queues
  behind the run it cancels.
- **Tool runs need a turn budget.** rig's implicit budget is a single model
  call; `build_agent` sets `default_max_turns(MAX_MODEL_TURNS)` so
  `search_knowledge` can be followed by the model's answer. A tool call without
  this never reaches a final response.
- **`search_knowledge` emits citations.** The tool numbers its hits `[n]`,
  records them in a `CitationCollector`, and the runtime drains it into
  `WorkerEvent::Citations` (each carrying a content snippet for the hover
  preview); the UI attaches them to the answer that carries the markers. The
  graph file is re-opened per call so entries mirrored after the agent was
  built are still searchable.
- **Tests never call a provider.** Cover the catalog, client construction,
  snapshot/config behaviour, failure paths, and cancellation with I/O-free
  tests; live prompts belong to manual verification.

## Copy rules

- Sentence case for English UI. No periods on labels, buttons, menu items, or
  short states; complete explanatory/error sentences do take punctuation.
- Verbs for commands (`Save`, `Duplicate`), nouns for destinations (`Settings`,
  `Providers`), short phrases for states (`Offline`, `Syncing…`).
- Use the single ellipsis character (`…`) and only on commands that open a
  dialog/sheet/window or need more input before they can complete.
- Icon-only buttons need `.tooltip("…")` and an accessible name. Ellipsis
  states use component loading affordances (`.loading(true)`) rather than a
  bare `…` label.
- Status/validation text sits next to the control it describes. Prefer naming
  the result (`Saved`, `Import failed`) over ritual phrasing. Errors say what
  happened and the next recovery step.
- Name the product concept, never the engine: the graph is "knowledge" in
  every user-facing string (`Build knowledge`, `Knowledge is up to date`).
  Internal crate/module names (`worktable-helix`, `helix_tool`) stay internal.
- Use the same term for an object everywhere: toolbar, menu, context menu,
  dialog, shortcuts, and settings.

## Accessibility

- Every action is keyboard reachable; focus order follows the visual order and
  focus returns to the trigger after overlays dismiss.
- Use the standard component for its semantic role. Do not build a custom
  clickable `div` where a Button, Link, menu, or navigation component exists.
- Give list rows a role and accessible name (entry cards use
  `Role::ListItem` + `aria_label` + `aria_selected`); status lines use
  `Role::Status`; errors use `Role::Alert`.
- Do not communicate state through color alone; checked/selected/disabled
  states need a structural or textual cue.

## Testing

Run the fast layers while iterating and the UI layer before finishing:

```sh
cargo test -p worktable-ui          # pure motion math
cargo test -p worktable-app         # state + UI integration tests
cargo test --workspace              # everything
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

UI integration tests (`worktable_view_tests.rs`) follow the testing guide:

- Use `#[gpui::test]` + `TestAppContext`, open a real `Root::new(WorktableView)`
  window, and drive it with `VisualTestContext` (`simulate_click`,
  `simulate_keystrokes`, `run_until_parked`).
- Address controls by stable debug selectors, never by measured coordinates:
  elements register `.debug_selector(|| "…".into())`, and tests locate them via
  `cx.debug_bounds("…")`. Adding or moving a control requires adding/updating
  its selector.
- Assert the visible result **and** the application state. Re-query after every
  interaction; snapshots are immutable records of the last completed frame.
- Cover the interaction contract: pointer and keyboard activation, controlled
  values, disabled behavior, focus movement, dismissal, and empty/failure
  states.

The heavy macOS visual runner renders real Metal frames off-screen and writes
PNG baselines:

```sh
cargo run -p worktable-app --bin worktable_visual_test --features visual-tests
UPDATE_BASELINE=1 cargo run -p worktable-app --bin worktable_visual_test --features visual-tests
```

It must run on the main thread (it is a binary, not a libtest case). Use it to
verify visual facts the semantic tree cannot express; it is not a substitute
for the interaction tests.

Known limitation: do not open an entry `ContextMenu` from an in-process test.
The pinned GPUI Component revision leaks the menu entity through a refcount
cycle in its dismiss subscription, so GPUI's leak detector fails the test even
though app state is correct. Context-menu commands are covered through their
keyboard equivalents; re-enable a direct test when the dependency is upgraded.

## Debug builds

`[profile.dev]` keeps workspace crates at `opt-level = 1` with full debug info
and compiles dependencies at `opt-level = 2`. This is what makes the debug app
feel smooth (GPUI's layout/paint path is optimized) while keeping rebuilds
fast. Do not raise workspace `opt-level` to 3 and do not drop dependency
optimization without measuring the frame cost.

```sh
cargo run -p worktable-app                     # normal app
RUST_LOG=worktable=debug cargo run -p worktable-app
```

Debug-only first-run notes: themes are watched from `themes/` (override with
`WORKTABLE_THEMES_DIR`), the database lives under the platform data dir
(`WORKTABLE_DB_PATH` overrides it), and captures land in `~/.worktable/images`.
Failures in optional services (menu-bar item, theme watcher, Helix) log and
degrade gracefully; they must never panic or block the window.

## Commit hygiene

- Keep changes focused; do not reformat or "improve" adjacent code in the same
  change.
- Update tests and `AGENTS.md` when behavior or conventions change.
- Before opening a change, confirm: state/side-effect ownership is explicit,
  no raw colors or unexplained `px(...)` were added, every new control has
  text/tooltip/accessibility metadata, and the full test suite plus Clippy and
  rustfmt pass.
