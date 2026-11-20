# Three directions for the same issue

These are design studies. Each uses the same Luma sample and a locally rendered issue list; edits inside a study are temporary. The application itself uses PostgreSQL and the existing authenticated routes.

| Direction | Where it worked | What the comparison exposed |
|---|---|---|
| [Precision ledger](studies/ledger.html) | Comparing titles, priority, project and recency on shared baselines made the list easy to scan. | A developer had to open a record to recover the saved instruction. Status totals repeated across navigation and headings. |
| [Issue workbench](studies/workbench.html) | The issue navigator stayed in place beside the next action, history and source note. It took little effort to inspect another candidate. | A permanent second pane sacrifices table width and does not fit on a phone. |
| [Focus queue](studies/focus.html) | The current checkpoint and the issues blocking public beta made resuming interrupted work immediate. | Using it as the whole issue browser would obscure equally important work outside the next release. |

[View the three rendered concepts](review.html#concepts)

The production design keeps ledger rows, opens a source-backed inspector when the display can support it, and leaves the complete issue and board one action away. Home uses the focus queue’s saved instruction and distinct critical work. A single shell serves all these tasks. On a phone the workbench becomes a list with an explicit preview action. Cross-project handling was subsequently exercised using a multi-project database fixture in the running application.
