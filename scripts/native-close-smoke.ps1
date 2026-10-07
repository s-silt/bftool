param([switch]$Idle, [string]$ExePath)
$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
$mode = if($Idle){'idle'}else{'active'}
$env:BFTOOL_TEST_PROFILE = "$repo/.build/native-profile-close-$mode"
$env:BFTOOL_TEST_CDP_PORT = '9432'
$exe = if ($ExePath) { (Resolve-Path -LiteralPath $ExePath).Path } else { "$repo/apps/desktop/src-tauri/target/debug/bftool-desktop.exe" }
if (-not (Test-Path -LiteralPath $exe)) { throw "Build a debug custom-protocol executable first, or pass -ExePath." }
# Keep the Process object from Start-Process so ExitCode is available after exit.
$process = Start-Process -FilePath $exe -WorkingDirectory $repo -WindowStyle Hidden -RedirectStandardOutput "$repo/.build/close-$mode-stdout.log" -RedirectStandardError "$repo/.build/close-$mode-stderr.log" -PassThru
try {
    if($Idle) {
        for($attempt = 0; $attempt -lt 100; $attempt++) {
            $process.Refresh()
            if($process.MainWindowHandle -ne 0){break}
            Start-Sleep -Milliseconds 100
        }
    } else {
        Push-Location $repo
        try { & node "$PSScriptRoot/native-close-check.mjs"; if($LASTEXITCODE -ne 0){throw 'Native close preparation failed'} }
        finally { Pop-Location }
    }
    if(-not $process.CloseMainWindow()){throw 'Native close request failed'}
    if(-not $process.WaitForExit(30000)){throw 'Candidate did not close within 30 seconds; process left for inspection'}
    $exitCode = $process.ExitCode
    if($exitCode -ne 0){throw "Candidate exited with code $exitCode"}
    if($Idle) {
        $evidence = [pscustomobject]@{ mode='idle'; exited=$true; exitCode=$exitCode }
    } else {
        $evidence = Get-Content -LiteralPath "$repo/.build/native-close-evidence.json" -Raw | ConvertFrom-Json
        if((Get-Item -LiteralPath $evidence.source).Length -ne $evidence.sourceSize){throw 'Source changed'}
        if(Test-Path -LiteralPath (Join-Path $evidence.target $evidence.destination)){throw 'Cancelled payload was published'}
        $metadataFiles = @(Get-ChildItem -LiteralPath $evidence.target -Recurse -File | ForEach-Object { $_.FullName })
        $evidence | Add-Member mode $mode
        $evidence | Add-Member exited $true
        $evidence | Add-Member exitCode $exitCode
        $evidence | Add-Member sourcePreserved $true
        $evidence | Add-Member payloadPublished $false
        $evidence | Add-Member metadataFiles $metadataFiles
    }
    $evidence | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath "$repo/.build/close-$mode-evidence.json" -Encoding utf8
    "Close $mode exit code: $exitCode"
} finally {
    $process.Dispose()
}
