# Ratio 0.2 design review

**The main change is the issue workbench.** On a wide screen, you can compare real issues and read the selected issue’s saved instruction, source note and recent activity without leaving the queue. On a phone, the issue list keeps its full width and opens an inspector on request. Every issue still has a direct link to the complete record.

[Before and after gallery](review.html) · [Image index](galleries.md) · [Three directions explored](explorations.md) · [Design system](../design-system.md) · [Research](research.md)

## What the audit found

The old app already had strong underlying data: real checkpoints, source paragraphs, milestone dependencies and undoable issue transitions. Its layout hid these relationships. At desktop width the ledger left a large gap between an issue title and its supporting fields. The full-width import control measured 1,117 px. Fourteen navigation destinations competed for attention, and the project filter and global search did not always agree about scope. A first-time user could see “No matching issues” without having applied a filter.

Task failures were more important than the styling. On a 25-issue phone fixture, New issue jumped the document 1,895 px. A blank-title submission opened a generic error page. Under Linux WebKit, focus could fall back to the document after using the search shortcut. The form-field border reached only 2.37:1 against its fill. The existing primary and secondary text generally had sufficient contrast; raising every text color would not have fixed the field boundary.

## Research and design decision

Linear’s redesign articles describe treating global navigation and contextual actions as different layers and testing changes in the running product. Its issue and display documentation shows how a dense list can preserve selection while keeping less common controls available. Vercel’s guidelines informed focus restoration, optical alignment, responsive testing and useful recovery states. Refactoring UI informed the separation of visual emphasis from semantic heading depth. The [research notes](research.md) distinguish what was observed from what was inferred. No upstream screenshots or brand artwork are reproduced in the product.

Three interactive studies were rendered at 1440 × 900 using the same 19 Luma issues. The ledger was best for scanning. The split workbench was best for recovering the reasoning behind an issue. The focus queue was best for resuming work, but hid wider project context. Ratio therefore uses a ledger with an optional inspector for Issues and the checkpoint-led layout on Home. The selected record’s margin mark ties those views together. [Explore the studies](explorations.md)

## Four live comparisons

Each stage was rendered from the Rust application against PostgreSQL, using seeded and synthetic workspace data. The requested sizes were 1440 × 900, 1280 × 800 and 390 × 844. The reviewers also tested 320px layouts, 200% browser zoom, short landscape windows, true monochrome, forced colors, and JavaScript-disabled forms. A partial stylesheet served during an early development build was discarded as visual evidence; the first official cycle used the complete frozen stylesheet.

| Pass | Observed in the running app | Change and recheck |
|---|---|---|
| Baseline → first build | The list lost the next action when scanning. New issue and search moved the viewport. Notes and import forms had no useful reading measure. | Added a source-backed inspector, a header composer, command search and a compact file ledger. The desktop queue and checkpoint became visible together. |
| First → second | Enabled primary buttons could read as disabled, including 1.09:1 on a form action. The inspector’s Open issue action fell below an 800px laptop viewport. Home repeated one critical finding in two queues. | Fixed the shared control hierarchy, moved Open issue into the inspector header, placed the saved instruction first and removed duplicate queue entries. Primary-action contrast measured 13.51:1 in graphite and 14.09:1 in monochrome. |
| Second → third | The phone issue list still devoted a mostly empty line to the layout toggle. A long note path wrapped to six lines. Cycle-time source rows stacked two separate controls. | Reorganized mobile controls, retained the complete path and its accessible name, and placed independent issue/timing targets in one row. The first normal phone issue moved from y318 to y266. Median timing-row height fell from 98 to 73px with two distinct 44px targets. |
| Third → release candidate | Pressing Enter in search could activate Save this view. A short landscape filter hid Apply below the viewport. Editable issue status followed seldom-used disclosures on a phone. | Restored the native default form submission, confined short-screen panels and actions to the visual viewport, and placed mobile Status directly after the checkpoint/source. At 844 × 390, Apply and Create now appear at opening without scrolling the document. Status moved 147px higher on the tested issue. |

## Changes beyond the issue list

- **Home and navigation.** The saved instruction has a direct resume action. The ready queue excludes work already shown as critical. Current project and current location have distinct treatments. Manage folds away until needed.
- **Search and commands.** Ctrl/Cmd+K finds authorized projects, issues and notes and offers actions in the current scope. Arrow keys and Enter operate the dialog; Escape restores focus. Slash still focuses inline issue search. Slow responses cannot activate stale results.
- **Notes and projects.** Project and revision metadata now align in a file ledger. Filtered location survives edit, preview, save and cancel. A long path stays accessible without consuming multiple lines. On a phone, the project’s current-work target grew from 20px to 47px.
- **Planning.** Burnup and scope changes lead to their underlying issues. Cycle time separates the cohort median from the selected observation. Roadmap and health expose the blocker or release behind an indicator. Exact timestamps are available on demand.
- **Settings, transfer and billing.** Form widths suit their content. Invalid input stays editable and receives focus. Import decisions distinguish stale, unchanged and conflicting records. Billing uses comparable plan rows, and a disconnected provider’s status stays separate from real findings.
- **Foundation.** Shared tokens define the surface hierarchy, three text roles, type scale, spacing, density, focus and motion. Page and section headers, toolbars, records, empty states and form actions use shared primitives. A content-validated local asset bundle serves the same CSS and scripts in the web and desktop clients.

## Release assessment

| Criterion | Evidence and judgment |
|---|---|
| Visual hierarchy | The checkpoint is visible beside the chosen issue. The full issue, source and history remain subordinate and reachable. |
| Typography | Issue titles and actions use Instrument Sans; keys, dates and quantitative comparisons use tabular or monospace text. The smallest displayed metadata was checked at real window sizes. |
| Spacing and alignment | List columns, inspector header and reading rails align. The phone’s first issue now appears 9px above its original position despite gaining preview access. |
| Color and contrast | The interface uses primarily luminance. Across the release-candidate samples, measured HTML text was at least 6.04:1 and sampled SVG labels at least 5.62:1. Field boundaries, keyboard focus, selection and current scope have separate treatments. |
| Information density | The 25-issue phone fixture fits nine issue links where the original fit seven. Desktop comparisons retain visibility of the selected issue’s context. |
| Consistency | The same action, reading and disclosure patterns now extend through Notes, Projects, planning, account and transfer screens. Special-purpose diagrams keep their own geometry. |
| Navigation | Native links preserve filtered routes. Project context, selected row, keyboard focus and page location can be distinguished in graphite and monochrome. |
| Micro-interactions | The verified paths include Space to inspect, Escape to return, filter dismissal on focus departure, correction of invalid submissions and return to a saved note draft. |
| Accessibility | Sixty authenticated keyboard scenarios passed across Chromium, Firefox and WebKit. The release candidate passed 84 touch/zoom/reflow checks. Redundant shapes/text, native links/forms, visible focus and reduced motion were checked. These checks do not constitute a full assistive-technology certification. |
| Originality | The workbench is recognizable as Ratio through the continuity of its checkpoint, source and release evidence. It deliberately uses familiar desktop conventions for actions where novelty would slow the user down. |

## Engineering evidence and limits

The default wide workbench adds one bounded source/context query relative to the original bare list. Profiling the ordered directory path on 10,000 issues exposed a wide count/window scan. A scoped count and indexed page query retained matching records and totals. With 25 concurrent readers, the median of three local list p95 measurements on otherwise identical redesigned builds fell from **102.05 ms to 57.70 ms**; search stayed near 58 ms. These are development-host timings, not hosted latency promises. [Reproducible load test](../../../tests/read_load.rs)

The release keeps existing accounts, provider grants, note drafts, exports and issue-history formats. Product screenshot evidence uses synthetic records. Long-term preference, real-user completion rates, hosted provider setup and field performance remain to be measured. [Desktop qualification](../../desktop-verification.md)

## Reproduce the interaction checks

Use the seeded local service and the [browser regression script](../../../qa/browser/README.md) for a compact check of native search submission and command navigation at desktop, laptop, phone and short-landscape sizes, with JavaScript on and off. Run the [server and platform checks](../../../README.md) for the larger database and installation suites.

A separate corrective review confirmed that Escape restores the issue row, Enter retains the Ready filter in all three browser engines, and the status control precedes optional integrations on phones. Opening and closing inline phone search with JavaScript preserves scroll offsets of both 0 and 60px. Without JavaScript, its anchor remains operable but can scroll to the top. The elapsed time of a task for a new user has not been measured.
