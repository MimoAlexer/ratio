"""Package an existing x86-64 Windows release with NSIS 3."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile


HERE = Path(__file__).resolve().parent
DESKTOP = HERE.parent.parent
# These known redistributables are resolved only if named by a PE import.
RUNTIME_NAMES = {
    name.lower(): name for name in
    ("WebView2Loader.dll", "libgcc_s_seh-1.dll", "libstdc++-6.dll", "libwinpthread-1.dll")
}


def imported_dlls(binary, objdump):
    report = subprocess.check_output([objdump, "-p", str(binary)], text=True)
    if not re.search(r"file format (?:pei|coff)-x86-64\b", report, re.IGNORECASE):
        raise SystemExit(f"Expected an x86-64 Windows PE executable or DLL: {binary}")
    imports = re.findall(r"^\s*DLL Name:\s*(\S+)", report, re.MULTILINE | re.IGNORECASE)
    if not imports:
        raise SystemExit(f"Could not inspect imported libraries: {binary}")
    return [name.lower() for name in imports]


def collect_payload(binary, dll_dirs, objdump):
    files = {"ratio.exe": binary}
    pending = [binary]
    while pending:
        current = pending.pop()
        for imported in imported_dlls(current, objdump):
            name = RUNTIME_NAMES.get(imported)
            if not name or name in files:
                continue
            candidates = [directory / name for directory in dll_dirs]
            if name == "WebView2Loader.dll":
                candidates.extend(sorted(binary.parent.glob("build/webview2-com-sys-*/out/x64/" + name)))
            source = next((path for path in candidates if path.is_file()), None)
            if not source:
                raise SystemExit(f"{current.name} imports {name}. Pass its directory with --dll-dir.")
            files[name] = source
            pending.append(source)
    return files


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--dll-dir", type=Path, action="append", default=[])
    parser.add_argument("--makensis", default="makensis")
    parser.add_argument("--objdump", default=shutil.which("llvm-objdump") or "objdump")
    parser.add_argument("--stub", choices=["x86-unicode", "amd64-unicode"],
                        help="NSIS stub supplied by the compiler; the app always targets x86-64")
    args = parser.parse_args()

    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"], cwd=DESKTOP))
    package = next(package for package in metadata["packages"]
                   if Path(package["manifest_path"]).resolve() == DESKTOP / "Cargo.toml")
    version = package["version"]
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        raise SystemExit("The NSIS version resource requires numeric major.minor.patch.")
    binary = args.binary.resolve(strict=True)
    sources = collect_payload(binary, [binary.parent, *args.dll_dir], args.objdump)
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    installer = output / f"Ratio-{version}-windows-x64-setup.exe"
    makensis = shutil.which(args.makensis)
    if not makensis:
        raise SystemExit("Install NSIS 3 or pass --makensis /path/to/makensis.")
    makensis = Path(makensis).resolve()
    prefix = "/" if os.name == "nt" else "-"
    with tempfile.TemporaryDirectory(dir=output) as stage_name:
        stage = Path(stage_name)
        shutil.copyfile(HERE / "setup.nsi", stage / "setup.nsi")
        shutil.copyfile(DESKTOP / "assets/icon.ico", stage / "icon.ico")
        for name, source in sources.items():
            shutil.copyfile(source, stage / name)
        install = [f'  File "{name}"' for name in sources]
        # A user can upgrade between dynamically linked GNU builds and
        # statically linked MSVC builds. Retire only DLLs owned by our installer.
        for name in RUNTIME_NAMES.values():
            if name in sources:
                install.append(f'  WriteRegDWORD HKCU "${{UNINSTALL_KEY}}" "Owns-{name}" 1')
            else:
                install.append(f'  !insertmacro RetireDll "{name}"')
        remove = [f'  Delete "$INSTDIR\\{name}"' for name in sources if name != "ratio.exe"]
        legacy = [f'  !insertmacro RetireLegacyDll "{name}" {int(name in sources)}'
                  for name in RUNTIME_NAMES.values()]
        (stage / "payload.nsh").write_text("\n".join([
            "!macro InstallPayload", *install, "!macroend",
            "!macro RemovePayload", *remove, "!macroend",
            "!macro RemoveLegacyDlls", *legacy, "!macroend", "",
        ]), encoding="utf-8")
        command = [str(makensis), f"{prefix}V2", f"{prefix}DVERSION={version}"]
        if args.stub:
            command.append(f"{prefix}DTARGET_STUB={args.stub}")
        packaging_env = os.environ.copy()
        packaging_env.setdefault("SOURCE_DATE_EPOCH", "946684800")
        subprocess.run([*command, "setup.nsi"], cwd=stage, env=packaging_env, check=True)
        (stage / "installer.exe").replace(installer)
    print(f"{hashlib.sha256(installer.read_bytes()).hexdigest()}  {installer.name}")


if __name__ == "__main__":
    main()
