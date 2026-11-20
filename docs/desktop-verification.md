# Ratio 0.2.0 desktop qualification

The private release provides x86-64 installers for Windows and Linux. Verify the downloaded files against the release’s `SHA256SUMS`. [Ratio 0.2.0 release](https://github.com/MimoAlexer/ratio/releases/tag/v0.2.0)

| File | Bytes | SHA-256 |
| --- | ---: | --- |
| `Ratio-0.2.0-windows-x64-setup.exe` | 1,343,150 | `699086b5326a2156b5cd85026bed0f9f5c8f802a5e6823241b35e58de5e3a152` |
| `ratio_0.2.0_amd64.deb` | 660,392 | `9f213f0bbf4b9cf33ed85ab4c093e888564ebd5652b10236f050247d3722f73b` |

## Connection and workspace

The launcher places the workspace address, recovery message and Connect button on one reading edge. First launch focuses the URL field. When a saved server is unreachable, the launcher offers Retry connection, Use another address and Forget saved address. The signed-in workspace comes from the selected Ratio server, so its host must also run the redesigned server to serve the new views.

The first-run comparisons show the same 1280 × 800 Linux window with the native application menu. The packaged client was also checked at 1440 × 900 and at its minimum window width of 520 px. The separate 390 × 844 check used the launcher document in WebKit. [Previous launcher](design/ratio-0.2/images/native-connection-before.png) · [Current launcher](design/ratio-0.2/images/native-connection-after.png)

The Linux WebView exposed the focused URL field to accessibility tools on first launch, kept a rejected address available for correction and focused Retry connection after a connection refusal. The packaged app then paired through a separate browser, opened the sample issues and forgot the saved server on request. Dark and light launcher text measured at least 6.04∶1 against its background, and input boundaries measured at least 3.72∶1.

Orca 50.3 spoke the full rejection reason after a remote HTTP address was pasted and submitted, repeated it when focus moved to Connect, and announced the offline reason on Retry connection. With simulated character-by-character typing, a run announced only the invalid state on Enter. The field retained focus and Orca’s braille output included the complete explanation. Editing the address cleared the invalid state.

A separate sign-in test used the archived 0.1.1 Linux client and a synthetic Luma account. After the older process quit, the 0.2.0 package reopened the same WebKit profile and remembered origin. It displayed the same authenticated issue list without a second pairing.

The 1440 × 900 workspace captures came from the packaged 0.2.0 Linux client connected to the redesigned server. Both show LU-32 from the same read-only Luma sample. [Issue list and inspector](images/ratio-issues.png) · [Full issue with activity](images/ratio-issue-detail.png)

## Platform checks

| Environment | Verified on the 0.2.0 artifacts |
| --- | --- |
| Ubuntu 22.04 and Debian 12 | APT clean installation, upgrade from published Ratio 0.1.1 and Working Set 0.1.0 packages, and purge. Each case checked the installed executable, desktop entry and icon. During upgrade and removal, synthetic saved-origin and profile data retained their original bytes. The distribution userspaces ran on a shared Linux kernel. |
| Windows Server 2025, published installer | Three independent clean workers downloaded the private release, checked its SHA-256, installed it, found the rendered Connect control through Windows UI Automation and uninstalled it. Both the 0.1.0 and 0.1.1 upgrades retained the profile. [Published installer checks](https://github.com/MimoAlexer/ratio/actions/runs/44806476304) |
| Windows x64 installer under Wine | Clean per-user installation, upgrades from both published predecessors, Start menu shortcut, uninstall registration, PE payload and removal. Profiles and unrelated files survived the installer lifecycle. The silent installer returned status 3 when the WebView2 prerequisite was absent. |
| Desktop unit suites | All 20 tests passed in the Ubuntu release build and as Windows binaries under Wine. Coverage includes origin parsing, isolated profiles, legacy upgrade paths, browser handoff and native export rules. |
| Packaging | Repackaging each exact release executable twice produced byte-identical installers. Windows Explorer metadata reports 0.2.0.0, and all ten embedded icon images matched the application artwork. |

The Linux package uses GTK 3, WebKitGTK 4.1, bubblewrap and glibc 2.34 or newer. The Windows installer is unsigned and requires Microsoft Edge WebView2 Evergreen. The native Windows workflow launches the installed executable and looks up its Connect button through Windows UI Automation. [Windows release verification](https://github.com/MimoAlexer/ratio/actions/workflows/verify-published-desktop.yml)

The 0.1.1 release and its original compatibility evidence remain available. [Archived installers](https://github.com/MimoAlexer/ratio/releases/tag/v0.1.1) · [0.1.1 qualification](https://github.com/MimoAlexer/ratio/blob/v0.1.1/docs/desktop-verification.md)
