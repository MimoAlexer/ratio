# Ratio 0.2.1 desktop qualification

The 0.2.1 desktop launcher uses a recessed URL field, a raised Connect button and neutral keyboard-focus rings. Connected workspace screens come from the Ratio server. Install the 0.2.1 server to receive those visual updates on an existing client. [Release and checksums](https://github.com/MimoAlexer/ratio/releases/tag/v0.2.1)

| Installer | Bytes | SHA-256 |
| --- | ---: | --- |
| `Ratio-0.2.1-windows-x64-setup.exe` | 1,343,188 | `e6d2969993ca3063b53a84a2527526e621003040af29cdf657d0955c0cd2d0c8` |
| `ratio_0.2.1_amd64.deb` | 659,760 | `a0464b3a7c11ded6049bef304ea6dcd51ffbba845d4e2895993d2f2808e140c2` |

## Connection and profiles

First launch focuses Workspace URL. When a saved server is unreachable, Retry connection receives focus. A rejected address stays editable. Both appearance modes use equal red, green and blue channel values for the launcher surfaces, text, borders and focus ring. Input boundaries measure 3.68∶1 in dark mode and 3.65∶1 in light mode. Focus rings measure 11.37∶1 and 7.26∶1 against the canvas.

The packaged Linux launcher was checked in light and dark modes at 1440, 1280 and 520 pixels wide. At each size, the verification covered initial URL focus, Connect button focus, invalid-address recovery and the offline Retry state.

On the packaged client, Orca 50.3 spoke the HTTPS correction after a pasted remote HTTP address was submitted and repeated it on Tab to Connect. Editing cleared the invalid state while the field kept focus. On connection refusal, Orca read Retry connection and the complete recovery message.

A separate end-to-end check paired the published 0.2.0 Linux client with the read-only Luma sample, quit it, and launched the 0.2.1 package with the same profile. It reopened the remembered server and the authenticated issue list without a second approval. A clean 0.2.1 installation recovered from an unreachable server, connected to the live service, and forgot the saved address on request.

The published Windows installer was also downloaded by GitHub, verified against the release checksum, and exercised on three native Windows Server 2025 workers. Fresh installation, upgrade from 0.1.0 and upgrade from 0.2.0 each passed. [Published installer verification](https://github.com/MimoAlexer/ratio/actions/runs/44815758573)

## Platform checks

| Environment | Qualification |
| --- | --- |
| Ubuntu 22.04 and Debian 12 | APT clean installation, direct upgrades from Ratio 0.2.0, Ratio 0.1.1 and Working Set 0.1.0, and purge in isolated distribution userspaces on a shared Linux kernel. Checks covered the installed executable, desktop entry, icon and original saved profile data. |
| Windows x64 installer under Wine | Clean install, direct upgrades from all three published predecessors, and uninstall. Tests checked the installed PE, Start menu shortcut, registration, user profile and unrelated files. Without WebView2 the silent installer returned status 3. |
| Linux and Windows release executables | All 20 desktop tests passed on both targets. Repackaging each frozen executable produced identical installer bytes. Explorer metadata reports 0.2.1.0 and all ten embedded icon images matched the application artwork. |

The Linux package requires GTK 3, WebKitGTK 4.1, bubblewrap and glibc 2.34 or newer. The Windows installer is unsigned and requires Microsoft Edge WebView2 Evergreen. The published-installer workflow launches the downloaded package on Windows and locates the Connect button through Windows UI Automation. [Native Windows release checks](https://github.com/MimoAlexer/ratio/actions/workflows/verify-published-desktop.yml)

Release 0.2.0 and its Linux, Wine and native Windows results are archived. [0.2.0 installers](https://github.com/MimoAlexer/ratio/releases/tag/v0.2.0) · [0.2.0 qualification](https://github.com/MimoAlexer/ratio/blob/a455ef1c01104ca93fa97aff6e34a1177d8a07c6/docs/desktop-verification.md)
