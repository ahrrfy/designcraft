param(
    [string]$ManifestPath,
    [switch]$LibraryOnly
)
$ErrorActionPreference = 'Stop'
$utf8NoBom = [System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = $utf8NoBom
$OutputEncoding = $utf8NoBom

# This updater never executes scripts from a release or writes inside the user's
# document/profile folders. Updates are side-by-side; the launcher pointer moves
# only while the application is closed. The previous version is retained.
function Read-UpdateJson([string]$Path) {
    if ((Get-Item -LiteralPath $Path).Length -gt 1048576) { throw 'Update JSON exceeds 1 MiB.' }
    Get-Content -LiteralPath $Path -Raw -Encoding UTF8 | ConvertFrom-Json
}

function Test-TrustedCiUrl([string]$Url) {
    return ($Url -cmatch '^https://github\.com/ahrrfy/designcraft/(?:actions/runs/[0-9]+|commit/[0-9a-f]{40})$')
}

function Assert-UpdateMetadata($Metadata) {
    if ($Metadata.schemaVersion -ne 1 -or $Metadata.repository -cne 'ahrrfy/designcraft') { throw 'Unexpected update source.' }
    foreach ($value in @($Metadata.repository, $Metadata.sourceCommit, $Metadata.archive.name, $Metadata.archive.sha256, $Metadata.ciUrl)) {
        if ($value -isnot [string]) { throw 'Update metadata must contain scalar strings.' }
    }
    if ($Metadata.sourceCommit -cnotmatch '^[0-9a-f]{40}$') { throw 'Invalid source commit.' }
    $syncBase = $Metadata.PSObject.Properties['syncBaseCommit']
    if ($null -ne $syncBase -and $null -ne $syncBase.Value -and ($syncBase.Value -isnot [string] -or $syncBase.Value -cnotmatch '^[0-9a-f]{40}$')) { throw 'Invalid sync base commit.' }
    if ($Metadata.archive.name -cnotmatch '^smart-design-[0-9A-Za-z.-]+-windows-x64-unsigned-portable\.zip$') { throw 'Invalid archive name.' }
    if ($Metadata.archive.sha256 -cnotmatch '^[0-9a-f]{64}$') { throw 'Invalid archive digest.' }
    $archiveSize = [long]0
    if (-not [long]::TryParse([string]$Metadata.archive.size, [ref]$archiveSize) -or $archiveSize -le 0 -or $archiveSize -gt 536870912) { throw 'Archive exceeds the 512 MiB limit.' }
    if (-not (Test-TrustedCiUrl $Metadata.ciUrl)) { throw 'Invalid CI proof URL.' }
    if ($Metadata.ciUrl -cmatch '^https://github\.com/ahrrfy/designcraft/commit/([0-9a-f]{40})$' -and $Matches[1] -cne $Metadata.sourceCommit) {
        throw 'Commit proof URL must match sourceCommit.'
    }
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
                $name -cnotmatch '^(skills|font-licenses)/[0-9A-Za-z_. /-]+\.(md|json|txt)$' -and
                $name -cnotmatch '^font-licenses/(?:[0-9A-Za-z_. /-]+/)?(?:OFL|LICENSE|COPYING|NOTICE|AUTHORS)[0-9A-Za-z_-]*$') { throw 'Unexpected executable or archive entry.' }
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

function Assert-PreviewProtectedPaths($record) {
    if (-not [IO.Path]::IsPathRooted([string]$record.profile) -or -not (Test-Path -LiteralPath $record.profile -PathType Container)) { throw 'Existing protected profile is missing.' }
    $book = if ($record.document_path) { $record.document_path } else { $record.repaired_native_book }
    if ($book -and (-not [IO.Path]::IsPathRooted([string]$book) -or -not (Test-Path -LiteralPath $book -PathType Leaf))) { throw 'Existing document is missing.' }
}

function Write-UpdateManifest([string]$Path, $Record) {
    $pending = $Path + '.' + [Guid]::NewGuid().ToString('N') + '.update-pending'
    try {
        $json = $Record | ConvertTo-Json -Depth 25
        if ([Text.Encoding]::UTF8.GetByteCount($json) -gt 1048576) { throw 'Updated manifest exceeds 1 MiB.' }
        [IO.File]::WriteAllText($pending, $json, [Text.UTF8Encoding]::new($false))
        # Same-directory replacement publishes either the complete old or new pointer.
        [IO.File]::Replace($pending, $Path, ($Path + '.before-update'), $true)
    } finally { if (Test-Path -LiteralPath $pending -PathType Leaf) { Remove-Item -LiteralPath $pending } }
}

function Assert-StagedUpdateFiles([string]$Executable, [string]$ExeSha, [string]$CliSha, [string]$SourceCommit, [string]$CiUrl) {
    if (-not [IO.Path]::IsPathRooted($Executable) -or [IO.Path]::GetFileName($Executable) -cne 'designcraft.exe') { throw 'Invalid staged executable path.' }
    if ($ExeSha -cnotmatch '^[0-9a-f]{64}$' -or $CliSha -cnotmatch '^[0-9a-f]{64}$' -or $SourceCommit -cnotmatch '^[0-9a-f]{40}$' -or
        -not (Test-TrustedCiUrl $CiUrl)) { throw 'Staged verification metadata is missing or invalid.' }
    $destination = Split-Path $Executable -Parent
    foreach ($name in @('designcraft.exe', 'designcraft-cli.exe')) {
        $path = Join-Path $destination $name
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw 'Staged executable is missing.' }
        $expected = if ($name -eq 'designcraft.exe') { $ExeSha } else { $CliSha }
        if ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -cne $expected) { throw 'Staged executable digest differs.' }
    }
    $sourcePath = Join-Path $destination 'SOURCE-COMMIT.txt'
    if (-not (Test-Path -LiteralPath $sourcePath -PathType Leaf) -or (Get-Item -LiteralPath $sourcePath).Length -gt 128) { throw 'Staged source commit is missing or oversized.' }
    if ((Get-Content -LiteralPath $sourcePath -Raw -Encoding ASCII).Trim() -cne $SourceCommit) { throw 'Staged source commit differs.' }
}

# No process is started here. The launcher calls this only after checking that
# the previous instance is closed; tests can verify promotion without binaries.
function Promote-StagedUpdate([string]$Path, [switch]$ValidateOnly) {
    $record = Read-UpdateJson $Path
    Assert-PreviewProtectedPaths $record
    if ($record.update_status -ne 'staged_waiting_for_app_close') { return $record.update_status }
    Assert-StagedUpdateFiles $record.staged_executable $record.staged_executable_sha256 $record.staged_cli_sha256 $record.staged_source_commit $record.staged_ci_url
    if ($ValidateOnly) { return 'staged_waiting_for_app_close' }
    $fields = @{
        previous_executable = $record.executable
        executable = $record.staged_executable
        executable_sha256 = $record.staged_executable_sha256
        published_source_commit = $record.staged_source_commit
        update_ci_url = $record.staged_ci_url
        update_status = 'installed_for_next_launch'
    }
    foreach ($item in $fields.GetEnumerator()) { $record | Add-Member -NotePropertyName $item.Key -NotePropertyValue $item.Value -Force }
    foreach ($name in @($record.PSObject.Properties.Name | Where-Object { $_ -like 'staged_*' })) { $record.PSObject.Properties.Remove($name) }
    Write-UpdateManifest $Path $record
    return 'installed_for_next_launch'
}

function Install-UpdatePointer([string]$Path, [string]$Destination, $Metadata, [bool]$AppRunning) {
    $record = Read-UpdateJson $Path
    Assert-PreviewProtectedPaths $record
    Assert-UpdateMetadata $Metadata
    $exe = Join-Path $Destination 'designcraft.exe'
    Assert-StagedUpdateFiles $exe $Metadata.binaries.'designcraft.exe' $Metadata.binaries.'designcraft-cli.exe' $Metadata.sourceCommit $Metadata.ciUrl
    $fields = @{
        update_status = 'staged_waiting_for_app_close'
        staged_executable = $exe
        staged_executable_sha256 = $Metadata.binaries.'designcraft.exe'
        staged_cli_sha256 = $Metadata.binaries.'designcraft-cli.exe'
        staged_source_commit = $Metadata.sourceCommit
        staged_ci_url = $Metadata.ciUrl
        staged_updated_utc = [DateTime]::UtcNow.ToString('o')
    }
    foreach ($item in $fields.GetEnumerator()) { $record | Add-Member -NotePropertyName $item.Key -NotePropertyValue $item.Value -Force }
    Write-UpdateManifest $Path $record
    if ($AppRunning) { return 'staged_waiting_for_app_close' }
    return (Promote-StagedUpdate $Path)
}

function Write-PreviewUpdateResult([string]$Status, $SelectedCommit, $TargetExe, $CiUrl) {
    if ($Status -notin @('up_to_date', 'no_published_update', 'staged_waiting_for_app_close', 'installed_for_next_launch')) { throw 'Invalid update result status.' }
    if ($null -ne $SelectedCommit -and ($SelectedCommit -isnot [string] -or $SelectedCommit -cnotmatch '^[0-9a-f]{40}$')) { throw 'Invalid update result commit.' }
    if ($null -ne $TargetExe -and ($TargetExe -isnot [string] -or -not [IO.Path]::IsPathRooted($TargetExe) -or $TargetExe.Length -gt 32767)) { throw 'Invalid update result path.' }
    if ($null -ne $CiUrl -and ($CiUrl -isnot [string] -or -not (Test-TrustedCiUrl $CiUrl))) { throw 'Invalid update result CI URL.' }
    $json = [pscustomobject]@{status=$Status; selectedCommit=$SelectedCommit; targetExe=$TargetExe; ciUrl=$CiUrl} | ConvertTo-Json -Compress
    if ([Text.Encoding]::UTF8.GetByteCount($json) -gt 65536) { throw 'Update result exceeds 64 KiB.' }
    Write-Output $json
}

function Get-PreviewUpdateDecision([string]$CurrentCommit, [string]$CandidateCommit, [string]$ComparisonStatus) {
    if ($CurrentCommit -cnotmatch '^[0-9a-f]{40}$' -or $CandidateCommit -cnotmatch '^[0-9a-f]{40}$') { throw 'Current or candidate source commit is unknown.' }
    if ($CurrentCommit -ceq $CandidateCommit -or $ComparisonStatus -in @('behind', 'identical')) { return 'up_to_date' }
    if ($ComparisonStatus -ceq 'ahead') { return 'install' }
    throw 'Automatic update would diverge from the installed fork.'
}

function Assert-PreviewCiProof($Ci, [string]$SourceCommit, [string]$SyncBaseCommit = '', [string]$MainComparisonStatus = '') {
    if ($Ci.head_repository.full_name -cne 'ahrrfy/designcraft' -or $Ci.status -ne 'completed' -or $Ci.conclusion -ne 'success') {
        throw 'The exact source commit has no successful trusted CI proof.'
    }
    $directCi = $Ci.path -ceq '.github/workflows/ci.yml' -and $Ci.head_sha -ceq $SourceCommit -and
        (($Ci.event -eq 'push' -and $Ci.head_branch -eq 'main') -or $Ci.event -eq 'workflow_dispatch')
    $upstreamSyncCi = $Ci.path -ceq '.github/workflows/upstream-sync.yml' -and $Ci.head_branch -ceq 'main' -and
        $Ci.event -in @('schedule', 'workflow_dispatch') -and $SyncBaseCommit -cmatch '^[0-9a-f]{40}$' -and
        $Ci.head_sha -ceq $SyncBaseCommit -and $MainComparisonStatus -in @('identical', 'ahead')
    if (-not ($directCi -or $upstreamSyncCi)) { throw 'The exact source commit has no successful trusted CI proof.' }
}

function Assert-PreviewCommitStatusProof($CombinedStatus, [string]$SourceCommit, [string]$MainComparisonStatus) {
    if ($SourceCommit -cnotmatch '^[0-9a-f]{40}$' -or $MainComparisonStatus -notin @('identical', 'ahead')) {
        throw 'Candidate commit is not on main.'
    }
    if ($null -eq $CombinedStatus -or $CombinedStatus.sha -cne $SourceCommit) {
        throw 'Commit status SHA does not match source commit.'
    }
    $verified = @($CombinedStatus.statuses | Where-Object {
        $_.context -ceq 'Windows quality gates' -and $_.state -ceq 'success' -and $_.creator.login -ceq 'ahrrfy'
    })
    if ($verified.Count -lt 1) {
        throw 'The exact source commit has no successful trusted quality-gate status.'
    }
}

function Test-PreviewProcessRunning($Record, [string]$Path) {
    $procs = @(Get-Process -Name designcraft -ErrorAction SilentlyContinue)
    if ($procs.Count -eq 0) { return $false }
    $managedRoot = [IO.Path]::GetFullPath((Join-Path (Split-Path $Path -Parent) 'managed-updates')).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    $knownPaths = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($prop in @('executable', 'staged_executable', 'previous_executable')) {
        $p = if ($null -ne $Record) { $Record.PSObject.Properties[$prop] } else { $null }
        if ($null -ne $p -and -not [string]::IsNullOrWhiteSpace([string]$p.Value) -and [IO.Path]::IsPathRooted([string]$p.Value)) {
            [void]$knownPaths.Add([IO.Path]::GetFullPath([string]$p.Value))
        }
    }
    foreach ($proc in $procs) {
        $procPath = ''
        try {
            $pathProp = $proc.PSObject.Properties['Path']
            if ($null -ne $pathProp -and $null -ne $pathProp.Value) { $procPath = [string]$pathProp.Value }
        } catch {
            $procPath = ''
        }
        if ([string]::IsNullOrWhiteSpace($procPath)) { return $true }
        $fullProcPath = ''
        try {
            $fullProcPath = [IO.Path]::GetFullPath($procPath)
        } catch {
            return $true
        }
        if ($knownPaths.Contains($fullProcPath) -or $fullProcPath.StartsWith($managedRoot, [StringComparison]::OrdinalIgnoreCase)) {
            return $true
        }
    }
    return $false
}

function Test-IsCanonicalManifest($Record, [string]$ManifestPath) {
    $modeProp = if ($null -ne $Record) { $Record.PSObject.Properties['install_mode'] } else { $null }
    if ($null -ne $modeProp -and [string]$modeProp.Value -ceq 'canonical_inplace') { return $true }
    return ([IO.Path]::GetFileName($ManifestPath) -ieq 'install-manifest.json')
}

function Resolve-UnlockedBackupPath([string]$BaseOldPath, [switch]$IsDirectory) {
    if (Test-Path -LiteralPath $BaseOldPath) {
        Remove-Item -LiteralPath $BaseOldPath -Recurse:$IsDirectory -Force -ErrorAction SilentlyContinue
    }
    if (Test-Path -LiteralPath $BaseOldPath) {
        return ($BaseOldPath + '.' + [Guid]::NewGuid().ToString('N').Substring(0, 8))
    }
    return $BaseOldPath
}

function Remove-UnlockedCanonicalBackups([string]$InstallDir) {
    foreach ($oldName in @('designcraft.exe.old', 'designcraft-cli.exe.old', 'SOURCE-COMMIT.txt.old', 'BUILD-IDENTITY.txt.old')) {
        Remove-Item -LiteralPath (Join-Path $InstallDir $oldName) -Force -ErrorAction SilentlyContinue
    }
    foreach ($oldDir in @('.skills.old', '.font-licenses.old')) {
        Remove-Item -LiteralPath (Join-Path $InstallDir $oldDir) -Recurse -Force -ErrorAction SilentlyContinue
    }
    foreach ($entry in @(Get-ChildItem -LiteralPath $InstallDir -Force -ErrorAction SilentlyContinue)) {
        if ($entry.Name -cmatch '^(?:designcraft(?:-cli)?\.exe|SOURCE-COMMIT\.txt|BUILD-IDENTITY\.txt)\.old\.[0-9a-f]{8}$') {
            Remove-Item -LiteralPath $entry.FullName -Force -ErrorAction SilentlyContinue
        } elseif ($entry.PSIsContainer -and $entry.Name -cmatch '^\.(?:skills|font-licenses)\.old\.[0-9a-f]{8}$') {
            Remove-Item -LiteralPath $entry.FullName -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
}

function Install-CanonicalSlotUpdate([string]$Path, [string]$StagedDir, $Metadata) {
    $record = Read-UpdateJson $Path
    Assert-PreviewProtectedPaths $record
    Assert-UpdateMetadata $Metadata
    $stagedExe = Join-Path $StagedDir 'designcraft.exe'
    Assert-StagedUpdateFiles $stagedExe $Metadata.binaries.'designcraft.exe' $Metadata.binaries.'designcraft-cli.exe' $Metadata.sourceCommit $Metadata.ciUrl
    $installDir = Split-Path ([IO.Path]::GetFullPath($Path)) -Parent
    $stagedPortable = Join-Path $StagedDir 'portable.txt'
    if (Test-Path -LiteralPath $stagedPortable -PathType Leaf) {
        Remove-Item -LiteralPath $stagedPortable -Force
    }
    # Verify binaries, subdirectories, and extra files can be replaced in-place atomically; roll back cleanly on any lock/IO failure.
    $movedOld = @()
    $movedSubDirs = @()
    $movedExtraFiles = @()
    try {
        foreach ($bin in @('designcraft.exe', 'designcraft-cli.exe')) {
            $target = Join-Path $installDir $bin
            $old = Resolve-UnlockedBackupPath ($target + '.old')
            $stagedBin = Join-Path $StagedDir $bin
            if (Test-Path -LiteralPath $target -PathType Leaf) {
                Move-Item -LiteralPath $target -Destination $old -Force
                $movedOld += @{ target = $target; old = $old; staged = $stagedBin }
            } else {
                $movedOld += @{ target = $target; old = $null; staged = $stagedBin }
            }
            Move-Item -LiteralPath $stagedBin -Destination $target -Force
        }
        foreach ($subDir in @('skills', 'font-licenses')) {
            $srcSub = Join-Path $StagedDir $subDir
            if (Test-Path -LiteralPath $srcSub -PathType Container) {
                $dstSub = Join-Path $installDir $subDir
                $oldSub = Resolve-UnlockedBackupPath (Join-Path $installDir ('.' + $subDir + '.old')) -IsDirectory
                if (Test-Path -LiteralPath $dstSub) {
                    Move-Item -LiteralPath $dstSub -Destination $oldSub -Force
                    $movedSubDirs += @{ dst = $dstSub; old = $oldSub; staged = $srcSub }
                } else {
                    $movedSubDirs += @{ dst = $dstSub; old = $null; staged = $srcSub }
                }
                Move-Item -LiteralPath $srcSub -Destination $dstSub -Force
            }
        }
        foreach ($entry in @(Get-ChildItem -LiteralPath $StagedDir -Force)) {
            if ($entry.Name -ieq 'portable.txt') { continue }
            if ($entry.PSIsContainer) { continue }
            $dstEntry = Join-Path $installDir $entry.Name
            $oldEntry = Resolve-UnlockedBackupPath ($dstEntry + '.old')
            if (Test-Path -LiteralPath $dstEntry -PathType Leaf) {
                Move-Item -LiteralPath $dstEntry -Destination $oldEntry -Force
                $movedExtraFiles += @{ dst = $dstEntry; old = $oldEntry; staged = $entry.FullName }
            } else {
                $movedExtraFiles += @{ dst = $dstEntry; old = $null; staged = $entry.FullName }
            }
            Move-Item -LiteralPath $entry.FullName -Destination $dstEntry -Force
        }
    } catch {
        foreach ($item in $movedExtraFiles) {
            if ((Test-Path -LiteralPath $item.dst -PathType Leaf) -and -not (Test-Path -LiteralPath $item.staged -PathType Leaf)) {
                Move-Item -LiteralPath $item.dst -Destination $item.staged -Force -ErrorAction SilentlyContinue
            }
            if ($item.old -and -not (Test-Path -LiteralPath $item.dst -PathType Leaf) -and (Test-Path -LiteralPath $item.old -PathType Leaf)) {
                Move-Item -LiteralPath $item.old -Destination $item.dst -Force -ErrorAction SilentlyContinue
            }
        }
        foreach ($sub in $movedSubDirs) {
            if ((Test-Path -LiteralPath $sub.dst -PathType Container) -and -not (Test-Path -LiteralPath $sub.staged -PathType Container)) {
                Move-Item -LiteralPath $sub.dst -Destination $sub.staged -Force -ErrorAction SilentlyContinue
            }
            if ($sub.old -and -not (Test-Path -LiteralPath $sub.dst -PathType Container) -and (Test-Path -LiteralPath $sub.old -PathType Container)) {
                Move-Item -LiteralPath $sub.old -Destination $sub.dst -Force -ErrorAction SilentlyContinue
            }
        }
        foreach ($pair in $movedOld) {
            if ((Test-Path -LiteralPath $pair.target -PathType Leaf) -and -not (Test-Path -LiteralPath $pair.staged -PathType Leaf)) {
                Move-Item -LiteralPath $pair.target -Destination $pair.staged -Force -ErrorAction SilentlyContinue
            }
            if ($pair.old -and -not (Test-Path -LiteralPath $pair.target -PathType Leaf) -and (Test-Path -LiteralPath $pair.old -PathType Leaf)) {
                Move-Item -LiteralPath $pair.old -Destination $pair.target -Force -ErrorAction SilentlyContinue
            }
        }
        # Fall back to staged_waiting_for_app_close in .staged-update (still zero versions/ folders).
        $fields = @{
            install_mode = 'canonical_inplace'
            update_status = 'staged_waiting_for_app_close'
            staged_executable = $stagedExe
            staged_executable_sha256 = $Metadata.binaries.'designcraft.exe'
            staged_cli_sha256 = $Metadata.binaries.'designcraft-cli.exe'
            staged_source_commit = $Metadata.sourceCommit
            staged_ci_url = $Metadata.ciUrl
            staged_updated_utc = [DateTime]::UtcNow.ToString('o')
        }
        foreach ($item in $fields.GetEnumerator()) { $record | Add-Member -NotePropertyName $item.Key -NotePropertyValue $item.Value -Force }
        Write-UpdateManifest $Path $record
        Remove-Item -LiteralPath ($Path + '.before-update') -Force -ErrorAction SilentlyContinue
        return 'staged_waiting_for_app_close'
    }

    foreach ($sub in $movedSubDirs) {
        if ($sub.old -and (Test-Path -LiteralPath $sub.old)) {
            Remove-Item -LiteralPath $sub.old -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
    foreach ($item in $movedExtraFiles) {
        if ($item.old -and (Test-Path -LiteralPath $item.old -PathType Leaf)) {
            Remove-Item -LiteralPath $item.old -Force -ErrorAction SilentlyContinue
        }
    }
    Remove-Item -LiteralPath $StagedDir -Recurse -Force -ErrorAction SilentlyContinue
    Remove-UnlockedCanonicalBackups $installDir
    $canonicalExe = Join-Path $installDir 'designcraft.exe'
    $fields = @{
        install_mode = 'canonical_inplace'
        executable = $canonicalExe
        executable_sha256 = $Metadata.binaries.'designcraft.exe'
        cli_sha256 = $Metadata.binaries.'designcraft-cli.exe'
        published_source_commit = $Metadata.sourceCommit
        update_ci_url = $Metadata.ciUrl
        update_status = 'installed_for_next_launch'
    }
    foreach ($item in $fields.GetEnumerator()) { $record | Add-Member -NotePropertyName $item.Key -NotePropertyValue $item.Value -Force }
    foreach ($name in @($record.PSObject.Properties.Name | Where-Object { $_ -like 'staged_*' -or $_ -eq 'previous_executable' })) {
        $record.PSObject.Properties.Remove($name)
    }
    Write-UpdateManifest $Path $record
    Remove-Item -LiteralPath ($Path + '.before-update') -Force -ErrorAction SilentlyContinue
    return 'installed_for_next_launch'
}

if ($LibraryOnly) { return }
try {
if (-not [IO.Path]::IsPathRooted($ManifestPath) -or -not (Test-Path -LiteralPath $ManifestPath -PathType Leaf)) { throw 'An existing absolute launcher manifest is required.' }
$record = Read-UpdateJson $ManifestPath
if (-not [IO.Path]::IsPathRooted($record.profile) -or -not (Test-Path -LiteralPath $record.profile -PathType Container)) { throw 'Existing protected profile is missing.' }
$canonicalMode = Test-IsCanonicalManifest $record $ManifestPath
$installDir = Split-Path ([IO.Path]::GetFullPath($ManifestPath)) -Parent
$root = if ($canonicalMode) { $installDir } else { Join-Path $installDir 'managed-updates' }
[IO.Directory]::CreateDirectory($root) | Out-Null
$lockPath = if ($canonicalMode) { Join-Path $installDir '.update.lock' } else { Join-Path $root 'update.lock' }
$lock = $null
try {
    $lock = [IO.File]::Open($lockPath, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    $record = Read-UpdateJson $ManifestPath
    Assert-PreviewProtectedPaths $record
    if ($canonicalMode) {
        Remove-UnlockedCanonicalBackups $installDir
    }
    # A verified pending update remains actionable until activated.
    if ($record.update_status -eq 'staged_waiting_for_app_close') {
        if ($canonicalMode -and $record.staged_executable -and (Test-Path -LiteralPath $record.staged_executable -PathType Leaf)) {
            $stagedDir = Split-Path ([string]$record.staged_executable) -Parent
            $stagedMeta = [pscustomobject]@{
                schemaVersion = 1
                repository = 'ahrrfy/designcraft'
                sourceCommit = [string]$record.staged_source_commit
                ciUrl = [string]$record.staged_ci_url
                archive = [pscustomobject]@{ name = 'smart-design-v0.0.0-windows-x64-unsigned-portable.zip'; sha256 = ('0' * 64); size = 1 }
                binaries = [pscustomobject]@{
                    'designcraft.exe' = [string]$record.staged_executable_sha256
                    'designcraft-cli.exe' = [string]$record.staged_cli_sha256
                }
            }
            $promoted = Install-CanonicalSlotUpdate $ManifestPath $stagedDir $stagedMeta
            $targetOut = if ($promoted -eq 'installed_for_next_launch') { Join-Path $installDir 'designcraft.exe' } else { $record.staged_executable }
            Write-PreviewUpdateResult $promoted $record.staged_source_commit $targetOut $record.staged_ci_url
            return
        }
        Promote-StagedUpdate $ManifestPath -ValidateOnly | Out-Null
        Write-PreviewUpdateResult 'staged_waiting_for_app_close' $record.staged_source_commit $record.staged_executable $record.staged_ci_url
        return
    }
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $headers = @{ 'User-Agent' = 'Smart-Design-Updater'; 'Accept' = 'application/vnd.github+json' }
    # Unroll explicitly: Windows PowerShell 5.1 Invoke-RestMethod writes JSON arrays with NoEnumerate.
    $rawReleases = Invoke-RestMethod -Uri 'https://api.github.com/repos/ahrrfy/designcraft/releases?per_page=20' -Headers $headers -TimeoutSec 45
    $releases = @($rawReleases | ForEach-Object { $_ })
    $release = $releases | Where-Object { -not $_.draft -and $_.tag_name -cmatch '^v[0-9]+\.[0-9]+\.[0-9]+-ai[0-9A-Za-z.-]*$' -and @($_.assets | Where-Object name -eq 'update.json').Count -eq 1 } | Sort-Object published_at -Descending | Select-Object -First 1
    if ($null -eq $release) { Write-PreviewUpdateResult 'no_published_update' $null $null $null; return }
    $asset = @($release.assets | Where-Object name -eq 'update.json')[0]
    if ($asset.size -gt 1048576 -or $asset.browser_download_url -cnotmatch '^https://github\.com/ahrrfy/designcraft/releases/download/v[0-9A-Za-z.-]+/update\.json$') { throw 'Invalid update metadata asset.' }
    $metaPath = Join-Path $root $(if ($canonicalMode) { '.downloaded-update.json' } else { 'downloaded-update.json' })
    try {
        Invoke-WebRequest -UseBasicParsing -Uri $asset.browser_download_url -OutFile $metaPath -TimeoutSec 45
        $meta = Read-UpdateJson $metaPath
    } finally {
        if ($canonicalMode -and (Test-Path -LiteralPath $metaPath -PathType Leaf)) {
            Remove-Item -LiteralPath $metaPath -Force -ErrorAction SilentlyContinue
        }
    }
    Assert-UpdateMetadata $meta
    if ($meta.sourceCommit -ceq $record.published_source_commit) { Write-PreviewUpdateResult 'up_to_date' $meta.sourceCommit $record.executable $meta.ciUrl; return }
    if ($record.published_source_commit -cnotmatch '^[0-9a-f]{40}$') { throw 'Current source commit is unknown; automatic replacement is disabled.' }
    $comparison = Invoke-RestMethod -Uri ('https://api.github.com/repos/ahrrfy/designcraft/compare/' + $record.published_source_commit + '...' + $meta.sourceCommit) -Headers $headers -TimeoutSec 45
    if ((Get-PreviewUpdateDecision $record.published_source_commit $meta.sourceCommit $comparison.status) -eq 'up_to_date') {
        $currentCiUrl = if ($record.update_ci_url) { $record.update_ci_url } else { $null }
        Write-PreviewUpdateResult 'up_to_date' $record.published_source_commit $record.executable $currentCiUrl
        return
    }
    if ($meta.ciUrl -cmatch '^https://github\.com/ahrrfy/designcraft/commit/([0-9a-f]{40})$') {
        $combinedStatus = Invoke-RestMethod -Uri ('https://api.github.com/repos/ahrrfy/designcraft/commits/' + $meta.sourceCommit + '/status') -Headers $headers -TimeoutSec 45
        $mainComparison = Invoke-RestMethod -Uri ('https://api.github.com/repos/ahrrfy/designcraft/compare/' + $meta.sourceCommit + '...main') -Headers $headers -TimeoutSec 45
        Assert-PreviewCommitStatusProof $combinedStatus $meta.sourceCommit ([string]$mainComparison.status)
    } else {
        $runId = $meta.ciUrl.Split('/')[-1]
        $ci = Invoke-RestMethod -Uri ('https://api.github.com/repos/ahrrfy/designcraft/actions/runs/' + $runId) -Headers $headers -TimeoutSec 45
        for ($attempt = 0; $attempt -lt 3 -and $ci.path -ceq '.github/workflows/upstream-sync.yml' -and $ci.status -in @('in_progress', 'queued'); $attempt++) {
            Start-Sleep -Seconds 5
            $ci = Invoke-RestMethod -Uri ('https://api.github.com/repos/ahrrfy/designcraft/actions/runs/' + $runId) -Headers $headers -TimeoutSec 45
        }
        $syncBaseVal = if ($null -ne $meta.PSObject.Properties['syncBaseCommit'] -and $meta.syncBaseCommit) { [string]$meta.syncBaseCommit } else { '' }
        $mainStatus = ''
        if ($ci.path -ceq '.github/workflows/upstream-sync.yml') {
            $mainComparison = Invoke-RestMethod -Uri ('https://api.github.com/repos/ahrrfy/designcraft/compare/' + $meta.sourceCommit + '...main') -Headers $headers -TimeoutSec 45
            $mainStatus = [string]$mainComparison.status
        }
        Assert-PreviewCiProof $ci $meta.sourceCommit $syncBaseVal $mainStatus
    }
    $archiveAsset = @($release.assets | Where-Object name -CEQ $meta.archive.name)
    if ($archiveAsset.Count -ne 1 -or $archiveAsset[0].size -ne $meta.archive.size -or $archiveAsset[0].browser_download_url -cnotmatch '^https://github\.com/ahrrfy/designcraft/releases/download/v[0-9A-Za-z.-]+/smart-design-[0-9A-Za-z.-]+-windows-x64-unsigned-portable\.zip$') { throw 'Invalid archive asset.' }
    if ($canonicalMode) {
        $stagedDir = Join-Path $installDir '.staged-update'
        if (Test-Path -LiteralPath $stagedDir) { Remove-Item -LiteralPath $stagedDir -Recurse -Force }
        $archivePath = Join-Path $installDir ('.' + $meta.archive.name)
        try {
            Invoke-WebRequest -UseBasicParsing -Uri $archiveAsset[0].browser_download_url -OutFile $archivePath -TimeoutSec 300
            Expand-VerifiedUpdate $archivePath $stagedDir $meta
        } finally {
            if (Test-Path -LiteralPath $archivePath -PathType Leaf) { Remove-Item -LiteralPath $archivePath -Force -ErrorAction SilentlyContinue }
        }
        $status = Install-CanonicalSlotUpdate $ManifestPath $stagedDir $meta
        $targetExe = if ($status -eq 'installed_for_next_launch') { Join-Path $installDir 'designcraft.exe' } else { Join-Path $stagedDir 'designcraft.exe' }
        Write-PreviewUpdateResult $status $meta.sourceCommit $targetExe $meta.ciUrl
    } else {
        $destination = Join-Path $root ('versions/' + $meta.sourceCommit)
        if (-not (Test-Path -LiteralPath $destination)) {
            $archivePath = Join-Path $root $meta.archive.name
            Invoke-WebRequest -UseBasicParsing -Uri $archiveAsset[0].browser_download_url -OutFile $archivePath -TimeoutSec 300
            Expand-VerifiedUpdate $archivePath $destination $meta
            Remove-Item -LiteralPath $archivePath
        }
        # Never terminate a live app, discard unsaved work, or resume an AI job.
        $running = Test-PreviewProcessRunning $record $ManifestPath
        $status = Install-UpdatePointer $ManifestPath $destination $meta $running
        Write-PreviewUpdateResult $status $meta.sourceCommit (Join-Path $destination 'designcraft.exe') $meta.ciUrl
    }
} finally {
    if ($null -ne $lock) { $lock.Dispose() }
    if ($canonicalMode -and (Test-Path -LiteralPath $lockPath -PathType Leaf)) {
        Remove-Item -LiteralPath $lockPath -Force -ErrorAction SilentlyContinue
    }
}
} catch {
    # One concise stderr line and a nonzero exit; never emit success-shaped JSON.
    $message = 'Update failed: ' + ($_.Exception.Message -replace '[\r\n]+', ' ')
    [Console]::Error.WriteLine($message.Substring(0, [Math]::Min(512, $message.Length)))
    exit 1
}
