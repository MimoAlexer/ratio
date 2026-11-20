# Ratio 0.1.1 desktop qualification

The [private release](https://github.com/MimoAlexer/ratio/releases/tag/v0.1.1) contains these x86-64 installers. Verify downloaded files using its SHA-256 manifest.

| File | SHA-256 |
|---|---|
| Ratio-0.1.1-windows-x64-setup.exe | `ddc157fe6e6787f21a2e93735a933a6441ec169061fc61b29a5ab12a02026057` |
| ratio_0.1.1_amd64.deb | `c0bdb765e1f88570d7fce3283874278ae86dfb81e86af194e695d768ff07b761` |

## Upgrade and compatibility

- An actual v0.1.0 account reopened its saved server when Ratio launched with zero arguments. A native issue and its saved checkpoint were present, and the original session cookie was accepted. A clean profile starts in a Ratio directory. Ratio uses the older profile in place when upgrading an installation without a separate Ratio profile.
- The Debian package upgrades the previously installed package and replaces the menu entry. Ubuntu 22.04 and Debian 12 virtual machines retained the saved origin, cookies and browser storage. The installed application also passed X11/Wayland and sandbox checks. Removal or purge preserved the user's profile.
- The per-user Windows installer sets the Ratio name, icon, version and Start menu entry. A clean installation and upgrades from default and custom locations passed under Wine, including preservation of unrelated files. The private repo also runs [native Windows and Linux source CI](https://github.com/MimoAlexer/ratio/actions/workflows/desktop.yml) and a [native test against the published Windows installer](https://github.com/MimoAlexer/ratio/actions/workflows/verify-published-desktop.yml). The Windows installer is unsigned and requires the official Microsoft WebView2 runtime.
- The server was upgraded on an existing database. Historical migrations, private project IDs, original authentication cookies and pending pairing codes remained valid. A newly signed-in Ratio device and a previous-version native client both received sync updates. Native ZIP exports preserve their format and archived encrypted backups remain readable by the restore utility.
- The server suite passed all 258 tests including the opt-in read measurement. The desktop suite passed 20 tests on Linux and as Windows binaries under Wine. These local results use synthetic data and provider fixtures. Hosted workflows perform the platform-specific checks independently.

The [issue list](images/ratio-issues.png) and [issue detail](images/ratio-issue-detail.png) screenshots were captured from the released Linux package using the read-only Luma sample. For historical build evidence, see the [archived 0.1.0 release](https://github.com/MimoAlexer/ratio/releases/tag/v0.1.0).
