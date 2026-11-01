$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'update-native-preview.ps1') -LibraryOnly
$testRoot = Join-Path ([IO.Path]::GetTempPath()) ('smart-design-updater-test-' + [Guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($testRoot) | Out-Null
function Check([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function Reject([scriptblock]$Action) {
    $failed = $false
    try { & $Action | Out-Null } catch { $failed = $true }
    Check $failed 'Expected rejection of invalid update.'
}
function JsonFile([string]$Path, $Value) { [IO.File]::WriteAllText($Path, ($Value | ConvertTo-Json -Depth 15), [Text.UTF8Encoding]::new($false)) }
Add-Type -AssemblyName System.IO.Compression.FileSystem
$source = Join-Path $testRoot 'source'
[IO.Directory]::CreateDirectory($source) | Out-Null
[IO.Directory]::CreateDirectory((Join-Path $source 'font-licenses')) | Out-Null
[IO.File]::WriteAllText((Join-Path $source 'font-licenses/LICENSE-MIT'), 'Synthetic redistribution notice without a file extension.')
[IO.Directory]::CreateDirectory((Join-Path $source 'skills/sample-style/templates')) | Out-Null
[IO.File]::WriteAllText((Join-Path $source 'skills/catalog.json'), '{"schema_version":1,"packages":[]}')
[IO.File]::WriteAllText((Join-Path $source 'skills/sample-style/SKILL.md'), "---`nname: sample-style`ndescription: fixture`n---`nInstructions.`n")
[IO.File]::WriteAllText((Join-Path $source 'skills/sample-style/templates/sample.json'), '[]')
foreach ($name in @('designcraft.exe', 'designcraft-cli.exe')) { [IO.File]::WriteAllText((Join-Path $source $name), ('synthetic fixture, never executable: ' + $name)) }
$sha = 'a' * 40
Check ((Get-PreviewUpdateDecision ('b' * 40) $sha 'behind') -eq 'up_to_date') 'An older published ancestor was not reported as up to date.'
Check ((Get-PreviewUpdateDecision $sha $sha 'identical') -eq 'up_to_date') 'An identical source was not up to date.'
Check ((Get-PreviewUpdateDecision ('b' * 40) $sha 'ahead') -eq 'install') 'A forward candidate was rejected.'
Reject { Get-PreviewUpdateDecision ('b' * 40) $sha 'diverged' }
Reject { Get-PreviewUpdateDecision '' $sha 'ahead' }
$ci = [pscustomobject]@{head_sha=$sha; head_repository=[pscustomobject]@{full_name='ahrrfy/designcraft'}; path='.github/workflows/ci.yml'; status='completed'; conclusion='success'; event='push'; head_branch='main'}
Assert-PreviewCiProof $ci $sha
foreach ($field in @('head_sha', 'path', 'status', 'conclusion', 'event', 'head_branch')) {
    $saved = $ci.$field
    $ci.$field = 'untrusted'
    Reject { Assert-PreviewCiProof $ci $sha }
    $ci.$field = $saved
}
$ci.head_repository.full_name = 'storytold/designcraft'
Reject { Assert-PreviewCiProof $ci $sha }
$ci.head_repository.full_name = 'ahrrfy/designcraft'
$ci.event = 'workflow_dispatch'
Assert-PreviewCiProof $ci $sha
$syncBase = 'c' * 40
$syncCi = [pscustomobject]@{head_sha=$syncBase; head_repository=[pscustomobject]@{full_name='ahrrfy/designcraft'}; path='.github/workflows/upstream-sync.yml'; status='completed'; conclusion='success'; event='schedule'; head_branch='main'}
Assert-PreviewCiProof $syncCi $sha $syncBase 'identical'
Assert-PreviewCiProof $syncCi $sha $syncBase 'ahead'
$syncCi.event = 'workflow_dispatch'
Assert-PreviewCiProof $syncCi $sha $syncBase 'identical'
Reject { Assert-PreviewCiProof $syncCi $sha $syncBase 'diverged' }
Reject { Assert-PreviewCiProof $syncCi $sha $syncBase 'behind' }
Reject { Assert-PreviewCiProof $syncCi $sha ('d' * 40) 'identical' }
Reject { Assert-PreviewCiProof $syncCi $sha '' 'identical' }
$combined = [pscustomobject]@{sha=$sha; statuses=@([pscustomobject]@{context='Windows quality gates'; state='success'; creator=[pscustomobject]@{login='ahrrfy'}})}
Assert-PreviewCommitStatusProof $combined $sha 'identical'
Assert-PreviewCommitStatusProof $combined $sha 'ahead'
Reject { Assert-PreviewCommitStatusProof $combined $sha 'diverged' }
Reject { Assert-PreviewCommitStatusProof ([pscustomobject]@{sha=$sha; statuses=@([pscustomobject]@{context='Windows quality gates'; state='success'; creator=[pscustomobject]@{login='other'}})}) $sha 'identical' }
Reject { Assert-PreviewCommitStatusProof ([pscustomobject]@{sha=$sha; statuses=@([pscustomobject]@{context='Windows quality gates'; state='failure'; creator=[pscustomobject]@{login='ahrrfy'}})}) $sha 'identical' }
[IO.File]::WriteAllText((Join-Path $source 'SOURCE-COMMIT.txt'), $sha)
$archive = Join-Path $testRoot 'fixture.zip'
[IO.Compression.ZipFile]::CreateFromDirectory($source, $archive)
$meta = [pscustomobject]@{
    schemaVersion = 1; repository = 'ahrrfy/designcraft'; sourceCommit = $sha
    ciUrl = 'https://github.com/ahrrfy/designcraft/actions/runs/1'
    archive = [pscustomobject]@{name = 'smart-design-v0.12.0-ai.test-windows-x64-unsigned-portable.zip'; sha256 = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant(); size = (Get-Item -LiteralPath $archive).Length}
    binaries = [pscustomobject]@{
        'designcraft.exe' = (Get-FileHash -LiteralPath (Join-Path $source 'designcraft.exe') -Algorithm SHA256).Hash.ToLowerInvariant()
        'designcraft-cli.exe' = (Get-FileHash -LiteralPath (Join-Path $source 'designcraft-cli.exe') -Algorithm SHA256).Hash.ToLowerInvariant()
    }
}
$installed = Join-Path $testRoot 'installed'
Expand-VerifiedUpdate $archive $installed $meta
Check (Test-Path -LiteralPath (Join-Path $installed 'skills/catalog.json') -PathType Leaf) 'Portable skills catalog was not extracted.'
Check ((Get-Content -LiteralPath (Join-Path $PSScriptRoot 'package.ps1') -Raw).Contains("Join-Path `$Portable 'skills'")) 'Portable ZIP packager does not stage skills/.'
Reject { Expand-VerifiedUpdate $archive $installed $meta }
$meta | Add-Member -NotePropertyName syncBaseCommit -NotePropertyValue $syncBase -Force
Assert-UpdateMetadata $meta
$meta.syncBaseCommit = 'invalid-sha'
Reject { Assert-UpdateMetadata $meta }
$meta.PSObject.Properties.Remove('syncBaseCommit')
$meta.ciUrl = ('https://github.com/ahrrfy/designcraft/commit/' + $sha)
Assert-UpdateMetadata $meta
$meta.ciUrl = ('https://github.com/ahrrfy/designcraft/commit/' + ('d' * 40))
Reject { Assert-UpdateMetadata $meta }
$meta.ciUrl = 'https://github.com/ahrrfy/designcraft/actions/runs/1'
$meta.repository = 'storytold/designcraft'
Reject { Assert-UpdateMetadata $meta }
$meta.repository = 'ahrrfy/designcraft'
$meta.archive.size = 536870913
Reject { Assert-UpdateMetadata $meta }
$meta.archive.size = (Get-Item -LiteralPath $archive).Length
$originalHash = $meta.archive.sha256
$meta.archive.sha256 = 'b' * 64
Reject { Expand-VerifiedUpdate $archive (Join-Path $testRoot 'bad-digest') $meta }
$meta.archive.sha256 = $originalHash
$badZip = Join-Path $testRoot 'traversal.zip'
$zip = [IO.Compression.ZipFile]::Open($badZip, [IO.Compression.ZipArchiveMode]::Create)
try { $entry = $zip.CreateEntry('../escape.exe'); $stream = $entry.Open(); $stream.Dispose() } finally { $zip.Dispose() }
$meta.archive.sha256 = (Get-FileHash -LiteralPath $badZip -Algorithm SHA256).Hash.ToLowerInvariant()
$meta.archive.size = (Get-Item -LiteralPath $badZip).Length
Reject { Expand-VerifiedUpdate $badZip (Join-Path $testRoot 'unsafe') $meta }
Check (-not (Test-Path -LiteralPath (Join-Path $testRoot 'escape.exe'))) 'Archive escaped its directory.'
$meta.archive.sha256 = $originalHash
$meta.archive.size = (Get-Item -LiteralPath $archive).Length
$profile = Join-Path $testRoot 'existing-profile'
[IO.Directory]::CreateDirectory($profile) | Out-Null
[IO.File]::WriteAllText((Join-Path $profile 'sentinel.txt'), 'Synthetic existing settings; never overwrite.')
$book = Join-Path $testRoot 'user-work.designcraft'
[IO.File]::WriteAllText($book, 'Synthetic user work; never overwrite.')
$manifest = Join-Path $testRoot 'launcher.json'
JsonFile $manifest ([pscustomobject]@{profile = $profile; document_path = $book; executable = 'old.exe'; published_source_commit = ('b' * 40); custom_setting = 'retain-me'})
Check ((Install-UpdatePointer $manifest $installed $meta $true) -eq 'staged_waiting_for_app_close') 'Live app was not deferred.'
$staged = Read-UpdateJson $manifest
Check ($staged.update_status -eq 'staged_waiting_for_app_close' -and $staged.staged_executable -eq (Join-Path $installed 'designcraft.exe')) 'Verified staged pointer was not persisted.'
Check ($staged.executable -eq 'old.exe' -and $staged.published_source_commit -eq ('b' * 40)) 'A live app changed the active pointer.'
Check ($staged.staged_cli_sha256 -eq $meta.binaries.'designcraft-cli.exe') 'CLI proof was not persisted.'
$before = (Get-FileHash -LiteralPath $manifest -Algorithm SHA256).Hash
[IO.File]::WriteAllText((Join-Path $installed 'designcraft-cli.exe'), 'damaged')
Reject { Promote-StagedUpdate $manifest }
Check ((Get-FileHash -LiteralPath $manifest -Algorithm SHA256).Hash -eq $before) 'Invalid staged CLI modified the manifest.'
[IO.File]::Copy((Join-Path $source 'designcraft-cli.exe'), (Join-Path $installed 'designcraft-cli.exe'), $true)
foreach ($name in @('designcraft.exe', 'designcraft-cli.exe', 'SOURCE-COMMIT.txt')) {
    $path = Join-Path $installed $name
    $saved = [IO.File]::ReadAllBytes($path)
    [IO.File]::WriteAllText($path, 'damaged')
    Reject { Promote-StagedUpdate $manifest }
    Check ((Get-FileHash -LiteralPath $manifest -Algorithm SHA256).Hash -eq $before) 'Corrupt stage modified the manifest.'
    [IO.File]::WriteAllBytes($path, $saved)
    [IO.File]::Move($path, ($path + '.missing'))
    Reject { Promote-StagedUpdate $manifest }
    Check ((Get-FileHash -LiteralPath $manifest -Algorithm SHA256).Hash -eq $before) 'Missing stage modified the manifest.'
    [IO.File]::Move(($path + '.missing'), $path)
}
Check ((Promote-StagedUpdate $manifest -ValidateOnly) -eq 'staged_waiting_for_app_close') 'Stage validation failed.'
Check ((Get-FileHash -LiteralPath $manifest -Algorithm SHA256).Hash -eq $before) 'Stage validation modified the manifest.'
Check ((Promote-StagedUpdate $manifest) -eq 'installed_for_next_launch') 'Valid update was not promoted.'
$after = Read-UpdateJson $manifest
Check ($after.executable -eq (Join-Path $installed 'designcraft.exe') -and $after.previous_executable -eq 'old.exe') 'Promotion pointer or rollback is wrong.'
Check (@($after.PSObject.Properties.Name | Where-Object { $_ -like 'staged_*' }).Count -eq 0) 'Promotion retained stale staged fields.'
Check ($after.profile -eq $profile -and $after.document_path -eq $book -and $after.custom_setting -eq 'retain-me') 'Existing launcher settings changed.'
Check ((Get-Content -LiteralPath $book -Raw) -eq 'Synthetic user work; never overwrite.') 'User book changed.'
Check ((Get-Content -LiteralPath (Join-Path $profile 'sentinel.txt') -Raw) -eq 'Synthetic existing settings; never overwrite.') 'Profile changed.'
Check (Test-Path -LiteralPath ($manifest + '.before-update')) 'Rollback pointer is missing.'
& {
    $recWithPid = [pscustomobject]@{executable = (Join-Path $installed 'designcraft.exe'); app_pid = 987654}
    function Get-Process { param($Name, $ErrorAction) return @([pscustomobject]@{Id = 987654; Path = 'C:\Program Files\DesignCraft\designcraft.exe'}) }
    Check (-not (Test-PreviewProcessRunning $recWithPid $manifest)) 'Recycled PID on unrelated Launcher installation falsely blocked update.'
    function Get-Process { param($Name, $ErrorAction) return @([pscustomobject]@{Id = 444; Path = (Join-Path $installed 'designcraft.exe')}) }
    Check (Test-PreviewProcessRunning $recWithPid $manifest) 'Active preview process path was not detected.'
    $throwingProc = [pscustomobject]@{Id = 555}
    $throwingProc | Add-Member -MemberType ScriptProperty -Name Path -Value { throw [UnauthorizedAccessException]::new('elevated') }
    function Get-Process { param($Name, $ErrorAction) return @($throwingProc) }
    Check (Test-PreviewProcessRunning $recWithPid $manifest) 'Inaccessible process Path did not fail closed safely.'
}
$canonicalDir = Join-Path $testRoot 'canonical-install'
[IO.Directory]::CreateDirectory($canonicalDir) | Out-Null
[IO.File]::WriteAllText((Join-Path $canonicalDir 'designcraft.exe'), 'old-exe-bytes')
[IO.File]::WriteAllText((Join-Path $canonicalDir 'designcraft-cli.exe'), 'old-cli-bytes')
$canonicalManifest = Join-Path $canonicalDir 'install-manifest.json'
JsonFile $canonicalManifest ([pscustomobject]@{
    install_mode = 'canonical_inplace'
    profile = $profile
    executable = (Join-Path $canonicalDir 'designcraft.exe')
    published_source_commit = ('b' * 40)
    update_script_path = (Join-Path $canonicalDir 'update-native-preview.ps1')
    update_status = 'up_to_date'
})
$canonicalStaged = Join-Path $canonicalDir '.staged-update'
Expand-VerifiedUpdate $archive $canonicalStaged $meta
[IO.File]::WriteAllText((Join-Path $canonicalStaged 'portable.txt'), 'must-be-stripped-for-canonical')
Check ((Install-CanonicalSlotUpdate $canonicalManifest $canonicalStaged $meta) -eq 'installed_for_next_launch') 'Canonical slot update did not promote in-place.'
Check (-not (Test-Path -LiteralPath $canonicalStaged)) 'Canonical .staged-update directory remained after promotion.'
Check (-not (Test-Path -LiteralPath (Join-Path $canonicalDir 'portable.txt'))) 'portable.txt was not stripped during canonical promotion.'
Check (-not (Test-Path -LiteralPath (Join-Path $canonicalDir 'designcraft.exe.old'))) 'Unlocked .old binary was not cleaned after canonical promotion.'
Check (-not (Test-Path -LiteralPath (Join-Path $canonicalDir 'managed-updates'))) 'Canonical update unexpectedly created managed-updates folder.'
Check ((Get-FileHash -LiteralPath (Join-Path $canonicalDir 'designcraft.exe') -Algorithm SHA256).Hash.ToLowerInvariant() -eq $meta.binaries.'designcraft.exe') 'Canonical designcraft.exe hash mismatch.'
Check ((Get-Content -LiteralPath (Join-Path $canonicalDir 'SOURCE-COMMIT.txt') -Raw).Trim() -eq $sha) 'Canonical SOURCE-COMMIT.txt mismatch.'
Check (Test-Path -LiteralPath (Join-Path $canonicalDir 'skills/catalog.json') -PathType Leaf) 'Canonical skills catalog missing after update.'
$lockedOldPath = Join-Path $canonicalDir 'designcraft.exe.old'
[IO.File]::WriteAllText($lockedOldPath, 'still-running-first-session')
$lockedOldHandle = [IO.File]::Open($lockedOldPath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
$runningExeHandle = [IO.File]::Open((Join-Path $canonicalDir 'designcraft.exe'), [IO.FileMode]::Open, [IO.FileAccess]::Read, ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
try {
    Expand-VerifiedUpdate $archive $canonicalStaged $meta
    Check ((Install-CanonicalSlotUpdate $canonicalManifest $canonicalStaged $meta) -eq 'installed_for_next_launch') 'Second canonical update failed when designcraft.exe.old remained locked.'
    $suffixedOld = @(Get-ChildItem -LiteralPath $canonicalDir -Filter 'designcraft.exe.old.*' -Force)
    Check ($suffixedOld.Count -eq 1) 'Locked designcraft.exe.old did not roll over to a suffixed backup.'
    Check (Test-Path -LiteralPath $lockedOldPath -PathType Leaf) 'Locked designcraft.exe.old disappeared unexpectedly.'
} finally {
    $runningExeHandle.Dispose()
    $lockedOldHandle.Dispose()
}
Remove-UnlockedCanonicalBackups $canonicalDir
Check (-not (Test-Path -LiteralPath $lockedOldPath)) 'Unlocked designcraft.exe.old remained after cleanup.'
Check (@(Get-ChildItem -LiteralPath $canonicalDir -Filter 'designcraft.exe.old.*' -Force).Count -eq 0) 'Unlocked suffixed designcraft.exe.old.* remained after cleanup.'
& {
    $script:restCalls = 0
    function Invoke-RestMethod {
        param($Uri, $Headers, $TimeoutSec)
        $script:restCalls++
        if ($script:restCalls -lt 3) { throw [Net.WebException]::new('Unable to connect to the remote server') }
        return [pscustomobject]@{ status = 'ahead' }
    }
    $res = Invoke-PreviewRestWithRetry -Uri 'https://api.github.com/repos/ahrrfy/designcraft/releases?per_page=20' -Headers @{} -TimeoutSec 5 -MaxAttempts 3 -RetryDelaySeconds 0
    Check ($res.status -eq 'ahead' -and $script:restCalls -eq 3) 'Transient REST timeout did not retry and recover.'

    $script:clientCalls = 0
    function Invoke-RestMethod {
        param($Uri, $Headers, $TimeoutSec)
        $script:clientCalls++
        $err = [Exception]::new('Not Found')
        $err | Add-Member -NotePropertyName Response -NotePropertyValue ([pscustomobject]@{ StatusCode = 404 }) -Force
        throw $err
    }
    Reject { Invoke-PreviewRestWithRetry -Uri 'https://api.github.com/repos/ahrrfy/designcraft/releases?per_page=20' -Headers @{} -TimeoutSec 5 -MaxAttempts 3 -RetryDelaySeconds 0 }
    Check ($script:clientCalls -eq 1) 'Client 404 error was retried unnecessarily.'
    Reject { Invoke-PreviewRestWithRetry -Uri 'https://evil.example/repos/ahrrfy/designcraft/releases' -Headers @{} -TimeoutSec 5 -MaxAttempts 2 -RetryDelaySeconds 0 }

    $dlOut = Join-Path $testRoot 'retry-download.json'
    $script:dlCalls = 0
    function Invoke-WebRequest {
        param([switch]$UseBasicParsing, $Uri, $OutFile, $TimeoutSec)
        $script:dlCalls++
        [IO.File]::WriteAllText($OutFile, 'partial-bytes')
        if ($script:dlCalls -eq 1) { throw [IO.IOException]::new('Connection reset by peer') }
        [IO.File]::WriteAllText($OutFile, '{"ok":true}')
    }
    Invoke-PreviewDownloadWithRetry -Uri 'https://github.com/ahrrfy/designcraft/releases/download/v0.15.2-ai.test/update.json' -OutFile $dlOut -TimeoutSec 5 -MaxAttempts 3 -RetryDelaySeconds 0
    Check ($script:dlCalls -eq 2 -and (Get-Content -LiteralPath $dlOut -Raw) -eq '{"ok":true}') 'Transient download reset did not clean partial file and recover.'
    Reject { Invoke-PreviewDownloadWithRetry -Uri 'https://evil.example/update.json' -OutFile $dlOut -TimeoutSec 5 -MaxAttempts 2 -RetryDelaySeconds 0 }
}
Write-Output 'Updater tests passed: source/size/digest/path, exact-CI, upstream-sync and commit-status guards, ancestry decisions, transient network retry, persisted live-app staging, canonical in-place single-slot promotion, multi-update locked .old rollover, corrupt/missing EXE/CLI/SOURCE rejection, read-only validation, profile/document preservation and atomic promotion/rollback.'
# Leave the small synthetic fixture for diagnosis; no recursive deletion of a computed path.
