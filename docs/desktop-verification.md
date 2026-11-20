# Ratio 0.4.1 desktop qualification

Windows can spell a temporary directory as `C:\Temp` or `\\?\C:\Temp`. Ratio now matches export downloads to the spelling WebView2 uses for the same file. The download still goes into a private, per-operation folder, and an existing destination file is preserved. The fix covers canvas SVGs and project ZIPs.

| Installer | Bytes | SHA-256 |
| --- | ---: | --- |
| `Ratio-0.4.1-windows-x64-setup.exe` | 1,345,910 | `63bca487faed63a67964c5d05a4990d9930d7de49d138f75326c5509945284dd` |
| `ratio_0.4.1_amd64.deb` | 661,184 | `e96bb402a1aa479871f3c0351760e0be04addce260158570c1007667975b0dc6` |

## Windows

Both path regression tests failed with the 0.4.0 lookup. The 0.4.1 tests cover ordinary paths, explicit extended-length paths, long paths that need the prefix, and two concurrent exports with the same filename. Ratio uses the same path conversion as its WebView2 integration. All 24 Windows-target tests passed in Wine. A native dialog probe wrote the expected SVG bytes using both an ordinary temp path and a child-only extended-length temp path. The probe checked that the file was staged in the selected temp directory and that the parent’s environment stayed unchanged.

The published 0.4.0 installer upgraded in place to 0.4.1 and retained the saved server, browser session data, export and user-created file. A fresh installation, reinstall, uninstall and missing-runtime handling passed. The x64 installer runs per user, requires the Evergreen WebView2 runtime and is unsigned.

The hosted fresh-install test opens an SVG through the installed WebView2 client twice. The second launch gives only that client an extended-length temporary path. It checks activity inside that folder, the native Save dialog and the exact saved file bytes. [Published Windows verification workflow](https://github.com/MimoAlexer/ratio/actions/workflows/verify-published-desktop.yml)

## Linux

The Ubuntu 22.04 release build passed all 22 desktop tests. Repackaging produced identical DEBs. The client uses GTK 3, WebKitGTK 4.1 and bubblewrap, and its newest imported glibc symbol is 2.34.

A direct upgrade from the published 0.4.0 package preserved the authenticated workspace profile. On Ubuntu 22.04 with X11 and Wayland, all 14 files in each test profile matched across installation. In a separate real workspace, all 43 profile files survived the upgrade. The installed client exported a five-node drawing and a project ZIP, preserved existing SVG and ZIP destinations, and wrote the expected bytes when another name was chosen.

## Previous release

The published 0.4.0 Windows package passed all three native release scenarios with the corrected test harness. The fresh-install worker opened the SVG from WebView2, confirmed the native Save dialog and verified all 83 saved bytes. [0.4.0 qualification](https://github.com/MimoAlexer/ratio/blob/v0.4.0/docs/desktop-verification.md) · [Published 0.4.0 Windows verification](https://github.com/MimoAlexer/ratio/actions/runs/44833428600) · [Native SVG save result](https://github.com/MimoAlexer/ratio/actions/runs/44833428600/job/136623836317)
