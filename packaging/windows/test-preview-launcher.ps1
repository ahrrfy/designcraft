$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$taskArabic = [string][char]0x0628 + [char]0x0631 + [char]0x0646 + [char]0x0627 + [char]0x0645 + [char]0x062C
$taskFixture = Join-Path (Join-Path $taskRoot 'outputs/preview-launcher-tests') ($taskArabic + '-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $taskFixture -Force | Out-Null
$taskProfile = Join-Path $taskFixture 'profile'
New-Item -ItemType Directory -Path $taskProfile | Out-Null
$taskBook = Join-Path $taskFixture ($taskArabic + '.designcraft')
$taskExe = Join-Path $taskFixture 'fixture.exe'
[System.IO.File]::WriteAllText($taskBook, 'synthetic fixture')
[System.IO.File]::WriteAllText($taskExe, 'never executed: validation only')
$taskManifest = Join-Path $taskFixture 'manifest.json'
$taskRecord = @{profile=$taskProfile;document_path=$taskBook;executable=$taskExe;port=65335;executable_sha256=(Get-FileHash -LiteralPath $taskExe -Algorithm SHA256).Hash.ToLowerInvariant()}
foreach ($taskBom in @($false, $true)) {
    [System.IO.File]::WriteAllText($taskManifest, ($taskRecord | ConvertTo-Json), [System.Text.UTF8Encoding]::new($taskBom))
    $taskHashBefore = (Get-FileHash -LiteralPath $taskManifest).Hash
    $taskResult = & (Join-Path $PSScriptRoot 'start-native-preview.ps1') -ManifestPath $taskManifest -ValidateOnly
    if ($taskResult.profile -ne $taskProfile -or $taskResult.document -ne $taskBook) { throw 'Unicode paths changed during validation.' }
    if ((Get-FileHash -LiteralPath $taskManifest).Hash -ne $taskHashBefore) { throw 'Validation modified the manifest.' }
}
foreach ($taskField in @('profile', 'document_path', 'executable')) {
    $taskBad = $taskRecord.Clone()
    $taskBad[$taskField] = Join-Path $taskFixture 'missing'
    [System.IO.File]::WriteAllText($taskManifest, ($taskBad | ConvertTo-Json), [System.Text.UTF8Encoding]::new($false))
    $taskRejected = $false
    try { & (Join-Path $PSScriptRoot 'start-native-preview.ps1') -ManifestPath $taskManifest -ValidateOnly | Out-Null } catch { $taskRejected = $true }
    if (-not $taskRejected) { throw "Missing $taskField was accepted." }
}
'PASS: UTF-8 with/without BOM, Arabic paths, read-only validation and missing-path rejection; no application or provider requests started.'
