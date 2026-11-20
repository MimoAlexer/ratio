# The inset workspace

The useful details in Linear’s review screen are the boundary treatments and pane proportions. A dark gutter surrounds the workspace. A thin highlight along its upper edge and a darker opposing edge separate it from the sidebar. The selected review sits on a raised row; its contents occupy the larger pane. [Source screenshots and original links](linear-references/sources.md) · [Linear’s review inbox](https://linear.app/changelog/2026-05-27-linear-diffs) · [Linear’s design refresh](https://linear.app/now/behind-the-latest-design-refresh)

Ratio 0.2.1 applies that hierarchy to its own data. The issue queue shows a title and a second line for key, priority, findings and date. Selecting a row reveals its checkpoint, linked note and history in the reader. The full issue is available from either pane. Each pane scrolls independently when there is room, and phones keep a direct route back to the queue. A dark, lightly beveled New issue control leaves the selected record as the main focus.

The previous primary button had a slightly yellow-green fill (#e4e5e0), and the old selection and input colors had a green component. The new frame uses a #131314 sidebar, #181819 working surface, #1b1b1d reading sheet and #303033 rim. At 1440px the issue reader is 809px wide, up from 525px. The 1280px reader is 709px, up from 457px. Controls, editor surfaces, dialogs, and the desktop connection screen have matching upper and lower edges. Semantic colors still identify real states such as Done and critical findings. Monochrome mode makes those gray as well.

[Native issue workbench, before and after](ratio-before-and-after.png) · [Current issue workbench](../../images/ratio-issues.png) · [Current issue detail](../../images/ratio-issue-detail.png)

## Verification

The review used populated, authenticated routes in Chromium and the installed WebKitGTK client. Across 13 sampled routes, the default theme had no green-dominant structural fills, borders, gradients or shadows. The native list retained a green Done glyph; the issue detail and strict monochrome sample contained no green-dominant pixels. Browser checks covered list selection, opening a full record, back/forward, independent scrolling, keyboard focus, and repeated selection on phones at 1440, 1280 and 390px. Tests with JavaScript disabled followed the real server routes. No horizontal overflow was observed at these widths.
