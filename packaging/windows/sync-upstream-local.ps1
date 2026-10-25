param(
    [Parameter(Mandatory = $true)][string]$ManifestPath,
    [string]$RepoPath = '',
    [string]$ToolsEnvPath = 'D:\designcraft-tools\env.ps1',
    [switch]$PublishCurrentMainOnly,
    [switch]$LibraryOnly
)
$ErrorActionPreference = 'Stop'
$utf8NoBom = [System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = $utf8NoBom
$OutputEncoding = $utf8NoBom
$CraftFontsPinnedRef = 'abb83316d96aa59c1cf64784289e378fe9fa5695'

function Resolve-UpstreamSyncAction([bool]$UpstreamIsAncestor, [bool]$MainReleaseExists) {
    if (-not $UpstreamIsAncestor) { return 'integrate_upstream_and_publish' }
    if (-not $MainReleaseExists) { return 'publish_existing_main' }
    return 'check_local_update_only'
}

function Ensure-PinnedCraftFonts([string]$CacheDir) {
    if (-not (Test-Path -LiteralPath (Join-Path $CacheDir '.git'))) {
        [IO.Directory]::CreateDirectory($CacheDir) | Out-Null
        git init $CacheDir | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Cannot initialize craft-fonts cache.' }
    }
    $current = ''
    try { $current = (git -C $CacheDir rev-parse HEAD 2>$null).Trim() } catch { $current = '' }
    if ($current -cne $CraftFontsPinnedRef) {
        git -C $CacheDir fetch --depth 1 https://github.com/storytold/craft-fonts.git $CraftFontsPinnedRef
        if ($LASTEXITCODE -ne 0) { throw 'Cannot fetch pinned craft-fonts commit.' }
        git -C $CacheDir checkout --force FETCH_HEAD
        if ($LASTEXITCODE -ne 0) { throw 'Cannot checkout pinned craft-fonts commit.' }
    }
    return $CacheDir
}

function Invoke-WorkspaceQualityGates([string]$Worktree, [string]$TargetDir) {
    Push-Location -LiteralPath $Worktree
    try {
        $env:CARGO_TARGET_DIR = $TargetDir
        & (Join-Path $Worktree 'packaging/windows/test-preview-launcher.ps1') | Out-Null
        & (Join-Path $Worktree 'packaging/windows/test-preview-updater.ps1') | Out-Null
        & (Join-Path $Worktree 'packaging/windows/test-preview-update-flow.ps1') | Out-Null
        cargo fmt --all -- --check
        if ($LASTEXITCODE -ne 0) { throw 'Formatting check failed.' }
        cargo clippy --locked --workspace --all-targets -- -D warnings
        if ($LASTEXITCODE -ne 0) { throw 'Clippy check failed.' }
        cargo test --locked --workspace --lib --bins --tests
        if ($LASTEXITCODE -ne 0) { throw 'Workspace tests failed.' }
        cargo test --locked --workspace --doc
        if ($LASTEXITCODE -ne 0) { throw 'Workspace doctests failed.' }
        foreach ($gate in @('pdf-deps', 'assets', 'layers', 'wasm')) {
            cargo run --locked -p xtask -- $gate
            if ($LASTEXITCODE -ne 0) { throw "xtask $gate failed." }
        }
        git diff --exit-code -- Cargo.lock
        if ($LASTEXITCODE -ne 0) { throw 'Quality gates modified Cargo.lock.' }
    } finally { Pop-Location }
}

function Publish-VerifiedCommitRelease([string]$Worktree, [string]$TargetDir, [string]$CraftFontsDir, [string]$Description) {
    Push-Location -LiteralPath $Worktree
    try {
        $sha = (git rev-parse HEAD).Trim()
        if ($LASTEXITCODE -ne 0 -or $sha -cnotmatch '^[0-9a-f]{40}$') { throw 'Cannot resolve HEAD commit.' }
        $cargoToml = Get-Content -LiteralPath (Join-Path $Worktree 'Cargo.toml') -Raw -Encoding UTF8
        if ($cargoToml -notmatch '(?m)^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"') { throw 'Cannot resolve Cargo.toml workspace version.' }
        $label = 'v' + $Matches[1] + '-ai.' + $sha.Substring(0, 12)
        gh release view $label --repo ahrrfy/designcraft *> $null
        if ($LASTEXITCODE -eq 0) { return $label }

        $ciUrl = 'https://github.com/ahrrfy/designcraft/commit/' + $sha
        gh api --method POST ("repos/ahrrfy/designcraft/statuses/$sha") `
            -f state='success' `
            -f context='Windows quality gates' `
            -f description=$Description `
            -f target_url=$ciUrl | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Cannot publish commit quality-gate status.' }

        $env:CARGO_TARGET_DIR = $TargetDir
        $env:CRAFT_FONTS_DIR = $CraftFontsDir
        $env:CRAFT_FONTS_REQUIRED = '1'
        $env:DESIGNCRAFT_REQUIRE_WINRES = '1'
        $env:DESIGNCRAFT_BUILD_SHA = $sha
        $env:DESIGNCRAFT_BUILD_DATE = (Get-Date).ToUniversalTime().ToString('yyyy-MM-dd')
        cargo build --release --locked -p designcraft -p designcraft-cli
        if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
        git diff --exit-code -- Cargo.lock
        if ($LASTEXITCODE -ne 0) { throw 'Release build changed Cargo.lock.' }

        $distRoot = Join-Path ([IO.Path]::GetTempPath()) ('dc-release-' + [Guid]::NewGuid().ToString('N'))
        $stage = Join-Path $distRoot 'smart-design-portable'
        $releaseDir = Join-Path $distRoot 'preview'
        New-Item -ItemType Directory -Force -Path $stage, $releaseDir | Out-Null
        try {
            $objcopy = Get-Command 'objcopy.exe' -ErrorAction SilentlyContinue
            foreach ($bin in @('designcraft.exe', 'designcraft-cli.exe')) {
                $built = Join-Path $TargetDir ('release/' + $bin)
                $dest = Join-Path $stage $bin
                if ($null -ne $objcopy) {
                    & $objcopy.Source '--strip-debug' $built $dest
                    if ($LASTEXITCODE -ne 0) { throw "Failed to strip debug symbols for $bin." }
                } else {
                    Copy-Item -LiteralPath $built -Destination $dest
                }
            }
            foreach ($doc in @('NOTICE', 'LICENSE-MIT', 'LICENSE-APACHE', 'LICENSE-FOXIT', 'ASSETS.md')) {
                Copy-Item -LiteralPath (Join-Path $Worktree $doc) -Destination $stage
            }
            'Portable Smart Design preview. Settings and recovery remain beside these executables.' | Set-Content -LiteralPath (Join-Path $stage 'portable.txt') -Encoding ascii
            Copy-Item -LiteralPath (Join-Path $Worktree 'docs/ai-production.ar.md') -Destination $stage
            Copy-Item -LiteralPath (Join-Path $Worktree 'skills') -Destination $stage -Recurse
            $fontNotices = Join-Path $stage 'font-licenses'
            New-Item -ItemType Directory -Force -Path $fontNotices | Out-Null
            Copy-Item -LiteralPath (Join-Path $Worktree 'licenses/epaint-default-fonts') -Destination $fontNotices -Recurse
            Get-ChildItem -LiteralPath (Join-Path $Worktree 'assets/fonts') -File -Filter 'OFL-*.txt' | Copy-Item -Destination $fontNotices
            foreach ($cfDoc in @('ATTRIBUTION.md', 'LICENSE-MIT', 'LICENSE-APACHE')) {
                Copy-Item -LiteralPath (Join-Path $CraftFontsDir $cfDoc) -Destination $fontNotices
            }
            Get-ChildItem -LiteralPath (Join-Path $CraftFontsDir 'fonts') -Recurse -File | Where-Object { $_.Name -match '^(OFL|LICENSE|COPYING|NOTICE|AUTHORS)' -or $_.Extension -eq '.md' } | ForEach-Object {
                $relative = $_.FullName.Substring($CraftFontsDir.TrimEnd('\', '/').Length + 1)
                $destination = Join-Path $fontNotices $relative
                New-Item -ItemType Directory -Force -Path (Split-Path $destination -Parent) | Out-Null
                Copy-Item -LiteralPath $_.FullName -Destination $destination
            }
            $arBrand = -join ([char[]]@(0x0627, 0x0644, 0x062A, 0x0635, 0x0645, 0x064A, 0x0645, 0x0020, 0x0627, 0x0644, 0x0630, 0x0643, 0x064A))
            $emDash = [string][char]0x2014
            $runMeLines = @(
                ('Smart Design / ' + $arBrand + ' ' + $emDash + ' independent Windows x64 preview'),
                'Based on DesignCraft by the ArtCraft team and contributors. This fork is independently maintained.',
                'UNSIGNED PREVIEW: these binaries are not code-signed. Pre-alpha; keep backup copies of documents.',
                'Extract the whole ZIP into a new folder, then run .\designcraft.exe. No MSI or system associations are installed.',
                ('Source commit: ' + $sha),
                ('Verified commit quality gates: ' + $ciUrl)
            ) -join "`r`n"
            [System.IO.File]::WriteAllText((Join-Path $stage 'RUN-ME.txt'), ($runMeLines + "`r`n"), $utf8NoBom)
            $sha | Set-Content -LiteralPath (Join-Path $stage 'SOURCE-COMMIT.txt') -Encoding ascii
            $ciUrl | Set-Content -LiteralPath (Join-Path $stage 'CI-RUN.txt') -Encoding ascii
            $zip = Join-Path $releaseDir ('smart-design-' + $label + '-windows-x64-unsigned-portable.zip')
            Add-Type -AssemblyName System.IO.Compression.FileSystem
            [IO.Compression.ZipFile]::CreateFromDirectory($stage, $zip)
            $hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
            ($hash + '  ' + (Split-Path $zip -Leaf)) | Set-Content -LiteralPath (Join-Path $releaseDir 'SHA256SUMS.txt') -Encoding ascii
            $update = @{
                schemaVersion = 1; repository = 'ahrrfy/designcraft'; sourceCommit = $sha; ciUrl = $ciUrl
                archive = @{ name = (Split-Path $zip -Leaf); sha256 = $hash; size = (Get-Item -LiteralPath $zip).Length }
                binaries = @{
                    'designcraft.exe' = (Get-FileHash -LiteralPath (Join-Path $stage 'designcraft.exe') -Algorithm SHA256).Hash.ToLowerInvariant()
                    'designcraft-cli.exe' = (Get-FileHash -LiteralPath (Join-Path $stage 'designcraft-cli.exe') -Algorithm SHA256).Hash.ToLowerInvariant()
                }
            }
            [System.IO.File]::WriteAllText((Join-Path $releaseDir 'update.json'), (($update | ConvertTo-Json -Depth 5) + "`n"), $utf8NoBom)
            $notesPath = Join-Path $distRoot 'release-notes.txt'
            $notesText = @(
                'Independent Smart Design fork. Unsigned Windows x64 portable preview; no MSI.',
                ('Source commit: ' + $sha),
                ('Verified Windows quality gates: ' + $ciUrl),
                'Automatically synced, verified and published. This does not certify physical printing or commercial-press output.'
            ) -join "`n"
            [System.IO.File]::WriteAllText($notesPath, ($notesText + "`n"), $utf8NoBom)
            gh release create $label --repo ahrrfy/designcraft --target $sha --prerelease --title ('Smart Design ' + $label + ' (unsigned preview)') --notes-file $notesPath $zip (Join-Path $releaseDir 'SHA256SUMS.txt') (Join-Path $releaseDir 'update.json') | Out-Null
            if ($LASTEXITCODE -ne 0) { throw 'Failed to create GitHub release.' }
            return $label
        } finally {
            if (Test-Path -LiteralPath $distRoot) { Remove-Item -LiteralPath $distRoot -Recurse -Force -ErrorAction SilentlyContinue }
        }
    } finally { Pop-Location }
}

if ($LibraryOnly) { return }
if (-not [IO.Path]::IsPathRooted($ManifestPath) -or -not (Test-Path -LiteralPath $ManifestPath -PathType Leaf)) {
    throw 'An existing absolute launcher manifest is required.'
}
if (-not $RepoPath) { $RepoPath = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..')) }
if ($ToolsEnvPath -and (Test-Path -LiteralPath $ToolsEnvPath -PathType Leaf)) { . $ToolsEnvPath }
$updater = Join-Path $PSScriptRoot 'update-native-preview.ps1'
$managedRoot = Join-Path (Split-Path $ManifestPath -Parent) 'managed-updates'
[IO.Directory]::CreateDirectory($managedRoot) | Out-Null
$syncLock = $null
try {
    $syncLock = [IO.File]::Open((Join-Path $managedRoot 'upstream-sync.lock'), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    git -C $RepoPath fetch --no-tags origin main
    if ($LASTEXITCODE -ne 0) { throw 'Cannot fetch origin/main.' }
    $originMain = (git -C $RepoPath rev-parse origin/main).Trim()
    $upstreamIsAncestor = $true
    $upstreamSha = $originMain
    if (-not $PublishCurrentMainOnly) {
        git -C $RepoPath fetch --no-tags https://github.com/storytold/designcraft.git main
        if ($LASTEXITCODE -ne 0) { throw 'Cannot fetch upstream storytold/designcraft main.' }
        $upstreamSha = (git -C $RepoPath rev-parse FETCH_HEAD).Trim()
        git -C $RepoPath merge-base --is-ancestor $upstreamSha $originMain
        $upstreamIsAncestor = ($LASTEXITCODE -eq 0)
    }

    $shortMain = $originMain.Substring(0, 12)
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $apiHeaders = @{ 'User-Agent' = 'Smart-Design-Sync'; 'Accept' = 'application/vnd.github+json' }
    $rawReleases = Invoke-RestMethod -Uri 'https://api.github.com/repos/ahrrfy/designcraft/releases?per_page=20' -Headers $apiHeaders -TimeoutSec 45
    $releases = @($rawReleases | ForEach-Object { $_ })
    $mainReleaseExists = @($releases | Where-Object { -not $_.draft -and $_.tag_name -like "*-ai.$shortMain" }).Count -gt 0
    $action = Resolve-UpstreamSyncAction $upstreamIsAncestor $mainReleaseExists

    if ($action -ne 'check_local_update_only') {
        $toolsRoot = if ($ToolsEnvPath -and (Test-Path -LiteralPath (Split-Path $ToolsEnvPath -Parent))) { Split-Path $ToolsEnvPath -Parent } else { [IO.Path]::GetTempPath() }
        $craftFontsDir = Ensure-PinnedCraftFonts (Join-Path $toolsRoot 'craft-fonts')
        $targetDir = Join-Path ([IO.Path]::GetTempPath()) ('dc-auto-sync-target-' + [Guid]::NewGuid().ToString('N').Substring(0, 8))
        $worktree = Join-Path ([IO.Path]::GetTempPath()) ('dc-auto-sync-wt-' + [Guid]::NewGuid().ToString('N').Substring(0, 8))
        try {
            if ($action -eq 'integrate_upstream_and_publish') {
                $shortUp = $upstreamSha.Substring(0, 12)
                $syncBranch = "sync/upstream-$shortUp"
                $openPrRaw = gh pr list --repo ahrrfy/designcraft --state open --head $syncBranch --json url | ConvertFrom-Json
                $openPrs = @($openPrRaw | ForEach-Object { $_ })
                $openPr = if ($openPrs.Count -gt 0) { [string]$openPrs[0].url } else { '' }
                if (-not $openPr) {
                    git -C $RepoPath worktree add --detach $worktree $originMain | Out-Null
                    Push-Location -LiteralPath $worktree
                    try {
                        git checkout -B $syncBranch | Out-Null
                        git merge --no-ff --no-commit $upstreamSha
                        if ($LASTEXITCODE -ne 0) {
                            $conflicts = @(git diff --name-only --diff-filter=U)
                            $autoResolvable = $conflicts.Count -gt 0 -and @($conflicts | Where-Object { $_ -ne 'Cargo.lock' -and $_ -cnotmatch '^\.github/workflows/[^/]+\.yml$' }).Count -eq 0
                            if ($autoResolvable) {
                                foreach ($wfPath in @($conflicts | Where-Object { $_ -cmatch '^\.github/workflows/[^/]+\.yml$' })) {
                                    git cat-file -e ("HEAD:" + $wfPath) 2>$null
                                    if ($LASTEXITCODE -eq 0) {
                                        git checkout --ours -- $wfPath
                                        git add -- $wfPath
                                    } else {
                                        git rm -f -- $wfPath | Out-Null
                                    }
                                }
                                if ($conflicts -contains 'Cargo.lock') {
                                    git checkout --ours Cargo.lock
                                    $env:CARGO_TARGET_DIR = $targetDir
                                    cargo update --workspace
                                    if ($LASTEXITCODE -ne 0) { throw 'Cargo.lock workspace refresh failed.' }
                                    git add Cargo.lock
                                }
                            } else {
                                git merge --abort
                                throw ("Upstream $shortUp requires manual conflict resolution: " + ($conflicts -join ', '))
                            }
                        }
                        git commit -m "chore(sync): merge upstream $shortUp into Smart Design" | Out-Null
                        if ($LASTEXITCODE -ne 0) { throw 'Failed to create upstream merge commit.' }
                    } finally { Pop-Location }

                    Invoke-WorkspaceQualityGates $worktree $targetDir
                    $syncHead = (git -C $worktree rev-parse HEAD).Trim()
                    git -C $worktree push --set-upstream origin $syncBranch | Out-Null
                    if ($LASTEXITCODE -ne 0) { throw 'Failed to push upstream sync branch.' }
                    gh api --method POST ("repos/ahrrfy/designcraft/statuses/$syncHead") `
                        -f state='success' `
                        -f context='Windows quality gates' `
                        -f description="Verified automated upstream sync ($shortUp)" `
                        -f target_url="https://github.com/ahrrfy/designcraft/commit/$syncHead" | Out-Null
                    $prUrl = (gh pr create --repo ahrrfy/designcraft --base main --head $syncBranch --title "chore(sync): integrate upstream $shortUp" --body "Automated local integration of upstream storytold/designcraft commit $upstreamSha after full Windows quality gates.").Trim()
                    $merged = $false
                    for ($i = 0; $i -lt 5; $i++) {
                        gh pr merge $prUrl --repo ahrrfy/designcraft --merge --delete-branch *> $null
                        if ($LASTEXITCODE -eq 0) { $merged = $true; break }
                        Start-Sleep -Seconds 3
                    }
                    if (-not $merged) { throw "Could not auto-merge $prUrl into main." }
                    git -C $RepoPath worktree remove --force $worktree | Out-Null
                    git -C $RepoPath fetch --no-tags origin main
                    $originMain = (git -C $RepoPath rev-parse origin/main).Trim()
                }
            }

            if (-not (Test-Path -LiteralPath $worktree)) {
                git -C $RepoPath worktree add --detach $worktree $originMain | Out-Null
            }
            if ($action -eq 'publish_existing_main') {
                Invoke-WorkspaceQualityGates $worktree $targetDir
            }
            Publish-VerifiedCommitRelease $worktree $targetDir $craftFontsDir 'Verified Windows quality gates on main' | Out-Null
        } finally {
            if (Test-Path -LiteralPath $worktree) { git -C $RepoPath worktree remove --force $worktree *> $null }
            if (Test-Path -LiteralPath $targetDir) { Remove-Item -LiteralPath $targetDir -Recurse -Force -ErrorAction SilentlyContinue }
        }
    }
} finally {
    if ($null -ne $syncLock) { $syncLock.Dispose() }
}
& $updater -ManifestPath $ManifestPath
