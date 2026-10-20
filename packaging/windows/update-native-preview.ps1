param(
    [string]$ManifestPath,
    [switch]$LibraryOnly
)
$ErrorActionPreference = 'Stop'

# This updater never executes scripts from a release or writes inside the user's
# document/profile folders. Updates are side-by-side; the launcher pointer moves
# only while the application is closed. The previous version is retained.
function Read-UpdateJson([string]$Path) {
    if ((Get-Item -LiteralPath $Path).Length -gt 1048576) { throw 'Update JSON exceeds 1 MiB.' }
    Get-Content -LiteralPath $Path -Raw -Encoding UTF8 | ConvertFrom-Json
}

function Assert-UpdateMetadata($Metadata) {
    if ($Metadata.schemaVersion -ne 1 -or $Metadata.repository -cne 'ahrrfy/designcraft') { throw 'Unexpected update source.' }
    foreach ($value in @($Metadata.repository, $Metadata.sourceCommit, $Metadata.archive.name, $Metadata.archive.sha256, $Metadata.ciUrl)) {
        if ($value -isnot [string]) { throw 'Update metadata must contain scalar strings.' }
    }
    if ($Metadata.sourceCommit -cnotmatch '^[0-9a-f]{40}$') { throw 'Invalid source commit.' }
    if ($Metadata.archive.name -cnotmatch '^smart-design-[0-9A-Za-z.-]+-windows-x64-unsigned-portable\.zip$') { throw 'Invalid archive name.' }
    if ($Metadata.archive.sha256 -cnotmatch '^[0-9a-f]{64}$') { throw 'Invalid archive digest.' }
    $archiveSize = [long]0
    if (-not [long]::TryParse([string]$Metadata.archive.size, [ref]$archiveSize) -or $archiveSize -le 0 -or $archiveSize -gt 536870912) { throw 'Archive exceeds the 512 MiB limit.' }
    if ($Metadata.ciUrl -cnotmatch '^https://github\.com/ahrrfy/designcraft/actions/runs/[0-9]+$') { throw 'Invalid CI proof URL.' }
    foreach ($name in @('designcraft.exe', 'designcraft-cli.exe')) {
        $property = $Metadata.binaries.PSObject.Properties[$name]
        if ($null -eq $property -or $property.Value -isnot [string] -or $property.Value -cnotmatch '^[0-9a-f]{64}$') { throw 'Executable digest is missing.' }
    }
}

function Expand-VerifiedUpdate([string]$Archive, [string]$Destination, $Metadata) {
    Assert-UpdateMetadata $Metadata
    if ((Get-Item -LiteralPath $Archive).Length -ne $Metadata.archive.size -or
        (Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash.ToLowerInvariant() -cne $Metadata.archive.sha256) { throw 'Archive digest or size differs.' }
    if (Test-Path -LiteralPath $Destination) { throw 'Refusing to replace an existing version.' }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($Archive)
    try {
        if ($zip.Entries.Count -eq 0 -or $zip.Entries.Count -gt 10000) { throw 'Invalid archive entry count.' }
        $total = [long]0
        $names = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($entry in $zip.Entries) {
            $name = $entry.FullName.Replace('\', '/')
            if ($name -match '(^/|:|(^|/)\.\.?(/|$))' -or $name -match '[\x00-\x1f]' -or
                (($entry.ExternalAttributes -shr 16) -band 61440) -eq 40960 -or
                ($name.Split('/') | Where-Object { $_ -match '[. ]$|^(?i:con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\.|$)' })) { throw 'Unsafe archive path.' }
            if (-not $names.Add($name)) { throw 'Duplicate archive path.' }
            $total += $entry.Length
            if ($entry.Length -gt 1073741824 -or $total -gt 2147483648) { throw 'Expanded archive exceeds 2 GiB.' }
            if (-not $name.EndsWith('/') -and $name -notin @('designcraft.exe', 'designcraft-cli.exe', 'NOTICE', 'LICENSE-MIT', 'LICENSE-APACHE', 'LICENSE-FOXIT', 'ASSETS.md', 'portable.txt', 'SOURCE-COMMIT.txt', 'CI-RUN.txt', 'RUN-ME.txt', 'ai-production.ar.md') -and
                $name -cnotmatch '^(skills|font-licenses)/[0-9A-Za-z_. /-]+\.(md|json|txt)$') { throw 'Unexpected executable or archive entry.' }
        }
        [IO.Directory]::CreateDirectory($Destination) | Out-Null
        foreach ($entry in $zip.Entries) {
            $path = Join-Path $Destination $entry.FullName
            if ($entry.FullName.EndsWith('/')) { [IO.Directory]::CreateDirectory($path) | Out-Null; continue }
            [IO.Directory]::CreateDirectory((Split-Path $path -Parent)) | Out-Null
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $path, $false)
        }
    } finally { $zip.Dispose() }
    foreach ($name in @('designcraft.exe', 'designcraft-cli.exe')) {
        if ((Get-FileHash -LiteralPath (Join-Path $Destination $name) -Algorithm SHA256).Hash.ToLowerInvariant() -cne $Metadata.binaries.PSObject.Properties[$name].Value) { throw 'Executable digest differs.' }
    }
    $source = (Get-Content -LiteralPath (Join-Path $Destination 'SOURCE-COMMIT.txt') -Raw -Encoding ASCII).Trim()
    if ($source -cne $Metadata.sourceCommit) { throw 'Source commit differs.' }
}

function Install-UpdatePointer([string]$Path, [string]$Destination, $Metadata, [bool]$AppRunning) {
    if ($AppRunning) { return 'staged_waiting_for_app_close' }
    $record = Read-UpdateJson $Path
    if (-not (Test-Path -LiteralPath $record.profile -PathType Container)) { throw 'Existing protected profile is missing.' }
    $book = if ($record.document_path) { $record.document_path } else { $record.repaired_native_book }
    if ($book -and -not (Test-Path -LiteralPath $book -PathType Leaf)) { throw 'Existing document is missing.' }
    Assert-UpdateMetadata $Metadata
    foreach ($name in @('designcraft.exe', 'designcraft-cli.exe')) {
        if ((Get-FileHash -LiteralPath (Join-Path $Destination $name) -Algorithm SHA256).Hash.ToLowerInvariant() -cne $Metadata.binaries.PSObject.Properties[$name].Value) { throw 'Staged executable digest differs.' }
    }
    if ((Get-Content -LiteralPath (Join-Path $Destination 'SOURCE-COMMIT.txt') -Raw -Encoding ASCII).Trim() -cne $Metadata.sourceCommit) { throw 'Staged source commit differs.' }
    $exe = Join-Path $Destination 'designcraft.exe'
    $fields = @{
        previous_executable = $record.executable
        executable = $exe
        executable_sha256 = $Metadata.binaries.PSObject.Properties['designcraft.exe'].Value
        published_source_commit = $Metadata.sourceCommit
        update_ci_url = $Metadata.ciUrl
        update_status = 'installed_for_next_launch'
    }
    foreach ($item in $fields.GetEnumerator()) { $record | Add-Member -NotePropertyName $item.Key -NotePropertyValue $item.Value -Force }
    $pending = $Path + '.update-pending'
    [IO.File]::WriteAllText($pending, ($record | ConvertTo-Json -Depth 25), [Text.UTF8Encoding]::new($false))
    # File.Replace is atomic on the same Windows volume; keep a rollback copy.
    [IO.File]::Replace($pending, $Path, ($Path + '.before-update'), $true)
    return 'installed_for_next_launch'
}

if ($LibraryOnly) { return }
if (-not [IO.Path]::IsPathRooted($ManifestPath) -or -not (Test-Path -LiteralPath $ManifestPath -PathType Leaf)) { throw 'An existing absolute launcher manifest is required.' }
$record = Read-UpdateJson $ManifestPath
if (-not [IO.Path]::IsPathRooted($record.profile) -or -not (Test-Path -LiteralPath $record.profile -PathType Container)) { throw 'Existing protected profile is missing.' }
$root = Join-Path (Split-Path $ManifestPath -Parent) 'managed-updates'
[IO.Directory]::CreateDirectory($root) | Out-Null
$lock = $null
try {
    $lock = [IO.File]::Open((Join-Path $root 'update.lock'), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $headers = @{ 'User-Agent' = 'Smart-Design-Updater'; 'Accept' = 'application/vnd.github+json' }
    $releases = @(Invoke-RestMethod -Uri 'https://api.github.com/repos/ahrrfy/designcraft/releases?per_page=20' -Headers $headers -TimeoutSec 45)
    $release = $releases | Where-Object { -not $_.draft -and $_.tag_name -cmatch '^v[0-9]+\.[0-9]+\.[0-9]+-ai[0-9A-Za-z.-]*$' -and @($_.assets | Where-Object name -eq 'update.json').Count -eq 1 } | Sort-Object published_at -Descending | Select-Object -First 1
    if ($null -eq $release) { Write-Output 'No published managed preview update is available.'; return }
    $asset = $release.assets | Where-Object name -eq 'update.json'
    if ($asset.size -gt 1048576 -or $asset.browser_download_url -cnotmatch '^https://github\.com/ahrrfy/designcraft/releases/download/v[0-9A-Za-z.-]+/update\.json$') { throw 'Invalid update metadata asset.' }
    $metaPath = Join-Path $root 'downloaded-update.json'
    Invoke-WebRequest -UseBasicParsing -Uri $asset.browser_download_url -OutFile $metaPath -TimeoutSec 45
    $meta = Read-UpdateJson $metaPath
    Assert-UpdateMetadata $meta
    if ($meta.sourceCommit -ceq $record.published_source_commit) { Write-Output 'The current preview is up to date.'; return }
    if ($record.published_source_commit -cnotmatch '^[0-9a-f]{40}$') { throw 'Current source commit is unknown; automatic replacement is disabled.' }
    $comparison = Invoke-RestMethod -Uri ('https://api.github.com/repos/ahrrfy/designcraft/compare/' + $record.published_source_commit + '...' + $meta.sourceCommit) -Headers $headers -TimeoutSec 45
    if ($comparison.status -ne 'ahead') { throw 'Automatic update would downgrade or diverge from the installed fork.' }
    $runId = $meta.ciUrl.Split('/')[-1]
    $ci = Invoke-RestMethod -Uri ('https://api.github.com/repos/ahrrfy/designcraft/actions/runs/' + $runId) -Headers $headers -TimeoutSec 45
    if ($ci.head_sha -cne $meta.sourceCommit -or $ci.head_repository.full_name -cne 'ahrrfy/designcraft' -or $ci.path -cne '.github/workflows/ci.yml' -or $ci.status -ne 'completed' -or $ci.conclusion -ne 'success' -or
        -not (($ci.event -eq 'push' -and $ci.head_branch -eq 'main') -or $ci.event -eq 'workflow_dispatch')) { throw 'The exact source commit has no successful trusted CI proof.' }
    $archiveAsset = @($release.assets | Where-Object name -CEQ $meta.archive.name)
    if ($archiveAsset.Count -ne 1 -or $archiveAsset[0].size -ne $meta.archive.size -or $archiveAsset[0].browser_download_url -cnotmatch '^https://github\.com/ahrrfy/designcraft/releases/download/v[0-9A-Za-z.-]+/smart-design-[0-9A-Za-z.-]+-windows-x64-unsigned-portable\.zip$') { throw 'Invalid archive asset.' }
    $destination = Join-Path $root ('versions/' + $meta.sourceCommit)
    if (-not (Test-Path -LiteralPath $destination)) {
        $archivePath = Join-Path $root $meta.archive.name
        Invoke-WebRequest -UseBasicParsing -Uri $archiveAsset[0].browser_download_url -OutFile $archivePath -TimeoutSec 300
        Expand-VerifiedUpdate $archivePath $destination $meta
        Remove-Item -LiteralPath $archivePath
    }
    # Never terminate a live app, discard unsaved work, or resume an AI job.
    $running = $null -ne (Get-Process -Name designcraft -ErrorAction SilentlyContinue)
    Install-UpdatePointer $ManifestPath $destination $meta $running
} finally { if ($null -ne $lock) { $lock.Dispose() } }
