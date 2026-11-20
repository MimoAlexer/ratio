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
using System.Runtime.InteropServices;
using System.Text;

public static class RatioWindowSmoke {
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

    public static string[] DescribeWindows(int processId) {
        var windows = new List<string>();
        EnumWindows(delegate(IntPtr window, IntPtr parameter) {
            uint pid;
            GetWindowThreadProcessId(window, out pid);
            if (pid == (uint)processId) {
                var title = new StringBuilder(512);
                var name = new StringBuilder(128);
                GetWindowText(window, title, title.Capacity);
                GetClassName(window, name, name.Capacity);
                windows.Add(String.Format("HWND=0x{0:X} owner=0x{1:X} class={2} title='{3}'",
                    window.ToInt64(), GetWindow(window, 4).ToInt64(), name, title));
            }
            return true;
        }, IntPtr.Zero);
        return windows.ToArray();
    }

    public static bool HasPopup(int processId) {
        IntPtr popup = FindWindow("#32768", null);
        uint owner;
        return popup != IntPtr.Zero && GetWindowThreadProcessId(popup, out owner) != 0
            && owner == (uint)processId;
    }
}
'@
    }
}

function Write-WindowTimeout($app, $stderrFile) {
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
    if (Test-Path -LiteralPath $stderrFile) {
        Get-Content -LiteralPath $stderrFile -Tail 12 -ErrorAction SilentlyContinue | Write-Host
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
    if (!$app.WaitForExit(10000) -or $app.ExitCode -ne 0) {
        throw 'Selecting Quit from the native popup did not exit Ratio cleanly.'
    }
    Write-Host 'Verified a menu-free window, WebView2 F10 and the native Quit action.'
}

function Assert-ConnectionWindow([string]$phase) {
    Initialize-WindowInspection
    $installedVersion = (Get-Item -LiteralPath $appExe).VersionInfo.ProductVersion
    $stderrFile = [IO.Path]::GetTempFileName()
    $app = $null
    try {
        $app = Start-Process $appExe -PassThru -RedirectStandardError $stderrFile
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
            try { Write-WindowTimeout $app $stderrFile } catch { Write-Warning "Window diagnostics failed: $_" }
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
        if ($app -and !$app.HasExited) {
            $null = $app.CloseMainWindow()
            if (!$app.WaitForExit(5000)) { Stop-Process -Id $app.Id -Force }
        }
        Remove-Item -LiteralPath $stderrFile -Force -ErrorAction SilentlyContinue
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
            body = ('<!doctype html><a href="' + route + '">Download test SVG</a>').encode()
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
        $link.SetFocus()
        $shell.SendKeys('{ENTER}')
        $condition = [System.Windows.Automation.AndCondition]::new(
            [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::NameProperty, 'Save canvas SVG'),
            [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::ProcessIdProperty, [int]$app.Id))
        $root = [System.Windows.Automation.AutomationElement]::RootElement
        $end = (Get-Date).AddSeconds(30)
        $save = $null
        $buttonCondition = [System.Windows.Automation.AndCondition]::new(
            [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::ControlTypeProperty, [System.Windows.Automation.ControlType]::Button),
            [System.Windows.Automation.OrCondition]::new(
                [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::AutomationIdProperty, '1'),
                [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::NameProperty, 'Save')))
        do {
            $dialog = $root.FindFirst([System.Windows.Automation.TreeScope]::Children, $condition)
            if ($dialog) { $save = $dialog.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $buttonCondition) }
            if (!$save) { Start-Sleep -Milliseconds 200 }
        } until ($save -or (Get-Date) -ge $end)
        if (!$save) { throw 'Ratio did not display its native Save canvas SVG dialog and Save button.' }
        # Send real input after UIA confirms the process-owned control. Calling
        # a UIA provider synchronously to invoke a modal control can block a test.
        $save.SetFocus()
        $shell.SendKeys(' ')
        $end = (Get-Date).AddSeconds(15)
        while (!(Test-Path -LiteralPath $destination) -and (Get-Date) -lt $end) { Start-Sleep -Milliseconds 100 }
        $expected = (Get-FileHash -LiteralPath (Join-Path $dir $name)).Hash
        if (!(Test-Path -LiteralPath $destination) -or (Get-FileHash -LiteralPath $destination).Hash -ne $expected) {
            throw 'The native Save dialog did not write the expected SVG bytes to the suggested filename.'
        }
        Write-Host "Verified native SVG dialog, suggested filename and saved bytes (SHA-256 $expected)."
    } finally {
        if ($app -and !$app.HasExited) {
            $null = $app.CloseMainWindow()
            if (!$app.WaitForExit(5000)) { $app.Kill(); $app.WaitForExit() }
        }
        if ($server -and !$server.HasExited) { $server.Kill(); $server.WaitForExit() }
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

# The default NSIS uninstall process relaunches from TEMP and removes itself.
$uninstaller = Join-Path $installDir 'Uninstall.exe'
$result = Start-Process $uninstaller -ArgumentList '/S' -Wait -PassThru
$end = (Get-Date).AddSeconds(20)
while ((Test-Path $uninstaller) -and (Get-Date) -lt $end) { Start-Sleep -Milliseconds 200 }
if ($result.ExitCode -ne 0 -or (Test-Path $installDir) -or (Test-Path $shortcut) -or (Test-Path $key)) {
    throw 'Ratio uninstall left application files, a shortcut, or a registration.'
}
if ((Get-Content -Raw $sentinel).Trim() -ne 'retained') { throw 'Uninstall changed user data.' }
if ($LegacyInstaller -and (Get-Content -Raw $legacyUserFile).Trim() -ne 'keep this file') {
    throw 'Uninstall removed an unrelated legacy file.'
}
