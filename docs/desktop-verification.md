# Ratio 0.4.0 desktop qualification

Ratio saves canvas SVGs and project ZIPs through the native file dialog. For a canvas export, the client uses the signed-in browser session and accepts the exact board export route on its workspace origin. It checks the returned media type and board identifier before offering to save. The suggested filename is `canvas-<board-uuid>.svg`. Temporary files belong to one download, and a name collision preserves the existing destination.

| Installer | Bytes | SHA-256 |
| --- | ---: | --- |
| `Ratio-0.4.0-windows-x64-setup.exe` | 1,345,114 | `91d2eb67759a43978a3666a7124359c451ee9b25971addf00bf79ffe2db1fba5` |
| `ratio_0.4.0_amd64.deb` | 660,864 | `17d94864bf61c05471568379a01474de317607d5a7893fc622f57e33816d1a1a` |

## Linux

The Ubuntu 22.04 build passed all 22 desktop tests. The package uses GTK 3, WebKitGTK 4.1 and bubblewrap. Its newest imported glibc symbol is 2.34. Packaging the frozen executable twice produced identical DEBs.

APT upgrades from the published 0.3.1 client passed on Ubuntu 22.04 with X11 and Wayland, and on Debian 12 with X11. Each upgrade retained its complete test profile and reopened the signed-in workspace. The Debian and Ubuntu guests also launched the installed desktop entry. An independent in-place upgrade retained all 43 files in a signed-in workspace profile and reopened Home.

On an actual signed-in canvas, **Tools → Export SVG** opened the GTK file chooser with the board’s suggested filename. The saved drawing matched the 6,322-byte authenticated response and contained the expected title and five nodes. A separate test saved the seeded Vault indexing board as a 7,638-byte standalone SVG and returned to the editor. The packaged app rejected a POST, a noncanonical board address, a sign-in redirect, the wrong media type and a forbidden response. Choosing an existing SVG filename preserved its original contents. Choosing another name on retry saved the expected drawing. The same desktop session saved a 27-entry project ZIP containing the matching board JSON.

## Windows

The x64 installer requests current-user privileges and includes the x64 WebView2 loader. The application and installer both report version `0.4.0.0`, include the expected ten icon images and require the Evergreen WebView2 runtime to launch the app. The installer is unsigned. Two independent compilations and packaging runs produced identical executables and installers. All 22 Windows-target tests passed in Wine.

The published 0.3.1 fixture upgraded in place under Wine. The 0.4.0 client read its saved origin and preserved the browser database, local storage, existing export and user-created file. Fresh installation, reinstall, uninstall and the missing-runtime check passed.

A Win32 dialog test exercised the production Save As function with a staged SVG. It showed the expected filename, cancellation left the Downloads folder empty, and saving wrote the original bytes. Accepting the system overwrite prompt still preserved the existing file and returned to Save As. The packaged WebView2-to-dialog path has a separate end-to-end check on the native Windows runner.

The tagged source workflow builds the desktop client and checks fresh and upgrade installations. The published Windows workflow separately verifies the release checksum and exercises the downloaded installer. [Desktop installer checks](https://github.com/MimoAlexer/ratio/actions/workflows/desktop.yml) · [Published Windows checks](https://github.com/MimoAlexer/ratio/actions/workflows/verify-published-desktop.yml)

[0.3.1 qualification](https://github.com/MimoAlexer/ratio/blob/v0.3.1/docs/desktop-verification.md) · [0.3.1 release and hosted results](https://github.com/MimoAlexer/ratio/releases/tag/v0.3.1)
