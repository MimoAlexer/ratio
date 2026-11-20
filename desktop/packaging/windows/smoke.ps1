# Run on a disposable Windows CI worker with an interactive desktop.
param([Parameter(Mandatory)][string]$Installer)
$ErrorActionPreference = 'Stop'
$setup = (Resolve-Path $Installer).Path
$result = Start-Process $setup -ArgumentList '/S' -Wait -PassThru
if ($result.ExitCode -eq 3) {
    # Only this disposable worker provisions a missing prerequisite. Verify the
    # Microsoft signature before running their official Evergreen bootstrapper.
    $bootstrap = Join-Path $env:RUNNER_TEMP 'MicrosoftEdgeWebview2Setup.exe'
    Invoke-WebRequest 'https://go.microsoft.com/fwlink/p/?LinkId=2124703' -OutFile $bootstrap
    $signature = Get-AuthenticodeSignature $bootstrap
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch '(^|,\s*)O=Microsoft Corporation(,|$)') {
        throw 'Microsoft WebView2 bootstrapper signature verification failed.'
    }
    $runtime = Start-Process $bootstrap -ArgumentList '/silent','/install' -Wait -PassThru
    if ($runtime.ExitCode -ne 0) { throw "WebView2 installer exited with $($runtime.ExitCode)" }
    $result = Start-Process $setup -ArgumentList '/S' -Wait -PassThru
}
if ($result.ExitCode -ne 0) { throw "Working Set installer exited with $($result.ExitCode)" }
$installDir = Join-Path $env:LOCALAPPDATA 'Programs\Working Set'
$shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'Working Set.lnk'
$key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\WorkingSet'
$appExe = Join-Path $installDir 'working-set.exe'
if (!(Test-Path $appExe) -or !(Test-Path $shortcut) -or !(Test-Path $key)) {
    throw 'Missing installed application, Start Menu entry, or uninstall registration.'
}
# The normal first-run profile is new on the hosted worker. The connection
# window title is set only after the app mounts its native WebView2 control.
$app = Start-Process $appExe -PassThru
try {
    $end = (Get-Date).AddSeconds(60)
    do {
        Start-Sleep -Milliseconds 400
        $app.Refresh()
        if ($app.HasExited) { throw "Application exited before opening a window ($($app.ExitCode))." }
    } until ($app.MainWindowTitle -like 'Working Set*Connection' -or (Get-Date) -gt $end)
    if ($app.MainWindowTitle -notlike 'Working Set*Connection') { throw 'First-run connection window did not open.' }
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
} finally {
    if (!$app.HasExited) {
        $null = $app.CloseMainWindow()
        if (!$app.WaitForExit(5000)) { Stop-Process -Id $app.Id -Force }
    }
}
$profileDir = Join-Path $env:LOCALAPPDATA 'Working Set'
$sentinel = Join-Path $profileDir 'installer-ci-sentinel.txt'
Set-Content -LiteralPath $sentinel -Value 'retained'
# _?= keeps the uninstaller in-process so its exit status can be checked.
$uninstaller = Join-Path $installDir 'Uninstall.exe'
$result = Start-Process $uninstaller -ArgumentList "/S _?=$installDir" -Wait -PassThru
if ($result.ExitCode -ne 0 -or (Test-Path $appExe) -or (Test-Path $shortcut) -or (Test-Path $key)) {
    throw 'The uninstall lifecycle did not finish.'
}
if ((Get-Content -Raw $sentinel).Trim() -ne 'retained') { throw 'The desktop profile was not preserved.' }
