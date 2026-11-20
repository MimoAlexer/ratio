# Desktop qualification

The published v0.1.0 application binaries were built from the Working Set desktop source in commit `cb1c2fc5d78bb122b4a5076cd0ce076ecd8531b2`. Both release assets were downloaded again from the private GitHub release and verified against the published SHA-256 manifest.

| Asset | SHA-256 |
|---|---|
| Working-Set-0.1.0-windows-x64-setup.exe | `71426a284bc1ddf97623f8d6b585116c812f278d87fbf967f02bc150e1ef8dcd` |
| working-set_0.1.0_amd64.deb | `31a30dd49bdb4509d98ae2b49e0e95551a36fb3edc14b63f86f12daf98545ee1` |

## Platform checks

- **Linux:** The final package passed installation, application-menu launch, initial rendering and saved-server relaunch on Ubuntu 22.04 and Debian 12.15 virtual machines. Ubuntu 22.04 and Ubuntu 26.04 also exercised X11 and native Wayland. The actual WebKit content process ran inside a separate PID namespace with seccomp and no-new-privileges. Removing the package preserved the local workspace profile.
- **Windows:** The per-user NSIS installer was checked for x64 architecture, installed file hashes, WebView2 runtime detection, Start menu entry, icon, version metadata and data-preserving uninstall. The signed Microsoft runtime is a separate prerequisite. Windows cross-build and Wine checks are complemented by [native Windows CI](https://github.com/MimoAlexer/working-set/actions/workflows/desktop.yml) and [published-installer verification](https://github.com/MimoAlexer/working-set/actions/workflows/verify-published-desktop.yml), which looks for the rendered Connect control through Windows UI Automation.
- **Real desktop session:** On Linux the native window handed off device approval to the default browser and received its own persistent session. A new issue from the native UI was visible to the independent client, and a changed checkpoint appeared back in the native app. Two isolated accounts were tested. A full restart preserved the intended server and account. Native Save As exported a valid archive, with no overwrite of existing files.
- **Sync:** With two independent sign-ins a remote change appeared in approximately ten seconds on an idle window. When a user was typing, the window offered to load updates and retained the draft. Offline saves waited for explicit retry.
- **Application:** Rust formatting and strict Clippy passed. The complete server suite passed 246 library and nine integration tests. The optional load test ran separately with 800 successful requests in the representative issue dataset. The real UI was inspected in graphite and monochrome at narrow and desktop widths; the issue list, board, issue detail and new-issue flow were tested with ordinary forms and keyboard input.

The screenshots in [images](images) were captured from the running native application against the read-only Luma sample project. Provider OAuth, email delivery and charges were qualified with local fixtures. See [open staging issues](https://github.com/MimoAlexer/working-set/issues) for work requiring the operator's accounts.
