param(
    [Parameter(Mandatory = $true)][string]$ManifestPath,
    [string]$ExecutablePath,
    [switch]$ValidateOnly,
    [switch]$Hidden
)
$ErrorActionPreference = 'Stop'

# Windows PowerShell 5 otherwise decodes UTF-8 JSON using the legacy code page.
if ((Get-Item -LiteralPath $ManifestPath).Length -gt 1048576) { throw 'Preview manifest exceeds its size limit.' }
$taskRecord = Get-Content -LiteralPath $ManifestPath -Raw -Encoding UTF8 | ConvertFrom-Json
$taskExecutable = if ($ExecutablePath) { $ExecutablePath } else { $taskRecord.executable }
$taskBook = if ($taskRecord.document_path) { $taskRecord.document_path } else { $taskRecord.repaired_native_book }
if (-not [System.IO.Path]::IsPathRooted($taskRecord.profile) -or -not (Test-Path -LiteralPath $taskRecord.profile -PathType Container)) {
    throw 'The existing AI profile was not found. Startup stopped to preserve its settings and protected keys.'
}
if (-not [System.IO.Path]::IsPathRooted($taskExecutable) -or -not (Test-Path -LiteralPath $taskExecutable -PathType Leaf)) {
    throw 'Native preview executable was not found.'
}
if ($taskBook -and (-not [System.IO.Path]::IsPathRooted($taskBook) -or -not (Test-Path -LiteralPath $taskBook -PathType Leaf))) {
    throw 'The saved native document was not found. Startup stopped instead of opening an unrelated blank document.'
}
$taskPort = 0
if (-not [int]::TryParse([string]$taskRecord.port, [ref]$taskPort) -or $taskPort -lt 1 -or $taskPort -gt 65535) { throw 'Invalid preview control port.' }
$taskHash = (Get-FileHash -LiteralPath $taskExecutable -Algorithm SHA256).Hash.ToLowerInvariant()
if ($taskRecord.executable_sha256 -and $taskHash -ne $taskRecord.executable_sha256) { throw 'Native preview executable does not match its recorded SHA-256.' }
if ($ValidateOnly) {
    [pscustomobject]@{profile=$taskRecord.profile;document=$taskBook;executable=$taskExecutable;port=$taskPort;validation_only=$true}
    return
}
if (Get-NetTCPConnection -LocalPort $taskPort -State Listen -ErrorAction SilentlyContinue) {
    throw 'Smart Design is already open. Finish reviewing, save your work and close that window before starting the updated preview.'
}
$env:DESIGNCRAFT_CONFIG_DIR = $taskRecord.profile
$taskArguments = @('--control', [string]$taskPort)
if ($taskBook) { $taskArguments += ('"' + $taskBook + '"') }
# Visible only for the user-initiated interactive launcher; automated probes use -Hidden.
$taskStyle = if ($Hidden) { 'Hidden' } else { 'Normal' }
$taskProcess = Start-Process -FilePath $taskExecutable -ArgumentList $taskArguments -WindowStyle $taskStyle -PassThru
if ($null -eq $taskProcess) { throw 'Could not start the native preview.' }
$taskFields = @{
    app_pid=$taskProcess.Id;executable=$taskExecutable;executable_sha256=$taskHash;
    status='updated_native_preview_started';profile_restore_pending=$false;
    app_start_utc=$taskProcess.StartTime.ToUniversalTime().ToString('o')
}
foreach ($taskField in $taskFields.GetEnumerator()) { $taskRecord | Add-Member -NotePropertyName $taskField.Key -NotePropertyValue $taskField.Value -Force }
$taskJson = $taskRecord | ConvertTo-Json -Depth 20
[System.IO.File]::WriteAllText($ManifestPath, $taskJson, [System.Text.UTF8Encoding]::new($false))
$taskProcess
