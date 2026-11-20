# Project import and export

Open **Import and export** in the workspace. Choose an active destination project, choose a format and upload the file. For a ZIP, the optional folder field selects one directory and its descendants. Leave it blank to select the archive root. A newer preview replaces your previous preview.

Review the proposed changes, conflicts, unsupported files, and unresolved links. When both copies have changed, choose which values to retain. **Import changes** applies the reviewed batch in one transaction. The preview expires 30 minutes after creation. Editing the destination project also requires a fresh preview. Repeating an identical import keeps existing records. Omitting a file keeps the existing project copy.

## Bring a Markdown vault

Zip the vault folder with its folder structure intact. Imports accept UTF-8 Markdown, including original frontmatter, comments and line endings. Supported image types are PNG, JPEG, GIF and WebP. Keep images at the relative paths used by your notes. A broken or ambiguous note link appears in the preview with its source path.

Each ZIP may contain up to 2,000 entries, including directories. The limits for both uploaded and expanded bytes are 64 MiB. Each Markdown note may be up to 2,000,000 bytes. An individual image may be up to 5 MB on Free, 10 MB on Plus or 20 MB on Max. Hidden paths, vault settings and unsupported attachments are skipped. The preview checks the decoded content against your workspace storage quota. A ZIP with an unsafe path or a symlink is rejected.

## Bring a Linear project

Export one project’s issues to CSV in Linear. Select **One Linear project** as the import format. The CSV limit is 8 MiB and 2,000 rows. The preview names the recognized fields. Imported issues retain the Linear source ID, original dates, and mapped status and priority. Parent links resolve after all rows are staged. Missing cells preserve existing values. Comments, Linear documents and attachment binaries require separate handling. A CSV establishes the project’s observed issue state as of import time.

## Download and restore

**Download ZIP** works for both active and archived projects. The export contains readable `Notes/`, `Issues/`, `Releases/`, saved `Checkpoints/`, and images beside the exported vault files. `manifest.json` stores stable IDs, paths, hashes and native fields. `history.jsonl` stores native project history and link destinations. The ZIP includes user-authored records and excludes permission-dependent provider metadata.

To restore a native export, select **Markdown vault or Workspace export** and upload the unchanged ZIP with the folder field empty. Its IDs and links are mapped into the destination. To edit a portable note, change the Markdown inside `Notes/`. A rename with unchanged bytes can be recognized when its hash identifies one file. For a simultaneous rename and content edit, update the matching file’s `path` in the manifest while retaining its original ID and base hashes. Keep release reading copies intact for structured release restoration.

Each drawing lives in `Canvases/<board-id>.json`. Its nodes, connections and pen strokes retain their positions. Named checkpoints and automatic recovery snapshots live in `Canvases/<board-id>/Checkpoints/<checkpoint-id>.json`. Keep these paths and their manifest entries together. Each canvas JSON file has an 8 MiB import limit. Import preview includes the current drawing and every retained checkpoint in the destination’s storage calculation. An import over the limit leaves the destination untouched.

When restoring to another project, Ratio maps issue and note pins to their destination records. If a source is missing, the preview reports the missing link and the node keeps its text and position. Omitting a board or checkpoint from the ZIP preserves any existing copy in the destination.

Full exports can exceed the browser limits. A native restore must also fit the 8 MiB manifest, 16 MiB history and 25,000 history-row limits. The current destination’s individual image and workspace quotas apply. Oversized ZIPs still open in standard archive tools. To bring only notes and images into another workspace, extract them from `Notes/`, keep relative paths, and split them into smaller vault ZIPs. This partial migration carries current Markdown and images. Full-service disaster recovery uses a PostgreSQL backup from the source service, restored by its operator. The browser has no single-project restore path for an oversized native history archive.

## Images and retention

Images use private, owner-scoped downloads. This implementation stores validated image bytes, MIME types and SHA-256 checksums in PostgreSQL so a database backup contains the image payloads and their referencing records. The baseline deployment uses this in place of a separate private object store.

Move an image to **Image trash** to start its seven-day recovery period. Trashed images continue to count toward storage. Restoring returns the same ID and path. Permanent deletion frees the retained bytes and leaves any Markdown references for you to edit. Expired rows are purged by the service worker. Until a trashed image is restored or purged, its path stays reserved.
