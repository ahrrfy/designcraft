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
foreach ($name in @('designcraft.exe', 'designcraft-cli.exe')) { [IO.File]::WriteAllText((Join-Path $source $name), ('synthetic fixture, never executable: ' + $name)) }
$sha = 'a' * 40
[IO.File]::WriteAllText((Join-Path $source 'SOURCE-COMMIT.txt'), $sha)
$archive = Join-Path $testRoot 'fixture.zip'
[IO.Compression.ZipFile]::CreateFromDirectory($source, $archive)
$meta = [pscustomobject]@{
    schemaVersion = 1; repository = 'ahrrfy/designcraft'; sourceCommit = $sha
    ciUrl = 'https://github.com/ahrrfy/designcraft/actions/runs/1'
    archive = [pscustomobject]@{name = 'smart-design-v0.9.0-ai.test-windows-x64-unsigned-portable.zip'; sha256 = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant(); size = (Get-Item -LiteralPath $archive).Length}
    binaries = [pscustomobject]@{
        'designcraft.exe' = (Get-FileHash -LiteralPath (Join-Path $source 'designcraft.exe') -Algorithm SHA256).Hash.ToLowerInvariant()
        'designcraft-cli.exe' = (Get-FileHash -LiteralPath (Join-Path $source 'designcraft-cli.exe') -Algorithm SHA256).Hash.ToLowerInvariant()
    }
}
$installed = Join-Path $testRoot 'installed'
Expand-VerifiedUpdate $archive $installed $meta
Reject { Expand-VerifiedUpdate $archive $installed $meta }
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
$before = (Get-FileHash -LiteralPath $manifest -Algorithm SHA256).Hash
Check ((Install-UpdatePointer $manifest $installed $meta $true) -eq 'staged_waiting_for_app_close') 'Live app was not deferred.'
Check ((Get-FileHash -LiteralPath $manifest -Algorithm SHA256).Hash -eq $before) 'A live app changed the launcher.'
[IO.File]::WriteAllText((Join-Path $installed 'designcraft-cli.exe'), 'damaged')
Reject { Install-UpdatePointer $manifest $installed $meta $false }
[IO.File]::Copy((Join-Path $source 'designcraft-cli.exe'), (Join-Path $installed 'designcraft-cli.exe'), $true)
Check ((Install-UpdatePointer $manifest $installed $meta $false) -eq 'installed_for_next_launch') 'Valid update was not installed.'
$after = Read-UpdateJson $manifest
Check ($after.profile -eq $profile -and $after.document_path -eq $book -and $after.custom_setting -eq 'retain-me') 'Existing launcher settings changed.'
Check ((Get-Content -LiteralPath $book -Raw) -eq 'Synthetic user work; never overwrite.') 'User book changed.'
Check ((Get-Content -LiteralPath (Join-Path $profile 'sentinel.txt') -Raw) -eq 'Synthetic existing settings; never overwrite.') 'Profile changed.'
Check (Test-Path -LiteralPath ($manifest + '.before-update')) 'Rollback pointer is missing.'
Write-Output 'Updater tests passed: source/size/digest/path guards, live-app deferral, damaged CLI rejection, profile/document preservation and atomic rollback.'
# Leave the small synthetic fixture for diagnosis; no recursive deletion of a computed path.
