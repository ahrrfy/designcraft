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
Write-Output 'Updater tests passed: source/size/digest/path, exact-CI, upstream-sync and commit-status guards, ancestry decisions, persisted live-app staging, corrupt/missing EXE/CLI/SOURCE rejection, read-only validation, profile/document preservation and atomic promotion/rollback.'
# Leave the small synthetic fixture for diagnosis; no recursive deletion of a computed path.
