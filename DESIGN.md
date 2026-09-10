# DESIGN.md — Worktable

This is the design system for the Worktable app: the concrete choices the code
makes, and the rules for adding to them. It applies the [GPUI Kit design
guide](https://gpui-kit.com/docs/design-guides/) to this product. Read that
guide first; this file does not repeat it, it records how Worktable follows it
and where the implemented tokens live.

## Design thesis

Worktable is a quiet, native desktop surface for capturing and revisiting
notes. Content is the interface: entry bodies, search results, and the
assistant conversation carry the screen. Chrome stays flat, dense-but-comfortable,
and keyboard-first.

The three product surfaces:

1. **Library** — Entries and Agent; the primary screen.
2. **Settings** — a line-separated category list (Appearance, Data,
   Providers); each category opens its own page.
3. **Settings sub-pages** — GitHub stars, entered from Settings and returning
   to it. Provider authentication opens as a modal dialog over Settings instead
   of replacing the page.

Navigation model: the Library header (search + circular icon-only Entries ⇄
Agent toggle + library menu) is stable; there are no header button groups and
no separate Ask agent button — the search field is the prompt box (Enter
sends; Tab moves focus to the next control). The toggle switches the pages
below with a
transitions.dev slide: the exiting page fades toward its own 8px offset and the
entering page fades in from the opposite side over 250ms
`cubic-bezier(0.22,1,0.36,1)`. Both pages stay mounted (list scroll and message
entrances survive switches); the inactive page is `display:none`. GPUI has no
element-level blur, so the spec's 3px blur is carried by the fade. The library
menu carries Settings, the sort modes, and Quit. Settings replaces the Library
body; its header's **Back** returns to Entries, and sub-pages' **Back** returns
to Settings. Every destination is also reachable by keyboard (`⌘1` Entries,
`⌘2` Agent, `⌘,` Settings), so no pointer-only path exists. From the search
field, **Enter** runs the query as a prompt; **Tab**/⇧**Tab** move focus
through the header controls, the cards, and the note bar. The native macOS menu
bar mirrors those keybindings (Settings is the app menu's ⌘, item).

Long entries open in a **morphing detail overlay**: the panel's rect tweens
between the entry card's real prepainted rect and the middle of the UI
(`MORPH_OPEN`/`MORPH_CLOSE`), content laid out at the target size and revealed
by the growing panel with a cross-fade (beui.dev's morphing modal, minus scale
and blur — GPUI has neither for divs). Positions convert from window space to
the content column's local space, so the morph starts on the card even when the
column is centered on a wide window; the panel fades with the tween. Closing
shrinks back into the same card. Triggers: double click, force click, the card context menu's *View
entry*, and Enter on the Library page; Escape, the close button, or the
backdrop closes it. Long bodies scroll inside
`TextView::scrollable(true)`, which owns the scrollbar and virtualization.
Image entries swap the body for the image itself: hovering reveals a download
button, and double clicking opens a borderless fullscreen popup above every
other app that fades in. The note bar is always the input itself
(`[＋ input ✓]`), and image entries show thumbnails in the list.


## Where the tokens live

| Concern | Owner |
| --- | --- |
| Colors, radii, typography, shadows | `gpui_component::Theme` (`cx.theme()`, `theme.radius_tokens()`) |
| Responsive reading column | `design::content_column_width` (45–62rem, 72% of the viewport) |
| Measured geometry (row metrics, reading column, titlebar inset) | [`crates/worktable-app/src/design.rs`](crates/worktable-app/src/design.rs) |
| Motion (specs, curves, loaders, hover fades) | [`crates/worktable-ui`](crates/worktable-ui) |
| Agent UI components (orbs, streaming text, citations) | [`worktable-ui`](crates/worktable-ui/src): `loading`, `streaming`, `citations` — ports of the MIT-licensed [aiCSS](https://www.aicss.dev) components, color- and size-parameterized by the caller |

Application code does not define raw colors or one-off pixel geometry. Spacing,
type, icon, and control sizes use GPUI's rem helpers (`gap_2`, `p_4`, `h_9`,
`size_4`, `text_sm`) or a component `Size` tier, so interface zoom
(`Root` maps the theme font size to `rem`) moves everything together. The only
`px` values in app code are documented physical boundaries: window chrome
(minimum window size, titlebar inset), theme token definitions, and the
platform AppKit sizes in the menu-bar item.

### Color roles

Colors are read from the active theme by meaning, never by palette position:

| Role | Used for |
| --- | --- |
| `background` / `foreground` | Window and primary text |
| `popover` / `popover_foreground` | Cards, composer bar, menus, floating surfaces |
| `muted` / `muted_foreground` | Search field, chips, metadata, secondary copy |
| `primary` / `primary_foreground` | Default commits, selection, active state |
| `danger` | Destructive actions and error copy |
| `border` / `input` / `ring` | Boundaries, fields, focus |
| `link` | External resources only (markdown links, GitHub repositories) |

The two supported themes are **Ayu Light** and **Ayu Dark** (⌘T toggles).
Every custom surface must be checked in both: nothing assumes light text or a
white background. Status is never communicated by color alone — badges carry
text/✓, errors and confirmations are sentences, selection carries a checkmark.

### Radius

Corner radii derive from the theme, which the app configures to the macOS Tahoe
scale (`theme.radius = 10`, `theme.radius_lg = 14`). Three semantic tiers:

| Tier | Value | Used for |
| --- | --- | --- |
| `radius_tokens().sm` | 5px | Topic chips, inline code, filename chips |
| `radius_tokens().md` | 10px | Entry cards, provider rows, controls |
| `radius_tokens().lg` | 14px | Composer bar, menus, popovers, floating surfaces |

Circles and pills use `rounded_full()`.

### Spacing, type, and size

Spacing follows the ecosystem scale (2, 4, 8, 12, 16, 24, 32 px at the default
rem) expressed as rem helpers. The reading column caps at **45rem** and is
centered; cards inset by `px_4` inside it.

Controls use component size tiers instead of bespoke heights:

| Tier | Frame | Typical use |
| --- | --- | --- |
| `xsmall` | 20px button/input | Dense provider controls |
| `small` | 24px button/input | Settings tabs, row actions, secondary buttons |
| medium (default) | 32px button/input | Header actions, primary commits, search |
| `large` | 32px button / 44px input | Sparse forms |

Icons are one family (GPUI Component's Lucide-based set) at 12/14/16/20/24px
tiers. Icon-only buttons always carry a tooltip and an accessible name.

Type stays near the ecosystem steps: `text_xs` for metadata and chips,
`text_sm` for body and labels, `text_2xl`/bold for empty-state titles. Bold and
semibold mark hierarchy; uppercase is reserved for the short section eyebrows
in the entry list and never for buttons or sentences.

## Surfaces and elevation

The window is flat. Regions are separated with background contrast and
hairlines, not nested cards:

- Entry cards: `popover` background, 1px `border`, no shadow.
- Section headers: eyebrow label plus a hairline rule.
- Composer bar and popovers/menus: `popover` background with a shadow because
  they float above content.
- Settings cards (e.g. GitHub stars): `popover` + border, no shadow.

Do not add a shadow to every card to make it "pop"; use spacing and grouping.

## Components in use

| Component | Where | Rules |
| --- | --- | --- |
| `Button` | Header, composer, providers, GitHub stars, entry detail | Default for ordinary commands; `primary` only for the commit of the current decision (Save, Send, Import, API-key save); `ghost` for quiet row/toolbar actions; `danger` reserved for destructive commits. Never a primary just because it is the only action. |
| `ButtonGroup` | Entries\|Agent, sort mode, settings tabs | Single-select segments. The selected segment is persistent, not hover-only. |
| `Input` / `InputState` | Search, composer, assistant prompt, API key, login prompt, GitHub username | Placeholder or adjacent label always present. Disabled while a required setup step is missing. |
| `ButtonGroup` (three icons) | Settings → Appearance theme mode | Light / dark / system; system follows the platform appearance. Icon-only segments carry tooltips. |
| `Switch` | Settings → Appearance "Show thinking" | Immediate-effect setting; the label sits beside it. |
| `DropdownMenu` | Library menu, model picker | The trigger owns open state, dismissal, and arrow-key navigation; it stays visibly pressed while open. |
| `ContextMenu` | Entry cards | Object-scoped commands (Copy, Copy as list, Open link, Delete). The same commands remain reachable from the keyboard (⌘C, ⌘⇧C, ⏎, ⌫). |
| `Link` | Markdown links, GitHub repository rows | External resources only; opens in the browser with a pointing cursor. In-app destinations use Buttons or the navigation components. |
| `VirtualList` + `Scrollbar` | Entries list | Fixed row metrics resolved from `rem` each render. Scrollbars belong to the scrolling region's trailing edge; content insets live inside rows, never on the scrolling container. |
| `Dialog` | Provider authentication (Settings → Providers) | Opens over Settings from a row's Configure button; holds the API key field or OAuth sign-in, the in-progress login panel, and its own status line. One commit action (`Save key`), quiet `Close` in the footer. |
| `TextView` (`gpui_component::text`) | Assistant message bodies, thinking blocks, entry detail | The component's selectable markdown renderer: headings, lists, links, inline code, code blocks, and window text selection. Assistant bubbles take a definite `85%` measure because selectable rich text needs a bounded frame; **user bubbles shrink to their text** with the same `85%` max. |
| `Orb` (`worktable_ui::loading`) | Assistant waiting/send and abort (S1), knowledge search/build and provider loading (G2) | aiCSS-derived activity indicator; theme colors passed in; reduced motion shows a static frame. The send button becomes the S1 orb while a run is active and cancels on click; "Build knowledge" carries the G2 orb and stops on a second click. |
| `ThinkingState` (`worktable_ui::thinking`) | Assistant reasoning label (Settings → UI "Show thinking") | aiCSS ThinkingState port: a dim band sweeps across "Thinking" (2.25s); each glyph is colored from the band because GPUI cannot clip gradients to text. The band's holds sit just past each edge of the label, so every loop restart is seamless. Reduced motion renders the static base color. The block is collapsible and long reasoning scrolls in a capped body; the run auto-collapses it when the answer lands. |
| `StreamingText` (`worktable_ui::streaming`) | Live assistant answers | Typewriter reveal with a steady caret while streaming; the finished answer swaps to markdown. |
| Entry detail overlay | Long entries | Morphs from the card's real rect; body text is inset like the header; footer chips are Copy + detected Link/Email/Call, with *Edit* at the right end switching to a markdown `Textarea` code view (Save/Cancel). |
| Boundary fades | Entries list | Top/bottom gradients soften the scroll cut; they hide when the list is at that edge. |
| `InlineCitations` (`worktable_ui::citations`) | Cited assistant answers | aiCSS port: word-level `[n]` marker chips in the prose plus numbered source rows. The chip's hover preview carries the source label, a content snippet (delivered with `KnowledgeCitation`), and the host. Clicking a chip or row routes through the app's citation handler: tool citations always open the morphing entry view (the entry's own Link chip opens the external URL). Paragraph breaks are preserved as wrapping blocks. |
| `CitationFooter` (`worktable_ui::citations`) | Not used by the app | The standalone footer remains for callers that render their own prose; cited answers use `InlineCitations`. |
| `Icon` | Throughout | Supplements a label; state-bearing icons are filled or colored only for their meaning. |

## Interaction states

- Rest: cards and rows are flat with a visible boundary; controls are quiet.
- Hover: a subtle `list_hover` wash blended through the motion kit; never the
  only cue for an action.
- Selected: entry cards take `list_active`, show the checkmark, and expose
  `aria_selected`; segmented controls take the selected button variant.
- Focus: components draw the theme focus ring; nothing clips it.
- Disabled: lower emphasis and inert (the assistant input/Send before a
  provider is configured).
- Loading: `Button::loading` preserves the label and blocks repeats; list and
  progress loaders animate, then settle into text ("Syncing…", "Importing…").
- Error: `danger` text with `Role::Alert`, naming what failed and the next step.

## Motion

Motion lives in `worktable-ui`: the zeron catalog (fade/rise entrances,
popover and dialog timing, loader pulses, hover color fades). It is
purposeful, short, and honors `App::reduce_motion`: reduced-motion snaps to end
states and schedules no frames. Motion explains change (list insertion slide,
pane slide, streaming chat) and is never required to understand state. No
component animates by default.

## Motion

One curve for every transition: transitions.dev's
`cubic-bezier(0.22, 1, 0.36, 1)` (`worktable_ui::EASE_TRANSITIONS`). Modal
dialogs open over `MODAL_OPEN` (250ms) and close over `MODAL_CLOSE` (150ms);
page switches use `PAGE_SLIDE` (250ms, 8px offset). GPUI has no element-level
blur or scale for divs, so the specs' blur is carried by the fade and the
modal scale by a 4px rise — the closest expressible port.

## Copy and voice

Sentence case, calm, concrete. Buttons name the result (`Save`, `Fetch stars`,
`Import stars to entries`); labels avoid periods; complete sentences do.
Ellipses are the single character `…` and only on commands that need more
input. Errors state what happened and how to recover ("GitHub API rate limit
reached. Set GITHUB_TOKEN to raise it."). Empty states point at the next action
("No entries yet — press ⌘N to create one."). One term per object everywhere:
entry, provider, model, assistant, library, knowledge. Never surface engine
names in UI copy: the graph is always "knowledge" (`Build knowledge`,
`Knowledge is up to date`), never the storage engine behind it.

## Accessibility checklist (per screen)

- Every action is keyboard reachable; focus order follows visual order.
- Focus returns to the trigger after menus dismiss.
- Icon-only controls have tooltips and accessible names.
- Lists expose roles and selection (`Role::List`/`ListItem`); status lines
  expose `Role::Status`; errors `Role::Alert`; the assistant log `Role::Log`.
- Text and boundaries have sufficient contrast in both themes.
- Content survives longer labels, larger text/zoom, and the minimum window
  size; only the region that overflows scrolls.

## Known limitations

The pinned GPUI Component revision retains an `ContextMenu`'s menu entity in a
reference cycle between its dismiss subscription and its menu state. Opening an
entry context menu therefore trips GPUI's test leak detector even though the
application is correct; the same behavior exists upstream. Until the dependency
is upgraded to a revision that clears the menu on dismissal, context-menu
interaction is verified manually, and its commands remain covered through their
keyboard equivalents (⌘C copy, ⌘⇧C copy link, ⌫ delete, ⏎ open).

## Verifying design changes

- `cargo test -p worktable-app` — interaction tests address controls by stable
  `debug_selector`s and assert both UI state and app state.
- `cargo run -p worktable-app --bin worktable_visual_test --features visual-tests`
  — real Metal captures in `target/visual_tests/` for visual review (entries,
  context menu, settings, note bar, image detail, assistant, wide/narrow
  layouts).
- When a control or surface changes, update its debug selector, capture, and
  this document if a rule changed.
