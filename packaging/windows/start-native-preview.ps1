param(
    [Parameter(Mandatory = $true)][string]$ManifestPath,
    [string]$ExecutablePath,
    [switch]$EnableControl,
    [switch]$ValidateOnly,
    [switch]$Hidden,
    [switch]$LibraryOnly
)
$ErrorActionPreference = 'Stop'
$utf8NoBom = [System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = $utf8NoBom
$OutputEncoding = $utf8NoBom
function Format-QuotedWinArg([string]$Value) {
    return '"' + ($Value -replace '(\\+)$', '$1$1') + '"'
}
# Load only our installed local helper, never a release-provided script.
$taskLauncherLibraryOnly = $LibraryOnly
. (Join-Path $PSScriptRoot 'update-native-preview.ps1') -ManifestPath $ManifestPath -LibraryOnly
$LibraryOnly = $taskLauncherLibraryOnly

function Resolve-PreviewLaunch([string]$Path, [string]$OverrideExecutable, [switch]$ValidateOnly, [bool]$AppRunning = $false) {
    if (-not [IO.Path]::IsPathRooted($Path)) { throw 'An absolute launcher manifest is required.' }
    $record = Read-UpdateJson $Path
    Assert-PreviewProtectedPaths $record
    $port = 0
    if (-not [int]::TryParse([string]$record.port, [ref]$port) -or $port -lt 1 -or $port -gt 65535) { throw 'Invalid preview control port.' }
    if (-not $ValidateOnly -and $AppRunning) { throw 'Smart Design is already open. Save your work and close that window before relaunching.' }
    if ($record.update_status -eq 'staged_waiting_for_app_close') {
        if ($OverrideExecutable) { throw 'An executable override cannot bypass a staged update.' }
        Promote-StagedUpdate $Path -ValidateOnly:$ValidateOnly | Out-Null
        if (-not $ValidateOnly) { $record = Read-UpdateJson $Path }
    }
    $exe = if ($OverrideExecutable) { $OverrideExecutable } else { $record.executable }
    if (-not [IO.Path]::IsPathRooted([string]$exe) -or -not (Test-Path -LiteralPath $exe -PathType Leaf)) { throw 'Native preview executable was not found.' }
    $hash = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($record.executable_sha256 -and $hash -cne $record.executable_sha256) { throw 'Native preview executable does not match its recorded SHA-256.' }
    return $record
}

function Set-PreviewLaunchEnvironment([string]$Path, $Record) {
    $env:DESIGNCRAFT_CONFIG_DIR = $Record.profile
    $env:DESIGNCRAFT_PREVIEW_MANIFEST = [IO.Path]::GetFullPath($Path)
}

function Get-PreviewUpdateTaskDefinition([string]$Path, [string]$TaskName = 'Smart Design Auto Update', [int]$IntervalMinutes = 120, [string]$ScriptName = 'update-native-preview.ps1', [string]$RepoPath = '') {
    if (-not [IO.Path]::IsPathRooted($Path)) { throw 'An absolute launcher manifest is required.' }
    if ([string]::IsNullOrWhiteSpace($TaskName) -or $TaskName -match '[\\/:*?"<>|]') { throw 'Invalid scheduled task name.' }
    if ($IntervalMinutes -lt 15 -or $IntervalMinutes -gt 1440) { throw 'Scheduled update interval must be between 15 and 1440 minutes.' }
    if ($ScriptName -notin @('update-native-preview.ps1', 'sync-upstream-local.ps1')) { throw 'Untrusted scheduled task script name.' }
    Resolve-PreviewLaunch $Path -ValidateOnly | Out-Null
    $scriptFile = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot $ScriptName))
    $updater = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot 'update-native-preview.ps1'))
    $fullManifest = [IO.Path]::GetFullPath($Path)
    $powershellExe = Join-Path ([Environment]::GetFolderPath('System')) 'WindowsPowerShell\v1.0\powershell.exe'
    $arguments = '-NoProfile -WindowStyle Hidden -NonInteractive -ExecutionPolicy Bypass -File ' + (Format-QuotedWinArg $scriptFile) + ' -ManifestPath ' + (Format-QuotedWinArg $fullManifest)
    if ($ScriptName -eq 'sync-upstream-local.ps1' -and $RepoPath) {
        if (-not [IO.Path]::IsPathRooted($RepoPath)) { throw 'An absolute repository path is required.' }
        $arguments += ' -RepoPath ' + (Format-QuotedWinArg ([IO.Path]::GetFullPath($RepoPath)))
    }
    return [pscustomobject]@{
        task_name = $TaskName
        executable = $powershellExe
        arguments = $arguments
        working_directory = (Split-Path $fullManifest -Parent)
        updater_script = $updater
        target_script = $scriptFile
        manifest_path = $fullManifest
        interval_minutes = $IntervalMinutes
    }
}

function Install-PreviewUpdateScheduledTask([string]$Path, [string]$TaskName = 'Smart Design Auto Update', [int]$IntervalMinutes = 120, [string]$ScriptName = 'update-native-preview.ps1', [string]$RepoPath = '', [int]$ExecutionMinutes = 120) {
    $spec = Get-PreviewUpdateTaskDefinition $Path $TaskName $IntervalMinutes $ScriptName $RepoPath
    $record = Read-UpdateJson $spec.manifest_path
    $record | Add-Member -NotePropertyName update_script_path -NotePropertyValue $spec.updater_script -Force
    Write-UpdateManifest $spec.manifest_path $record
    $action = New-ScheduledTaskAction -Execute $spec.executable -Argument $spec.arguments -WorkingDirectory $spec.working_directory
    $periodicTrigger = New-ScheduledTaskTrigger -Once -At ([DateTime]::Now.AddMinutes(5)) -RepetitionInterval (New-TimeSpan -Minutes $spec.interval_minutes)
    $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -RunOnlyIfNetworkAvailable -MultipleInstances IgnoreNew -ExecutionTimeLimit (New-TimeSpan -Minutes $ExecutionMinutes)
    Register-ScheduledTask -TaskName $spec.task_name -Action $action -Trigger @($periodicTrigger) -Settings $settings -Description 'Synchronizes, verifies and stages Smart Design updates from ahrrfy/designcraft without interrupting open documents.' -Force | Out-Null
    return $spec
}

# Explicit setup step: a normal launch opens the existing document; its paired
# update shortcut checks/stages only. Neither action changes installed ArtCraft.
function Install-PreviewLauncherShortcuts([string]$Path, [string]$ShortcutDirectory) {
    if (-not [IO.Path]::IsPathRooted($ShortcutDirectory)) { throw 'An absolute shortcut directory is required.' }
    Resolve-PreviewLaunch $Path -ValidateOnly | Out-Null
    $updater = Join-Path $PSScriptRoot 'update-native-preview.ps1'
    $launcher = Join-Path $PSScriptRoot 'start-native-preview.ps1'
    $record = Read-UpdateJson $Path
    $record | Add-Member -NotePropertyName update_script_path -NotePropertyValue ([IO.Path]::GetFullPath($updater)) -Force
    Write-UpdateManifest $Path $record
    [IO.Directory]::CreateDirectory($ShortcutDirectory) | Out-Null
    $shell = New-Object -ComObject WScript.Shell
    try {
        foreach ($item in @(
            @{name='Smart Design Launcher.lnk'; script=$launcher; description='Open Smart Design and activate its verified staged update.'},
            @{name='Check Smart Design Updates.lnk'; script=$updater; description='Check and stage verified Smart Design updates without closing your work.'}
        )) {
            $shortcutPath = Join-Path $ShortcutDirectory $item.name
            $shortcut = $shell.CreateShortcut($shortcutPath)
            $shortcut.TargetPath = Join-Path ([Environment]::GetFolderPath('System')) 'WindowsPowerShell/v1.0/powershell.exe'
            $shortcut.Arguments = '-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File ' + (Format-QuotedWinArg $item.script) + ' -ManifestPath ' + (Format-QuotedWinArg $Path)
            $shortcut.WorkingDirectory = Split-Path $Path -Parent
            $shortcut.Description = $item.description
            $shortcut.WindowStyle = 7
            $shortcut.Save()
            $shortcutPath
        }
    } finally { if ($null -ne $shell) { [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell) | Out-Null } }
}

if ($LibraryOnly) { return }
if ($ValidateOnly) {
    $taskRecord = Resolve-PreviewLaunch $ManifestPath $ExecutablePath -ValidateOnly
    $taskBook = if ($taskRecord.document_path) { $taskRecord.document_path } else { $taskRecord.repaired_native_book }
    $taskExecutable = if ($ExecutablePath) { $ExecutablePath } else { $taskRecord.executable }
    $taskUseControl = $EnableControl.IsPresent -or ($taskRecord.PSObject.Properties['enable_control'] -and [bool]$taskRecord.enable_control)
    [pscustomobject]@{profile=$taskRecord.profile; document=$taskBook; executable=$taskExecutable; port=$taskRecord.port; control_enabled=$taskUseControl; validation_only=$true}
    return
}
# Share the updater lock through promotion and process creation. A concurrent
# update fails safely rather than overwriting a newly published pointer.
if (-not [IO.Path]::IsPathRooted($ManifestPath) -or -not (Test-Path -LiteralPath $ManifestPath -PathType Leaf)) { throw 'An existing absolute launcher manifest is required.' }
$taskRoot = Join-Path (Split-Path $ManifestPath -Parent) 'managed-updates'
[IO.Directory]::CreateDirectory($taskRoot) | Out-Null
$taskLock = $null
try {
    $taskLock = [IO.File]::Open((Join-Path $taskRoot 'update.lock'), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    $taskRecord = Read-UpdateJson $ManifestPath
    $taskPort = 0
    if (-not [int]::TryParse([string]$taskRecord.port, [ref]$taskPort) -or $taskPort -lt 1 -or $taskPort -gt 65535) { throw 'Invalid preview control port.' }
    $taskRunning = (Test-PreviewProcessRunning $taskRecord $ManifestPath) -or
        ($null -ne (Get-NetTCPConnection -LocalPort $taskPort -State Listen -ErrorAction SilentlyContinue))
    $taskRecord = Resolve-PreviewLaunch $ManifestPath $ExecutablePath -AppRunning:$taskRunning
    $taskRecord | Add-Member -NotePropertyName update_script_path -NotePropertyValue ([IO.Path]::GetFullPath((Join-Path $PSScriptRoot 'update-native-preview.ps1'))) -Force
    Write-UpdateManifest $ManifestPath $taskRecord
    $taskExecutable = if ($ExecutablePath) { $ExecutablePath } else { $taskRecord.executable }
    $taskBook = if ($taskRecord.document_path) { $taskRecord.document_path } else { $taskRecord.repaired_native_book }
    Set-PreviewLaunchEnvironment $ManifestPath $taskRecord
    $taskUseControl = $EnableControl.IsPresent -or ($taskRecord.PSObject.Properties['enable_control'] -and [bool]$taskRecord.enable_control)
    $taskArguments = @()
    if ($taskUseControl) {
        $tokenBytes = New-Object byte[] 32
        $rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
        try { $rng.GetBytes($tokenBytes) } finally { $rng.Dispose() }
        $controlToken = -join ($tokenBytes | ForEach-Object { $_.ToString('x2') })
        $env:DESIGNCRAFT_CONTROL_TOKEN = $controlToken
        $tokenFile = Join-Path $taskRecord.profile 'control.token'
        [IO.Directory]::CreateDirectory($taskRecord.profile) | Out-Null
        [IO.File]::WriteAllText($tokenFile, $controlToken, $utf8NoBom)
        $taskArguments += @('--control', [string]$taskPort)
    } else {
        Remove-Item Env:DESIGNCRAFT_CONTROL_TOKEN -ErrorAction SilentlyContinue
        if ($taskRecord.profile) {
            Remove-Item -LiteralPath (Join-Path $taskRecord.profile 'control.token') -Force -ErrorAction SilentlyContinue
        }
    }
    if ($taskBook) { $taskArguments += (Format-QuotedWinArg $taskBook) }
    # Automated starts stay hidden; a user-initiated interactive launch is visible.
    $taskStyle = if ($Hidden) { 'Hidden' } else { 'Normal' }
    $taskProcess = Start-Process -FilePath $taskExecutable -ArgumentList $taskArguments -WindowStyle $taskStyle -PassThru
    if ($null -eq $taskProcess) { throw 'Could not start the native preview.' }
    $taskFields = @{
        app_pid=$taskProcess.Id; executable=$taskExecutable;
        executable_sha256=(Get-FileHash -LiteralPath $taskExecutable -Algorithm SHA256).Hash.ToLowerInvariant();
        status='updated_native_preview_started'; profile_restore_pending=$false;
        app_start_utc=$taskProcess.StartTime.ToUniversalTime().ToString('o')
    }
    foreach ($taskField in $taskFields.GetEnumerator()) { $taskRecord | Add-Member -NotePropertyName $taskField.Key -NotePropertyValue $taskField.Value -Force }
    Write-UpdateManifest $ManifestPath $taskRecord
    $taskProcess
} finally { if ($null -ne $taskLock) { $taskLock.Dispose() } }
