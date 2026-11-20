# Ratio 0.3.1 desktop qualification

The dark connection screen uses a restrained neutral gradient that peaks at the lower center. Once sign-in completes, the 0.3.1 server opens Home. The server also chooses the landing page when a paired client reopens its workspace.

| Installer | Bytes | SHA-256 |
| --- | ---: | --- |
| `Ratio-0.3.1-windows-x64-setup.exe` | 1,344,281 | `5d7107e216be0b209311053a04b814075cf5cc0f81ce4d66b6290b8e32ecdf2a` |
| `ratio_0.3.1_amd64.deb` | 660,316 | `86c8f5b4372f4e208c4c9605f5844f5701349134948d0b963c8274a82f6b3975` |

## Linux

The Ubuntu 22.04 build passed all 21 desktop tests. The package uses GTK 3, WebKitGTK 4.1 and bubblewrap. Its newest imported glibc symbol is 2.34. Two packaging runs produced identical DEBs.

APT upgrades from the published 0.3.0 package passed on Ubuntu 22.04 on X11 and Wayland, and on Debian 12 on X11. Every file in each test profile matched immediately before and after installation. The app reopened its saved origin and restored its HTTP-only cookie, local storage and IndexedDB. F10 and the pointer both opened the five native desktop commands. Escape returned to the existing draft.

Against the 0.3.1 server, a fresh desktop pairing opened Home at `/` after browser approval. A second test paired the published 0.3.0 client and upgraded it in place. The new client reopened Home with the existing session and all 43 files in the original profile intact.

The packaged launcher was compared with 0.3.0 at 1440×900, 1280×800 and 520×844. In each dark screenshot, the top center measured `#111111`, the bottom center `#262626`, and the bottom side `#1b1b1b`. The previous version measured `#0a0a0a`, `#484848` and `#484848` at those positions. The light screenshots and form geometry matched the previous release. Across 24 checks, URL and button focus, invalid-address recovery and offline Retry all behaved as expected.

## Windows

The x64 setup installs per user and requires the Microsoft Edge WebView2 Evergreen Runtime. The extracted application and installer report version `0.3.1.0`. Both include the expected ten icon images, and the package includes the x64 WebView2 loader. The installer is unsigned. Two independent compilations and packaging runs produced identical executables and installers.

All 21 Windows-target tests passed in Wine. An in-place upgrade from the published 0.3.0 installer retained the saved workspace, browser database, local storage, export and user-created file. Reinstallation and uninstall preserved those files. A clean installation registered the per-user shortcut, and unattended setup returned exit code 3 when WebView2 was absent.

The published-installer workflow checks the release checksum and runs fresh, Working Set 0.1.0 and Ratio 0.3.0 upgrade scenarios on separate Windows workers. Each check opens the WebView2 Connect form, tests the F10 popup, selects Quit and uninstalls. [Native Windows release checks](https://github.com/MimoAlexer/ratio/actions/workflows/verify-published-desktop.yml)

[0.3.0 installer qualification and hosted results](https://github.com/MimoAlexer/ratio/blob/cb6f3b2bff4e5a55a96c3881f17723483bd276eb/docs/desktop-verification.md)
