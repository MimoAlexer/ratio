# Ratio 0.3.0 desktop qualification

The connection screen and workspace put **Desktop controls** behind a single button. **F10** opens the same native menu from the keyboard. The updated workspace comes from the 0.3.0 server.

| Installer | Bytes | SHA-256 |
| --- | ---: | --- |
| `Ratio-0.3.0-windows-x64-setup.exe` | 1,343,608 | `ac1d225bc8e19d424d90ee335aeb648adc37090ea349c712b070a6f9cfa60280` |
| `ratio_0.3.0_amd64.deb` | 660,384 | `4b534a35e8e8556be54e0ba7d8f0bce897d9912ba2a1ed54473b96d005cd55ed` |

## Linux

The Ubuntu 22.04 build passed all 21 desktop tests. The package requires GTK 3, WebKitGTK 4.1, bubblewrap and glibc 2.34 or later. Packaging the same executable twice produced identical bytes.

On Debian 12 with X11, the connected WebKit document grew from 795 to 820 pixels high in the same window. The accessibility tree on the Linux host exposed no menu bar. Pressing physical F10 inside the document opened a native menu with Back, Forward, Reload, Connection settings and Quit. The launcher button, the desktop issue list at 1440 pixels and issue detail at 520 pixels all exposed the same five commands. Choosing Quit exited the process.

Ubuntu 22.04 (X11 and Wayland) and Debian 12 (X11) installed the published 0.2.1 package and upgraded it in place. All 14 files in the Ubuntu test profile matched immediately before and after upgrade. Subsequent launches restored the saved origin, HTTP-only cookie, local storage and IndexedDB. A separate fresh profile had none of the previous browser state.

The packaged launcher passed 24 checks across light and dark modes at three widths. Orca announced the complete recovery message when a new connection failed and when a saved server was unreachable. It also spoke the URL correction after submission and when focus moved to Connect.

An independent profile check paired the published 0.2.1 Linux client with a disposable workspace, opened its issue list, then quit. The 0.3.0 package used the same default profile and reopened the remembered server directly on the signed-in issue list. The server still had the original browser and desktop sessions.

## Windows

The x64 installer runs with the current user’s privileges. Its bundled WebView2 loader, Start menu icon and executable version `0.3.0.0` were verified from the package. The installer is unsigned and requires the Microsoft Edge WebView2 Evergreen Runtime. Packaging the same executable twice produced identical bytes.

Wine ran all 21 Windows-target desktop tests. Fresh installation, an upgrade from the published 0.2.1 installer, a repeated install and uninstall all passed. The upgrade reopened the saved origin and preserved the existing profile, including a test session database and local storage. Unattended setup returned exit code 3 when the runtime registration was absent.

The published-installer workflow downloads the release asset, verifies its SHA-256 and runs fresh, legacy and 0.2.1 upgrade scenarios on separate Windows workers. Each run checks the Connect button in WebView2, confirms that the window has no attached menu bar, opens the popup with F10 and selects Quit. [Native Windows release checks](https://github.com/MimoAlexer/ratio/actions/workflows/verify-published-desktop.yml)

[0.2.1 installer qualification](https://github.com/MimoAlexer/ratio/blob/3a5e7366d32b10d49e5480ce65c608dc825f7f81/docs/desktop-verification.md)

## Hosted release results

GitHub verified the published v0.3.0 Windows installer against the release manifest on three separate Windows Server 2025 machines. All three opened the rendered Connect form, opened the desktop controls with F10, selected Quit and completed uninstallation. The upgrade tests retained the existing profile. [Fresh install](https://github.com/MimoAlexer/ratio/actions/runs/44822320397/job/136586051864) · [Upgrade from Working Set 0.1.0](https://github.com/MimoAlexer/ratio/actions/runs/44822320397/job/136586051065) · [Upgrade from Ratio 0.2.1](https://github.com/MimoAlexer/ratio/actions/runs/44822320397/job/136586051460)

The tagged source passed native builds, fresh installation and upgrades from 0.1.0, 0.2.0 and 0.2.1 on both Ubuntu 22.04 and Windows. Backend qualification passed PostgreSQL migrations and tests, encrypted backup and restore, and startup and readiness checks for the non-root container. [Ubuntu installer smoke](https://github.com/MimoAlexer/ratio/actions/runs/44822288591/job/136585938897) · [Desktop jobs](https://github.com/MimoAlexer/ratio/actions/runs/44822288591) · [Backend and infrastructure](https://github.com/MimoAlexer/ratio/actions/runs/44822288575)
