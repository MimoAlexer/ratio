# Ratio interface system

Ratio uses one shell for two different tasks. A developer scans many records in the issue list, then reads a single description, checkpoint or source note. A short vertical mark identifies the selected record or current checkpoint. Group headings carry their own context so each row can give more space to its title.

## Surfaces and type

| Token | Use |
| --- | --- |
| `--sidebar`, `--canvas` | A dark neutral navigation rail and a slightly lighter inset working surface |
| `--rim`, `--edge-light`, `--edge-dark` | One-pixel perimeter, upper lip, lower shadow; use together for a workspace or recessed reading pane |
| `--surface`, `--raised`, `--hover`, `--selected` | Group or reading surface, raised control, pointer feedback and selection |
| `--fg`, `--fg-2`, `--fg-3` | Issue titles and edited text, supporting labels and quiet metadata |
| `--subtle-line`, `--line`, `--control-line` | Row division, group boundary and field outline |
| `--accent`, `--risk` | Focus or an active checkpoint, and a finding requiring attention |
| `--s1` through `--s8` | Spacing in 4 px steps from 4 to 32 px |
| `--row`, `--control` | 36 px records and 32 px controls by default, 44 px in the phone shell |
| `--radius`, `--radius-panel` | 4 px for controls and 6 px for elevated reading surfaces |

Use Instrument Sans for labels, actions and prose. IBM Plex Mono distinguishes issue keys, filenames, short dates and keyboard hints. The base type scale is 12 px metadata, 13 px controls, 14 px body, 16 px section headings and 24 px page titles at default browser settings. Give running text a 1.5 line height and a reading measure of up to 66 characters. The Monochrome theme replaces chromatic tokens with neutral values. Selected, dangerous and current states also have text, icons or shape.

## Shell and navigation

The 196 px desktop rail groups daily work, project shortcuts and planning. A filled row locates the current section. A short horizontal branch marks its project context when one applies. Manage stays collapsed until it contains the current route or someone opens it. There is one Settings destination in Manage. Account options expose the signed-in address, theme and sign out. The adjacent question-mark button opens keyboard help.

Call `ui::section_page` for a section index. Its header gives the group and the page heading identifies the view. Use `ui::page_in_project` for a child page with a known project. It renders the parent section link, scoped issue-search fallback and project identifier for the command menu. Pass `None` or use `ui::page` for an unscoped child. The mobile shell switches at 760 px. Its native navigation disclosure supports keyboard focus and remains usable when scripts are disabled.

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

In the desktop issue workbench, the queue uses about a third of the working pane; the rest is reserved for the selected issue. The 8px shell gutter and opposing light and dark edges give the desktop workspace depth. A phone uses the available width for the queue and then the selected issue. [Reference images](ratio-0.2.1/linear-references/sources.md)

The [foundation stylesheet](../../assets/app.css) imports the local fonts. The [page renderer](../../src/ui.rs) adds route styles after the foundation, followed by the private command stylesheet. Scope feature-specific selectors to their view. Buttons, inputs and layout defaults use low specificity so a local layout can change density without competing with global selectors.
