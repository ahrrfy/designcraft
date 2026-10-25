$ErrorActionPreference = 'Stop'
# Reuse the local archive, digest and protection regressions; no fixture is executable.
. (Join-Path $PSScriptRoot 'test-preview-updater.ps1')
. (Join-Path $PSScriptRoot 'start-native-preview.ps1') -ManifestPath $manifest -LibraryOnly
. (Join-Path $PSScriptRoot 'sync-upstream-local.ps1') -ManifestPath $manifest -LibraryOnly
$launcher = Join-Path $PSScriptRoot 'start-native-preview.ps1'
$updater = Join-Path $PSScriptRoot 'update-native-preview.ps1'
$syncScript = Join-Path $PSScriptRoot 'sync-upstream-local.ps1'
$activeExe = Join-Path $source 'designcraft.exe'
Check ((Resolve-UpstreamSyncAction $false $true) -eq 'integrate_upstream_and_publish') 'New upstream commits were not scheduled for integration.'
Check ((Resolve-UpstreamSyncAction $true $false) -eq 'publish_existing_main') 'Unpublished main commit was not scheduled for publication.'
Check ((Resolve-UpstreamSyncAction $true $true) -eq 'check_local_update_only') 'Already synced and published main did not fast-path to local update check.'
function Reset-FlowManifest {
    JsonFile $manifest ([pscustomobject]@{
        profile=$profile; document_path=$book; executable=$activeExe; port=65335
        executable_sha256=$meta.binaries.'designcraft.exe'; published_source_commit=('b' * 40)
        custom_setting=[pscustomobject]@{nested='retain-me'; values=@(1,2,3)}
    })
}
Reset-FlowManifest
Install-UpdatePointer $manifest $installed $meta $true | Out-Null
$before = (Get-FileHash -LiteralPath $manifest).Hash
$beforeConfig = $env:DESIGNCRAFT_CONFIG_DIR
$beforeManifest = $env:DESIGNCRAFT_PREVIEW_MANIFEST
$checked = & $launcher -ManifestPath $manifest -ValidateOnly
Check ($checked.executable -eq $activeExe) 'Validation promoted the active executable.'
Check ((Get-FileHash -LiteralPath $manifest).Hash -eq $before) 'Validation wrote the staged manifest.'
Check ($env:DESIGNCRAFT_CONFIG_DIR -eq $beforeConfig -and $env:DESIGNCRAFT_PREVIEW_MANIFEST -eq $beforeManifest) 'Validation mutated the environment.'
Check (-not (Test-Path -LiteralPath (Join-Path $testRoot 'managed-updates'))) 'Validation created update storage.'
$taskSpec = Get-PreviewUpdateTaskDefinition $manifest 'Smart Design Auto Update' 120
Check ($taskSpec.task_name -eq 'Smart Design Auto Update' -and $taskSpec.interval_minutes -eq 120) 'Scheduled task definition metadata differs.'
Check ($taskSpec.executable.EndsWith('powershell.exe') -and $taskSpec.arguments.Contains('-WindowStyle Hidden') -and $taskSpec.arguments.Contains('-NonInteractive') -and $taskSpec.arguments.Contains($updater) -and $taskSpec.arguments.Contains($manifest)) 'Scheduled task command arguments differ.'
$syncTaskSpec = Get-PreviewUpdateTaskDefinition $manifest 'Smart Design Upstream Sync' 360 'sync-upstream-local.ps1' $PSScriptRoot
Check ($syncTaskSpec.arguments.Contains($syncScript) -and $syncTaskSpec.arguments.Contains('-RepoPath')) 'Upstream sync scheduled task definition differs.'
Check ((Get-FileHash -LiteralPath $manifest).Hash -eq $before) 'Task definition mutated the manifest.'
Reject { Get-PreviewUpdateTaskDefinition $manifest 'Invalid:TaskName' 120 }
Reject { Get-PreviewUpdateTaskDefinition $manifest 'Smart Design Auto Update' 5 }
Reject { Get-PreviewUpdateTaskDefinition $manifest 'Smart Design Auto Update' 120 'evil.ps1' }
Reject { Resolve-PreviewLaunch $manifest -AppRunning:$true }
Check ((Get-FileHash -LiteralPath $manifest).Hash -eq $before) 'Live launcher promoted a stage.'
Reject { Resolve-PreviewLaunch $manifest $activeExe }
Check ((Get-FileHash -LiteralPath $manifest).Hash -eq $before) 'Executable override changed a staged pointer.'
$resolved = Resolve-PreviewLaunch $manifest -AppRunning:$false
Check ($resolved.executable -eq (Join-Path $installed 'designcraft.exe') -and $resolved.update_status -eq 'installed_for_next_launch') 'Next launch did not promote the valid stage.'
Check ($resolved.custom_setting.nested -eq 'retain-me' -and $resolved.custom_setting.values.Count -eq 3) 'Unrelated nested settings changed.'
try {
    Set-PreviewLaunchEnvironment $manifest $resolved
    Check ($env:DESIGNCRAFT_CONFIG_DIR -eq $profile -and $env:DESIGNCRAFT_PREVIEW_MANIFEST -eq $manifest) 'Launcher environment contract differs.'
} finally {
    $env:DESIGNCRAFT_CONFIG_DIR = $beforeConfig
    $env:DESIGNCRAFT_PREVIEW_MANIFEST = $beforeManifest
}
$shortcutDir = Join-Path $testRoot 'shortcuts'
$shortcuts = @(Install-PreviewLauncherShortcuts $manifest $shortcutDir)
Check ($shortcuts.Count -eq 2) 'Launch/update shortcut pair is missing.'
$shell = New-Object -ComObject WScript.Shell
try {
    foreach ($path in $shortcuts) {
        Check (Test-Path -LiteralPath $path -PathType Leaf) 'Shortcut was not saved.'
        $link = $shell.CreateShortcut($path)
        Check ($link.Arguments.Contains('-WindowStyle Hidden') -and $link.Arguments.Contains($manifest) -and $link.TargetPath.EndsWith('powershell.exe')) 'Shortcut arguments are wrong.'
        $expectedScript = if ($path.EndsWith('Smart Design Launcher.lnk')) { $launcher } else { $updater }
        Check ($link.Arguments.Contains($expectedScript)) 'Shortcut target script is wrong.'
    }
} finally { [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell) | Out-Null }
Check ((Read-UpdateJson $manifest).update_script_path -eq $updater) 'Trusted updater path was not installed.'

# Exercise the actual updater entry point with local-only network/process doubles.
# It must emit exactly one JSON line for every successful outcome.
$metaFile = Join-Path $testRoot 'metadata.json'
JsonFile $metaFile $meta
$syncMeta = [pscustomobject]@{
    schemaVersion = 1; repository = 'ahrrfy/designcraft'; sourceCommit = $sha; syncBaseCommit = ('c' * 40)
    ciUrl = 'https://github.com/ahrrfy/designcraft/actions/runs/2'
    archive = $meta.archive; binaries = $meta.binaries
}
$syncMetaFile = Join-Path $testRoot 'sync-metadata.json'
JsonFile $syncMetaFile $syncMeta
$commitMeta = [pscustomobject]@{
    schemaVersion = 1; repository = 'ahrrfy/designcraft'; sourceCommit = $sha
    ciUrl = ('https://github.com/ahrrfy/designcraft/commit/' + $sha)
    archive = $meta.archive; binaries = $meta.binaries
}
$commitMetaFile = Join-Path $testRoot 'commit-metadata.json'
JsonFile $commitMetaFile $commitMeta
$destination = Join-Path $testRoot ('managed-updates/versions/' + $sha)
[IO.Directory]::CreateDirectory($destination) | Out-Null
foreach ($name in @('designcraft.exe','designcraft-cli.exe','SOURCE-COMMIT.txt')) { [IO.File]::Copy((Join-Path $source $name), (Join-Path $destination $name), $true) }
function Invoke-FlowScenario([string]$Scenario) {
    function Invoke-RestMethod {
        param($Uri, $Headers, $TimeoutSec)
        if ($Scenario -eq 'already_staged') { throw 'A staged update must retain its ready result without a network request.' }
        if ($Uri -like '*/releases?*') {
            if ($Scenario -eq 'no_published_update') { Write-Output -NoEnumerate @(); return }
            $candidate = [pscustomobject]@{
                draft=$false; tag_name='v0.12.0-ai.bffea26327cf'; published_at='2026-10-23T20:03:00Z'
                assets=@(
                    [pscustomobject]@{name='update.json'; size=1000; browser_download_url='https://github.com/ahrrfy/designcraft/releases/download/v0.12.0-ai.bffea26327cf/update.json'},
                    [pscustomobject]@{name=$meta.archive.name; size=$meta.archive.size; browser_download_url=('https://github.com/ahrrfy/designcraft/releases/download/v0.12.0-ai.bffea26327cf/' + $meta.archive.name)}
                )
            }
            $legacy = [pscustomobject]@{
                draft=$false; tag_name='v0.9.0-ai.1'; published_at='2026-10-18T00:00:00Z'
                assets=@()
            }
            # Match Windows PowerShell 5.1 Invoke-RestMethod non-enumerated JSON array output.
            Write-Output -NoEnumerate @($candidate, $legacy)
            return
        }
        if ($Uri -like '*/commits/*/status') {
            return [pscustomobject]@{sha=$sha; statuses=@([pscustomobject]@{context='Windows quality gates'; state='success'; creator=[pscustomobject]@{login='ahrrfy'}})}
        }
        if ($Uri -like '*/compare/*...main') { return [pscustomobject]@{status='identical'} }
        if ($Uri -like '*/compare/*') { return [pscustomobject]@{status=$(if ($Scenario -eq 'older_ancestor') { 'behind' } else { 'ahead' })} }
        if ($Uri -like '*/actions/runs/*') {
            if ($Scenario -eq 'upstream_sync') {
                return [pscustomobject]@{head_sha=('c' * 40); head_repository=[pscustomobject]@{full_name='ahrrfy/designcraft'}; path='.github/workflows/upstream-sync.yml'; status='completed'; conclusion='success'; event='schedule'; head_branch='main'}
            }
            return [pscustomobject]@{head_sha=$sha; head_repository=[pscustomobject]@{full_name='ahrrfy/designcraft'}; path='.github/workflows/ci.yml'; status='completed'; conclusion='success'; event='push'; head_branch='main'}
        }
        throw 'Unexpected network request.'
    }
    function Invoke-WebRequest {
        param([switch]$UseBasicParsing, $Uri, $OutFile, $TimeoutSec)
        if ($Uri -notlike '*/update.json') { throw 'Unexpected archive download.' }
        $sourceMeta = if ($Scenario -eq 'upstream_sync') { $syncMetaFile } elseif ($Scenario -eq 'commit_status_proof') { $commitMetaFile } else { $metaFile }
        [IO.File]::Copy($sourceMeta, $OutFile, $true)
    }
    function Get-Process {
        param($Name, $ErrorAction)
        if ($Scenario -eq 'staged_waiting_for_app_close') { return [pscustomobject]@{Id=123} }
        if ($Scenario -eq 'installed_for_next_launch') { return [pscustomobject]@{Id=987654; Path='C:\Program Files\DesignCraft\designcraft.exe'} }
    }
    $lines = @(& $updater -ManifestPath $manifest)
    Check ($lines.Count -eq 1 -and $lines[0] -is [string] -and $lines[0].Length -lt 40000) 'Updater did not emit exactly one bounded JSON result.'
    $result = $lines[0] | ConvertFrom-Json
    $expectedStatus = if ($Scenario -eq 'older_ancestor') { 'up_to_date' } elseif ($Scenario -eq 'already_staged') { 'staged_waiting_for_app_close' } elseif ($Scenario -in @('upstream_sync', 'commit_status_proof')) { 'installed_for_next_launch' } else { $Scenario }
    Check ($result.status -eq $expectedStatus -and @($result.PSObject.Properties).Count -eq 4) 'Updater result schema/status differs.'
    if ($Scenario -eq 'no_published_update') {
        Check ($null -eq $result.selectedCommit -and $null -eq $result.targetExe -and $null -eq $result.ciUrl) 'Missing release result must use null targets.'
    } elseif ($Scenario -eq 'older_ancestor') {
        Check ($result.selectedCommit -eq ('b' * 40) -and $result.targetExe -eq $activeExe -and $null -eq $result.ciUrl) 'Older published ancestor replaced the installed build.'
        Check ((Read-UpdateJson $manifest).executable -eq $activeExe) 'Older published ancestor changed the active pointer.'
    } else {
        $expectedCiUrl = if ($Scenario -eq 'upstream_sync') { $syncMeta.ciUrl } elseif ($Scenario -eq 'commit_status_proof') { $commitMeta.ciUrl } else { $meta.ciUrl }
        Check ($result.selectedCommit -eq $sha -and [IO.Path]::IsPathRooted($result.targetExe) -and $result.ciUrl -eq $expectedCiUrl) 'Updater result metadata differs.'
    }
}
foreach ($scenario in @('no_published_update','up_to_date','older_ancestor','staged_waiting_for_app_close','installed_for_next_launch','upstream_sync','commit_status_proof','already_staged')) {
    Reset-FlowManifest
    if ($scenario -eq 'up_to_date') {
        $record = Read-UpdateJson $manifest
        $record.published_source_commit = $sha
        JsonFile $manifest $record
    }
    if ($scenario -eq 'already_staged') { Install-UpdatePointer $manifest $installed $meta $true | Out-Null }
    Invoke-FlowScenario $scenario
}
Check ((Get-Content -LiteralPath $book -Raw) -eq 'Synthetic user work; never overwrite.') 'Flow changed the document.'
Check ((Get-Content -LiteralPath (Join-Path $profile 'sentinel.txt') -Raw) -eq 'Synthetic existing settings; never overwrite.') 'Flow changed profile settings.'
'PASS: next-launch promotion, live refusal, read-only validation/environment, scheduled-task specs, nested settings, launch/update shortcut setup, all one-line updater JSON outcomes including upstream-sync and commit-status proofs, older ancestor and staged recheck; no binaries started or network/provider requests sent.'
