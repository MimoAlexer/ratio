# Ratio interface system

Ratio uses one shell for two different tasks. A developer scans many records in the issue list, then reads a single description, checkpoint or source note. A short vertical mark identifies the selected record or current checkpoint. Group headings carry their own context so each row can give more space to its title.

## Surfaces and type

| Token | Use |
| --- | --- |
| `--atmosphere`, `--surface-sheen` | Use a low-contrast near-black to charcoal fade outside the frame and a faint upper-left highlight inside it |
| `--shell`, `--canvas`, `--raised` | Navigation rail (#101010), working surface (#171717) and selected row or floating control (#252525) |
| `--rim`, `--edge-light`, `--edge-dark` | One-pixel perimeter, upper lip and opposing dark edge around the working surface |
| `--surface`, `--hover`, `--selected` | Reading areas share the canvas color. Hover uses a translucent overlay. Selection uses the raised surface |
| `--fg`, `--muted-ink` | Primary text (#eeeeee) and metadata (#aaaaaa). Secondary and tertiary text use the same muted ink |
| `--subtle-line`, `--line`, `--control-line` | Row division, group boundary and field outline |
| `--accent`, `--risk` | Focus or an active checkpoint, and a finding requiring attention |
| `--s1` through `--s8` | Spacing in 4 px steps from 4 to 32 px |
| `--row`, `--control` | 36 px records and 32 px controls by default, 44 px in the phone shell |
| `--radius`, `--radius-panel` | 5 px for controls and 8 px for the inset workspace and floating menus |

Use Instrument Sans for labels, actions and prose. IBM Plex Mono distinguishes issue keys, filenames, short dates and keyboard hints. The base type scale is 12 px metadata, 13 px controls, 14 px body, 16 px section headings and 24 px page titles at default browser settings. Give running text a 1.5 line height and a reading measure of up to 66 characters. Both themes are grayscale. Monochrome increases the contrast of type, controls and dividers. Distinguish chart series by stroke, marker and label. Give findings a text label or a recognizable shape.

## Shell and navigation

The desktop rail is 182–232 px wide and groups daily work, project shortcuts and planning. A filled row locates the current section. A short horizontal branch marks its project context when one applies. Manage stays collapsed until it contains the current route or someone opens it. Account options expose the signed-in address, theme and sign out. The adjacent question-mark button opens keyboard help. In the installed 0.3 desktop app, F10 and the ellipsis beside the Ratio name open the native controls menu.

Call `ui::section_page` for a section index. Its page heading identifies the view directly. Use `ui::page_in_project` for a child page with a known project. It renders the parent section link, scoped issue-search fallback and project identifier for the command menu. Pass `None` or use `ui::page` for an unscoped child. The mobile shell switches at 760 px. Its navigation disclosure supports keyboard focus and remains usable when scripts are disabled.

## Building a view

| Task | Shared primitive |
| --- | --- |
| Align a title and its actions | `.view-head`, `.view-actions` |
| Place controls below the title | `.view-tools` |
| Introduce a group of related records or settings | `.section-head`, `.content-section` |
| Read a document or edit a compact form | `.reading`, `.form-stack` |
| Scan comparable records | `.record-list`, `.record-row` with project-specific grid tracks |
| Explain an outcome or first action | `.status-note`, `.empty-state` |
| Emphasize an intended action | `.action-primary` for the main action, `.action-secondary` for a supporting operation, `.action-quiet` for a low-emphasis action |

Place a field error beside its input, preserve submitted values and connect the error using `aria-describedby`. Use a native form for a write, a link with a real destination for navigation and native details/summary for disclosure. Keep a visible focus outline. Stateful menus support Escape and restore focus to their trigger. New content should remain usable at browser zoom, with long project names and on a phone.

On desktop, Issues opens as a full-width list. Selecting a record moves the queue to about a third of the working pane and opens the reader alongside it. Give each issue group a plain text heading and reserve the filled background for the current row. The inset frame is 6 px inside the shell. The outer margin widens with the window and narrows to 5 px on phones, where the queue and selected issue open in sequence. [Desktop captures and reference gallery](ratio-0.3/README.md)

The [foundation stylesheet](../../assets/app.css) imports the local fonts. The [page renderer](../../src/ui.rs) adds route styles after the foundation, followed by the private command stylesheet. Scope feature-specific selectors to their view. Buttons, inputs and layout defaults use low specificity so a local layout can change density without competing with global selectors.
