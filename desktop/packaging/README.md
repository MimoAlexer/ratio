# Building the desktop installers

Run these commands from the `desktop` directory. Cargo uses the repository's pinned Rust toolchain and the desktop crate's lockfile. Packaging takes a prebuilt release and writes to the chosen output directory.

## Linux x86-64

The release baseline is Ubuntu 22.04 with the WebKitGTK 4.1 development package from the Ubuntu repositories. Build and package in the same environment so `dpkg-shlibdeps` can read the exact libraries and symbols the binary uses.

```sh
sudo apt-get update
sudo apt-get install build-essential pkg-config libgtk-3-dev libwebkit2gtk-4.1-dev libdbus-1-dev dpkg-dev desktop-file-utils python3
cargo test --locked
cargo build --release --locked
packaging/linux/build.sh target/release/ratio dist
dpkg-deb -I dist/ratio_*_amd64.deb
```

Install the resulting file with `sudo apt install ./dist/ratio_*_amd64.deb`. APT resolves the WebKitGTK, GTK 3 and bubblewrap runtime dependencies. The package installs the executable in `/usr/bin` and the Ratio launcher in the applications menu.

## Windows x64

Use a Visual Studio C++ build environment with the Windows SDK, the Rust MSVC toolchain, Python 3, NSIS 3 and LLVM. Make `makensis.exe` and `llvm-objdump.exe` available on PATH or pass absolute paths with `--makensis` and `--objdump`. The native CI job downloads the upstream NSIS 3.13 archive from Tauri’s GitHub mirror, with a direct SourceForge mirror as a fallback. Both copies must match the same pinned SHA-256 before extraction. [Archive provenance and digest](https://github.com/tauri-apps/binary-releases/releases/tag/nsis-3.13)

```powershell
$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = '-C target-feature=+crt-static'
cargo test --locked --target x86_64-pc-windows-msvc
cargo build --release --locked --target x86_64-pc-windows-msvc
python packaging/windows/build.py target/x86_64-pc-windows-msvc/release/ratio.exe dist
```

For a cross-build from Linux, install the Rust `x86_64-pc-windows-gnu` target and the x86-64 MinGW-w64 GCC, C++ compiler, archiver, resource compiler and binutils. The resource compiler embeds the Ratio icon and version in the Windows executable. Ensure the prefixed tools are on PATH. The release command suppresses the MinGW linker’s wall-clock timestamp so repeated links can produce identical PE files.

```sh
rustup target add x86_64-pc-windows-gnu
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
export RC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-windres
cargo rustc --release --locked --target x86_64-pc-windows-gnu --bin ratio -- -C link-arg=-Wl,--no-insert-timestamp
python3 packaging/windows/build.py target/x86_64-pc-windows-gnu/release/ratio.exe dist
```

The packager inspects PE imports and includes the x64 WebView2 loader when the executable uses it. If additional imported MinGW runtime DLLs live outside the release directory, pass their directory using `--dll-dir`. Debian's NSIS package also provides a native x64 installer stub, selectable with `--stub amd64-unicode`.

The installer runs per user. If Evergreen WebView2 is missing, interactive setup offers Microsoft's download page. Unattended setup exits with code 3. The app's Start Menu shortcut and Windows uninstall registration point to the per-user install. Uninstall preserves the user's profile and saved data. [Microsoft WebView2 distribution](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution)

Ratio 0.4.0 upgrades Ratio 0.3.1, 0.3.0, 0.2.1, 0.2.0 and 0.1.1 in place. Users of Working Set 0.1.0 can also upgrade directly. APT handles the package name transition. On Windows, close the previous app before installing. For Working Set, the installer uses the previous per-user uninstall registration to identify its application files. These upgrades preserve existing workspace settings and browser profiles.
