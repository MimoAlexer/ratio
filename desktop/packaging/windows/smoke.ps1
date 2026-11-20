# Run on a disposable Windows CI worker with an interactive desktop.
param(
    [Parameter(Mandatory)][string]$Installer,
    [string]$LegacyInstaller,
    [string]$PreviousRatioInstaller
)
$ErrorActionPreference = 'Stop'
if ($LegacyInstaller -and $PreviousRatioInstaller) { throw 'Choose one previous installer.' }
$setup = (Resolve-Path $Installer).Path
if ((Split-Path $setup -Leaf) -notmatch '^Ratio-([0-9]+\.[0-9]+\.[0-9]+)-windows-x64-setup\.exe$') {
    throw 'Expected the versioned Ratio installer.'
}
$version = $Matches[1]
$programs = [Environment]::GetFolderPath('Programs')
$uninstallRoot = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall'
$installDir = Join-Path $env:LOCALAPPDATA 'Programs\Ratio'
$appExe = Join-Path $installDir 'ratio.exe'
$shortcut = Join-Path $programs 'Ratio.lnk'
$key = Join-Path $uninstallRoot 'Ratio'
$ratioProfile = Join-Path $env:LOCALAPPDATA 'Ratio'
$legacyDir = Join-Path $env:LOCALAPPDATA 'Programs\Working Set'
$legacyProfile = Join-Path $env:LOCALAPPDATA 'Working Set'
$legacyShortcut = Join-Path $programs 'Working Set.lnk'
$legacyKey = Join-Path $uninstallRoot 'WorkingSet'

function Install-Application([string]$setup) {
    $result = Start-Process $setup -ArgumentList '/S' -Wait -PassThru
    if ($result.ExitCode -eq 3) {
        # Only this disposable worker provisions a missing prerequisite. Verify the
        # Microsoft signature before running their official Evergreen bootstrapper.
        $bootstrap = Join-Path $env:RUNNER_TEMP 'MicrosoftEdgeWebview2Setup.exe'
        Invoke-WebRequest -UseBasicParsing 'https://go.microsoft.com/fwlink/p/?LinkId=2124703' -OutFile $bootstrap
        $signature = Get-AuthenticodeSignature $bootstrap
        if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch '(^|,\s*)O=Microsoft Corporation(,|$)') {
            throw 'Microsoft WebView2 bootstrapper signature verification failed.'
        }
        $runtime = Start-Process $bootstrap -ArgumentList '/silent','/install' -Wait -PassThru
        if ($runtime.ExitCode -ne 0) { throw "WebView2 installer exited with $($runtime.ExitCode)" }
        $result = Start-Process $setup -ArgumentList '/S' -Wait -PassThru
    }
    if ($result.ExitCode -ne 0) { throw "Installer $setup exited with $($result.ExitCode)" }
}

function Initialize-WindowInspection {
    if (!('RatioWindowSmoke' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Collections.Concurrent;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;

public static class RatioWindowSmoke {
    public sealed class WindowInfo {
        public IntPtr Handle, Owner;
        public uint ProcessId;
        public string Title, ClassName;
        public override string ToString() {
            return String.Format("PID={0} HWND=0x{1:X} owner=0x{2:X} class={3} title='{4}'",
                ProcessId, Handle.ToInt64(), Owner.ToInt64(), ClassName, Title);
        }
    }
    [DllImport("user32.dll")]
    public static extern IntPtr GetMenu(IntPtr window);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern IntPtr FindWindow(string className, string title);

    [DllImport("user32.dll")]
    private static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);

    private delegate bool EnumWindowProc(IntPtr window, IntPtr parameter);
    [DllImport("user32.dll")]
    private static extern bool EnumWindows(EnumWindowProc callback, IntPtr parameter);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetWindowText(IntPtr window, StringBuilder text, int size);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetClassName(IntPtr window, StringBuilder text, int size);
    [DllImport("user32.dll")]
    private static extern IntPtr GetWindow(IntPtr window, uint command);
    [DllImport("user32.dll")]
    private static extern bool IsWindowVisible(IntPtr window);

    public static WindowInfo[] Windows(int processId, bool visibleOnly) {
        var windows = new List<WindowInfo>();
        EnumWindows(delegate(IntPtr window, IntPtr parameter) {
            uint pid;
            GetWindowThreadProcessId(window, out pid);
            if ((processId == 0 || pid == (uint)processId) && (!visibleOnly || IsWindowVisible(window))) {
                var title = new StringBuilder(512);
                var name = new StringBuilder(128);
                GetWindowText(window, title, title.Capacity);
                GetClassName(window, name, name.Capacity);
                windows.Add(new WindowInfo { Handle = window, Owner = GetWindow(window, 4),
                    ProcessId = pid, ClassName = name.ToString(), Title = title.ToString() });
            }
            return true;
        }, IntPtr.Zero);
        return windows.ToArray();
    }

    public static string[] DescribeWindows(int processId) {
        return Array.ConvertAll(Windows(processId, false), w => w.ToString());
    }

    public static bool BelongsTo(IntPtr window, int processId) {
        while (window != IntPtr.Zero) {
            uint pid;
            GetWindowThreadProcessId(window, out pid);
            if (pid == (uint)processId) return true;
            window = GetWindow(window, 4); // GW_OWNER
        }
        return false;
    }

    public static bool HasPopup(int processId) {
        IntPtr popup = FindWindow("#32768", null);
        uint owner;
        return popup != IntPtr.Zero && GetWindowThreadProcessId(popup, out owner) != 0
            && owner == (uint)processId;
    }
}

// TempDir may disappear before UI Automation has observed a Save dialog.
public sealed class RatioStageWatch : IDisposable {
    private readonly FileSystemWatcher watcher;
    private readonly string root;
    private readonly ConcurrentDictionary<string, bool> seen = new ConcurrentDictionary<string, bool>();
    public RatioStageWatch(string tempDir) {
        root = Path.GetFullPath(tempDir).TrimEnd(Path.DirectorySeparatorChar) + Path.DirectorySeparatorChar;
        watcher = new FileSystemWatcher(root);
        watcher.IncludeSubdirectories = true;
        watcher.NotifyFilter = NotifyFilters.DirectoryName | NotifyFilters.FileName | NotifyFilters.Size;
        watcher.Created += Observe;
        watcher.Changed += Observe;
        watcher.Deleted += Observe;
        watcher.Renamed += Observe;
        watcher.Error += delegate { seen.TryAdd("Watcher buffer/error; stage history may be incomplete.", true); };
        watcher.EnableRaisingEvents = true;
    }
    private void Observe(object sender, FileSystemEventArgs e) {
        if (!e.FullPath.StartsWith(root + "ratio-export-", StringComparison.OrdinalIgnoreCase)) return;
        string relative = e.FullPath.Substring(root.Length);
        string size = "";
        try { if (File.Exists(e.FullPath)) size = " bytes=" + new FileInfo(e.FullPath).Length; }
        catch (IOException) { }
        catch (UnauthorizedAccessException) { }
        seen.TryAdd(e.ChangeType + " " + relative + size, true);
    }
    public string[] Observations() { return new List<string>(seen.Keys).ToArray(); }
    public void Dispose() { watcher.Dispose(); }
}
'@
    }
}

function Write-WindowTimeout($app) {
    $windows = @([RatioWindowSmoke]::DescribeWindows($app.Id))
    if ($windows.Count) { $windows | Write-Host } else { Write-Host 'No top-level windows belong to this process.' }
    $clientKey = 'Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
    foreach ($path in @("HKLM:\SOFTWARE\WOW6432Node\$clientKey", "HKCU:\SOFTWARE\$clientKey")) {
        $pv = Get-ItemPropertyValue -LiteralPath $path -Name pv -ErrorAction SilentlyContinue
        Write-Host "WebView2 Evergreen [$path] pv='$pv'"
    }
    # A browser that started may be using a different build from the latest registration.
    $children = Get-CimInstance Win32_Process -Filter "ParentProcessId=$($app.Id) AND Name='msedgewebview2.exe'" -OperationTimeoutSec 5 -ErrorAction SilentlyContinue
    foreach ($child in $children) {
        if ($child.ExecutablePath) {
            $file = Get-Item -LiteralPath $child.ExecutablePath -ErrorAction SilentlyContinue
            Write-Host "Running WebView2 PID=$($child.ProcessId) version='$($file.VersionInfo.ProductVersion)'"
        }
    }
}

function Assert-DesktopControls($app, $connectButton) {
    if ([RatioWindowSmoke]::GetMenu($app.MainWindowHandle) -ne [IntPtr]::Zero) {
        throw 'The Ratio window still has an attached Win32 menu bar.'
    }
    $shell = New-Object -ComObject WScript.Shell
    if (!$shell.AppActivate($app.Id)) { throw 'Could not activate the Ratio window for keyboard input.' }
    $connectButton.SetFocus()
    $shell.SendKeys('{F10}')
    $end = (Get-Date).AddSeconds(10)
    while (![RatioWindowSmoke]::HasPopup($app.Id) -and (Get-Date) -lt $end) {
        Start-Sleep -Milliseconds 100
    }
    if (![RatioWindowSmoke]::HasPopup($app.Id)) {
        throw 'F10 did not open the native controls popup while WebView2 had focus.'
    }
    $shell.SendKeys('q')
    if (!$app.WaitForExit(10000)) {
        throw "Selecting Quit left Ratio running after 10 seconds (PID $($app.Id))."
    }
    $exitCode = $app.ExitCode
    if ($null -eq $exitCode) {
        throw "Ratio quit, but PowerShell could not read the exit code (PID $($app.Id))."
    }
    if ($exitCode -ne 0) {
        throw "Selecting Quit exited Ratio with code $exitCode (PID $($app.Id))."
    }
    Write-Host 'Verified a menu-free window, WebView2 F10 and the native Quit action.'
}

function Assert-ConnectionWindow([string]$phase) {
    Initialize-WindowInspection
    $installedVersion = (Get-Item -LiteralPath $appExe).VersionInfo.ProductVersion
    $app = Start-Process $appExe -PassThru
    try {
        # Windows PowerShell needs the process handle open before Quit to report its exit code.
        $null = $app.Handle
        $context = "$phase, Ratio $installedVersion, PID $($app.Id)"
        Write-Host "Opening connection window ($context)."
        $end = (Get-Date).AddSeconds(60)
        do {
            Start-Sleep -Milliseconds 400
            $app.Refresh()
            if ($app.HasExited) { throw "Application exited before opening a window ($context, exit $($app.ExitCode))." }
        } until ($app.MainWindowTitle -like 'Ratio*Connection' -or (Get-Date) -gt $end)
        if ($app.MainWindowTitle -notlike 'Ratio*Connection') {
            $message = "Connection window timed out ({0}). Main HWND=0x{1:X} title='{2}'." -f $context, $app.MainWindowHandle.ToInt64(), $app.MainWindowTitle
            try { Write-WindowTimeout $app } catch { Write-Warning "Window diagnostics failed: $_" }
            throw $message
        }
        # UI Automation reaches into WebView2. This verifies that the server-choice
        # form rendered, even when the frame was created successfully.
        Add-Type -AssemblyName UIAutomationClient
        Add-Type -AssemblyName UIAutomationTypes
        $window = [System.Windows.Automation.AutomationElement]::FromHandle($app.MainWindowHandle)
        $condition = [System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::NameProperty, 'Connect')
        $end = (Get-Date).AddSeconds(45)
        do {
            $button = $window.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $condition)
            if (!$button) { Start-Sleep -Milliseconds 400 }
        } until ($button -or (Get-Date) -gt $end)
        if (!$button -or $button.Current.ControlType -ne [System.Windows.Automation.ControlType]::Button) {
            throw 'The connection form did not expose its Connect button to Windows UI Automation.'
        }
        if ([version](Get-ItemProperty -LiteralPath $key).DisplayVersion -ge [version]'0.3.0') {
            Assert-DesktopControls $app $button
        }
    } finally {
        if (!$app.HasExited) {
            $null = $app.CloseMainWindow()
            if (!$app.WaitForExit(5000)) { $app.Kill(); $app.WaitForExit() }
        }
        $app.Dispose()
    }
}

function Write-SvgTimeout($app, $destination, $stageWatcher) {
    $visible = @([RatioWindowSmoke]::Windows(0, $true))
    Write-Host "SVG save timeout for Ratio PID=$($app.Id). Visible windows ($($visible.Count)):"
    $owners = @($visible | Where-Object ProcessId -eq $app.Id | ForEach-Object { $_.Handle })
    foreach ($item in $visible) {
        Write-Host $item.ToString()
        $related = $item.ProcessId -eq $app.Id -or $owners -contains $item.Owner
        if ($item.Title -notmatch 'save|export|error|could not' -and !($related -and $item.ClassName -eq '#32770')) { continue }
        try {
            $dialog = [System.Windows.Automation.AutomationElement]::FromHandle([IntPtr]$item.Handle)
            $controls = $dialog.FindAll([System.Windows.Automation.TreeScope]::Descendants,
                [System.Windows.Automation.Condition]::TrueCondition)
            for ($i = 0; $i -lt [Math]::Min(40, $controls.Count); $i++) {
                $control = $controls[$i].Current
                Write-Host ("  {0} ID='{1}' name='{2}'" -f $control.ControlType.ProgrammaticName, $control.AutomationId, $control.Name)
            }
        } catch {
            Write-Host "  UI Automation could not inspect this window: $($_.Exception.Message)"
        }
    }
    try {
        $directory = Split-Path -Parent $destination
        $stem = [IO.Path]::GetFileNameWithoutExtension($destination)
        $files = @(Get-ChildItem -LiteralPath $directory -File -Filter "$stem*" -Force -ErrorAction Stop)
        Write-Host "Downloads matches for '$destination': $($files.Count)"
        foreach ($file in $files) {
            Write-Host "  $($file.FullName) ($($file.Length) bytes)"
            if ($file.Extension -eq '.svg') {
                $hash = Get-FileHash -LiteralPath $file.FullName -ErrorAction Stop
                Write-Host "  SHA-256 $($hash.Hash)"
            }
        }
    } catch { Write-Host "Could not inspect matching downloads: $($_.Exception.Message)" }
    if ($null -ne $stageWatcher) {
        try {
            $rows = @($stageWatcher.Observations())
            Write-Host "Staging observations: $($rows.Count)"
            foreach ($row in $rows) { Write-Host "  $row" }
        } catch { Write-Host "Could not read staging observations: $($_.Exception.Message)" }
    }
}

function Assert-CanvasSvgSave {
    # Exercise the installed WebView2 download callback and IFileSaveDialog with a
    # same-origin attachment. Keep the release worker's regular profile unchanged.
    $id = [Guid]::NewGuid().ToString('D')
    $dir = Join-Path ([IO.Path]::GetTempPath()) "ratio-svg-$id"
    $null = New-Item -ItemType Directory -Path $dir
    $name = "canvas-$id.svg"
    $downloads = (New-Object -ComObject Shell.Application).NameSpace('shell:Downloads').Self.Path
    $destination = Join-Path $downloads $name
    if (Test-Path -LiteralPath $destination) { throw "Unexpected pre-existing SVG fixture: $destination" }
    $svg = '<svg xmlns="http://www.w3.org/2000/svg"><title>Ratio canvas save test</title></svg>'
    [IO.File]::WriteAllText((Join-Path $dir $name), $svg, [Text.Encoding]::ASCII)
    $server = $null
    $app = $null
    $stageWatcher = $null
    try {
        @'
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import sys

root = Path(__file__).resolve().parent
name = "canvas-" + sys.argv[1] + ".svg"
route = "/canvas/" + sys.argv[1] + "/export.svg"

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/desktop/start":
            mime = "text/html; charset=utf-8"
            body = ('<!doctype html><a download="" href="' + route + '">Download test SVG</a>').encode()
        elif self.path == route:
            mime = "image/svg+xml; charset=utf-8"
            body = (root / name).read_bytes()
        else:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Type", mime)
        if self.path == route:
            self.send_header("Content-Disposition", 'attachment; filename="' + name + '"')
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        if self.path == route:
            (root / "attachment-sent").write_text(name, encoding="ascii")
            print("Sent canonical GET attachment " + name + " (" + str(len(body)) + " bytes).", flush=True)

with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
    port_file = root / "port.tmp"
    port_file.write_text(str(server.server_port), encoding="ascii")
    port_file.replace(root / "port")
    server.serve_forever()
'@ | Set-Content -LiteralPath (Join-Path $dir 'serve.py') -Encoding Ascii
        $server = Start-Process python -WorkingDirectory $dir -ArgumentList 'serve.py',$id -NoNewWindow -PassThru
        $portFile = Join-Path $dir 'port'
        $end = (Get-Date).AddSeconds(15)
        while (!(Test-Path -LiteralPath $portFile) -and !$server.HasExited -and (Get-Date) -lt $end) { Start-Sleep -Milliseconds 100 }
        if (!(Test-Path -LiteralPath $portFile)) { throw 'Local SVG fixture did not start.' }
        $port = [int](Get-Content -Raw -LiteralPath $portFile)
        $profile = Join-Path $dir 'profile'
        $app = Start-Process $appExe -ArgumentList '--server',"http://127.0.0.1:$port",'--profile-dir',"`"$profile`"" -PassThru
        Add-Type -AssemblyName UIAutomationClient
        Add-Type -AssemblyName UIAutomationTypes
        $linkName = [System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::NameProperty, 'Download test SVG')
        $end = (Get-Date).AddSeconds(75)
        $link = $null
        while (!$link -and (Get-Date) -lt $end) {
            $app.Refresh()
            if ($app.HasExited) { throw "Ratio exited before exposing the download link ($($app.ExitCode))." }
            if ($app.MainWindowHandle -ne [IntPtr]::Zero) {
                $window = [System.Windows.Automation.AutomationElement]::FromHandle($app.MainWindowHandle)
                $link = $window.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $linkName)
            }
            if (!$link) { Start-Sleep -Milliseconds 250 }
        }
        if (!$link) { throw 'WebView2 did not expose the SVG download link.' }
        $shell = New-Object -ComObject WScript.Shell
        if (!$shell.AppActivate($app.Id)) { throw 'Could not activate the SVG fixture window.' }
        $stageWatcher = [RatioStageWatch]::new([IO.Path]::GetTempPath())
        $link.SetFocus()
        $shell.SendKeys('{ENTER}')
        $editCondition = [System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::ControlTypeProperty, [System.Windows.Automation.ControlType]::Edit)
        $end = (Get-Date).AddSeconds(30)
        $save = $null
        $buttonCondition = [System.Windows.Automation.AndCondition]::new(
            [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::ControlTypeProperty, [System.Windows.Automation.ControlType]::Button),
            [System.Windows.Automation.OrCondition]::new(
                [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::AutomationIdProperty, '1'),
                [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::NameProperty, 'Save')))
        do {
            foreach ($window in [RatioWindowSmoke]::Windows(0, $true)) {
                if ($window.Title -ne 'Save canvas SVG' -or ![RatioWindowSmoke]::BelongsTo($window.Handle, $app.Id)) { continue }
                try {
                    $dialog = [System.Windows.Automation.AutomationElement]::FromHandle($window.Handle)
                    if ($window.ProcessId -ne $app.Id) {
                        # A brokered dialog must belong to Ratio and advertise this test's unique filename.
                        $matchingName = $false
                        foreach ($edit in $dialog.FindAll([System.Windows.Automation.TreeScope]::Descendants, $editCondition)) {
                            $value = $null
                            if ($edit.TryGetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern, [ref]$value) -and
                                $value.Current.Value -eq $name) { $matchingName = $true; break }
                        }
                        if (!$matchingName) { continue }
                    }
                    $save = $dialog.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $buttonCondition)
                    if ($save) { break }
                } catch [System.Windows.Automation.ElementNotAvailableException] { continue }
            }
            if (!$save) { Start-Sleep -Milliseconds 200 }
        } until ($save -or (Get-Date) -ge $end)
        if (!$save) {
            Write-SvgTimeout $app $destination $stageWatcher
            throw 'Ratio did not display its native Save canvas SVG dialog and Save button.'
        }
        $receipt = Join-Path $dir 'attachment-sent'
        if (!(Test-Path -LiteralPath $receipt) -or [IO.File]::ReadAllText($receipt) -ne $name) {
            throw 'The fixture did not serve the canonical SVG GET attachment.'
        }
        # Send real input after UIA confirms the process-owned control. Calling
        # a UIA provider synchronously to invoke a modal control can block a test.
        $save.SetFocus()
        $shell.SendKeys(' ')
        $end = (Get-Date).AddSeconds(15)
        while (!(Test-Path -LiteralPath $destination) -and (Get-Date) -lt $end) { Start-Sleep -Milliseconds 100 }
        $expected = (Get-FileHash -LiteralPath (Join-Path $dir $name)).Hash
        if (!(Test-Path -LiteralPath $destination) -or (Get-FileHash -LiteralPath $destination).Hash -ne $expected) {
            Write-SvgTimeout $app $destination $stageWatcher
            throw 'The native Save dialog did not write the expected SVG bytes to the suggested filename.'
        }
        Write-Host "Verified native SVG dialog, suggested filename and saved bytes (SHA-256 $expected)."
    } finally {
        if ($stageWatcher) { $stageWatcher.Dispose() }
        if ($app -and !$app.HasExited) {
            $null = $app.CloseMainWindow()
            if (!$app.WaitForExit(5000)) { $app.Kill(); $app.WaitForExit() }
        }
        if ($app) { $app.Dispose() }
        if ($server -and !$server.HasExited) { $server.Kill(); $server.WaitForExit() }
        if ($server) { $server.Dispose() }
        if (Test-Path -LiteralPath $destination) { Remove-Item -LiteralPath $destination -Force }
        # Evergreen can release the test profile asynchronously after the app exits.
        Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# This guard limits the test to a clean machine and protects existing profiles.
foreach ($path in @($installDir, $legacyDir, $ratioProfile, $legacyProfile, $key, $legacyKey, $shortcut, $legacyShortcut)) {
    if (Test-Path -LiteralPath $path) { throw "Installer smoke needs a clean worker. Found $path." }
}
$scenario = if ($LegacyInstaller) { 'working-set-upgrade' } elseif ($PreviousRatioInstaller) { 'ratio-upgrade' } else { 'fresh' }
Write-Host "Testing Ratio $version ($scenario)"
$expectedProfileDir = $ratioProfile
if ($LegacyInstaller) {
    $LegacyInstaller = (Resolve-Path $LegacyInstaller).Path
    if ((Get-FileHash $LegacyInstaller).Hash -ne '71426a284bc1ddf97623f8d6b585116c812f278d87fbf967f02bc150e1ef8dcd') {
        throw 'Published Working Set 0.1.0 installer checksum mismatch.'
    }
    Install-Application $LegacyInstaller
    $oldExe = Join-Path $legacyDir 'working-set.exe'
    if (!(Test-Path $oldExe) -or !(Test-Path $legacyShortcut) -or !(Test-Path $legacyKey)) {
        throw 'The legacy installer did not establish the upgrade fixture.'
    }
    $legacyFiles = @(Get-ChildItem -LiteralPath $legacyDir -File)
    $legacyUserFile = Join-Path $legacyDir 'user-owned.txt'
    Set-Content -LiteralPath $legacyUserFile -Value 'keep this file'
    $expectedProfileDir = $legacyProfile
}
if ($PreviousRatioInstaller) {
    $previous = (Resolve-Path $PreviousRatioInstaller).Path
    $publishedVersions = @{
        'ddc157fe6e6787f21a2e93735a933a6441ec169061fc61b29a5ab12a02026057' = '0.1.1'
        '699086b5326a2156b5cd85026bed0f9f5c8f802a5e6823241b35e58de5e3a152' = '0.2.0'
        'e6d2969993ca3063b53a84a2527526e621003040af29cdf657d0955c0cd2d0c8' = '0.2.1'
        'ac1d225bc8e19d424d90ee335aeb648adc37090ea349c712b070a6f9cfa60280' = '0.3.0'
        '5d7107e216be0b209311053a04b814075cf5cc0f81ce4d66b6290b8e32ecdf2a' = '0.3.1'
    }
    $previousVersion = $publishedVersions[(Get-FileHash $previous).Hash]
    if (!$previousVersion) { throw 'Unknown previous Ratio installer checksum.' }
    if ([version]$previousVersion -ge [version]$version) { throw 'The upgrade fixture must be an older Ratio version.' }
    Install-Application $previous
    if ((Get-ItemProperty -LiteralPath $key).DisplayVersion -ne $previousVersion -or !(Test-Path $appExe)) {
        throw "The previous installer did not establish the Ratio $previousVersion upgrade fixture."
    }
    Assert-ConnectionWindow 'previous install'
}
if ($scenario -ne 'fresh') {
    New-Item -ItemType Directory -Path $expectedProfileDir -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $expectedProfileDir 'installer-ci-sentinel') -Value 'retained'
}
Install-Application $setup
$entry = Get-ItemProperty -LiteralPath $key
if (!(Test-Path $appExe) -or !(Test-Path $shortcut) -or $entry.DisplayName -ne 'Ratio' -or $entry.DisplayVersion -ne $version) {
    throw 'Missing or incorrect installed Ratio binary, Start Menu entry, or registration.'
}
$link = (New-Object -ComObject WScript.Shell).CreateShortcut($shortcut)
if ($link.TargetPath -ne $appExe) { throw 'The Start Menu shortcut points to the wrong executable.' }
$appInfo = (Get-Item $appExe).VersionInfo
$appVersion = "$($appInfo.FileMajorPart).$($appInfo.FileMinorPart).$($appInfo.FileBuildPart)"
if ($appInfo.ProductName -ne 'Ratio' -or $appVersion -ne $version) { throw 'Unexpected executable product name or version.' }
if ((Test-Path $legacyKey) -or (Test-Path $legacyShortcut)) { throw 'The legacy registration or shortcut remains.' }
if ($LegacyInstaller) {
    foreach ($old in $legacyFiles) {
        if (Test-Path -LiteralPath $old.FullName) { throw "Legacy application file remains: $($old.Name)" }
    }
    if ((Get-Content -Raw $legacyUserFile).Trim() -ne 'keep this file') { throw 'Upgrade removed an unrelated file.' }
}

Assert-ConnectionWindow "$scenario, installed target"
if ($scenario -eq 'fresh' -and [version]$version -ge [version]'0.4.0') {
    Assert-CanvasSvgSave
}
if (!(Test-Path -LiteralPath (Join-Path $expectedProfileDir 'launcher'))) { throw 'The app opened the wrong profile.' }
$sentinel = Join-Path $expectedProfileDir 'installer-ci-sentinel'
if ($scenario -eq 'fresh') { Set-Content -LiteralPath $sentinel -Value 'retained' }
if ((Get-Content -Raw $sentinel).Trim() -ne 'retained') { throw 'Install changed user data.' }

# The installed command starts a temporary NSIS process. Its wrapper's exit
# code covers the handoff; the installed files, shortcut and key cover cleanup.
$uninstaller = Join-Path $installDir 'Uninstall.exe'
$start = New-Object System.Diagnostics.ProcessStartInfo
$start.FileName = $uninstaller
$start.Arguments = '/S'
$start.UseShellExecute = $false
$wrapper = [Diagnostics.Process]::Start($start)
try {
    $wrapperId = $wrapper.Id
    $wrapper.WaitForExit()
    $wrapperExit = $wrapper.ExitCode
    Write-Host "NSIS uninstall wrapper PID=$wrapperId exit=$wrapperExit."
} finally {
    $wrapper.Dispose()
}
$end = (Get-Date).AddSeconds(20)
do {
    $remaining = @($installDir, $shortcut, $key | Where-Object { Test-Path -LiteralPath $_ })
    if (!$remaining.Count -or (Get-Date) -ge $end) { break }
    Start-Sleep -Milliseconds 200
} while ($true)
if ($wrapperExit -ne 0 -or $remaining.Count) {
    foreach ($path in @($installDir, $appExe, $uninstaller, $shortcut, $key)) {
        Write-Host "Uninstall state: exists=$(Test-Path -LiteralPath $path) path='$path'"
    }
    if (Test-Path -LiteralPath $installDir) {
        Get-ChildItem -Force -Recurse -LiteralPath $installDir -ErrorAction SilentlyContinue | ForEach-Object {
            Write-Host "Remaining entry: '$($_.FullName)' bytes=$($_.Length) attributes=$($_.Attributes)"
        }
    }
    if (Test-Path -LiteralPath $shortcut) {
        try { Write-Host "Remaining shortcut target: '$(((New-Object -ComObject WScript.Shell).CreateShortcut($shortcut)).TargetPath)'" }
        catch { Write-Warning "Could not inspect remaining shortcut: $_" }
    }
    if (Test-Path -LiteralPath $key) {
        $leftEntry = Get-ItemProperty -LiteralPath $key -ErrorAction SilentlyContinue
        Write-Host "Remaining registration: version='$($leftEntry.DisplayVersion)' location='$($leftEntry.InstallLocation)'"
    }
    $filter = "ParentProcessId=$wrapperId OR Name LIKE 'Un%.exe' OR Name='ratio.exe'"
    try {
        $related = @(Get-CimInstance Win32_Process -Filter $filter -OperationTimeoutSec 5 | Where-Object {
            $_.ParentProcessId -eq $wrapperId -or $_.ExecutablePath -eq $appExe -or
                ($_.CommandLine -and $_.CommandLine.IndexOf("_?=$installDir", [StringComparison]::OrdinalIgnoreCase) -ge 0)
        })
        Write-Host "Live Ratio or related NSIS processes: $($related.Count)"
        foreach ($process in $related) {
            Write-Host "PID=$($process.ProcessId) parent=$($process.ParentProcessId) image='$($process.ExecutablePath)' command='$($process.CommandLine)'"
        }
    } catch { Write-Warning "Could not inspect uninstall processes: $_" }
    throw "Ratio uninstall failed: wrapper exit=$wrapperExit, remaining paths=[$($remaining -join ', ')]."
}
if ((Get-Content -Raw $sentinel).Trim() -ne 'retained') { throw 'Uninstall changed user data.' }
if ($LegacyInstaller -and (Get-Content -Raw $legacyUserFile).Trim() -ne 'keep this file') {
    throw 'Uninstall removed an unrelated legacy file.'
}
