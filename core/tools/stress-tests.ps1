param(
    [string]$Filter = '',
    [ValidateRange(1, 10000)][int]$Iterations = 30,
    [switch]$Workspace,
    [ValidateRange(0, 256)][int]$TestThreads = 0
)
$ErrorActionPreference = 'Stop'
$core = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$repo = [IO.Path]::GetFullPath((Join-Path $core '..'))
if (!$env:CARGO_TARGET_DIR) {
    $env:CARGO_TARGET_DIR = Join-Path ([IO.Path]::GetTempPath()) 'glide-stress-target'
}
$target = [IO.Path]::GetFullPath($env:CARGO_TARGET_DIR)
if ($target.Equals($repo, [StringComparison]::OrdinalIgnoreCase) -or
    $target.StartsWith($repo + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'CARGO_TARGET_DIR must be outside the repository.'
}
$env:CARGO_TARGET_DIR = $target
$logs = Join-Path $target ('stress-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $logs -Force | Out-Null
$cargoArgs = @('test', '--offline', '--locked')
if ($Workspace) { $cargoArgs += '--workspace' } else { $cargoArgs += @('-p', 'glide-daemon', '--lib') }
if ($Filter) { $cargoArgs += $Filter }
$cargoArgs += @('--', '--nocapture')
if ($TestThreads) { $cargoArgs += "--test-threads=$TestThreads" }
$passed = 0
$failed = 0
Push-Location $core
try {
    for ($run = 1; $run -le $Iterations; $run++) {
        $log = Join-Path $logs "$run.log"
        # Windows PowerShell wraps native stderr as ErrorRecords; judge Cargo's exit code.
        $ErrorActionPreference = 'Continue'
        & cargo @cargoArgs > $log 2>&1
        $code = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
        if ($code -eq 0 -and (Select-String -Path $log -Pattern 'test result: ok\. [1-9][0-9]* passed' -Quiet)) {
            $passed++
            Write-Host "$run/$Iterations PASS"
        } else {
            $failed++
            Write-Host "$run/$Iterations FAIL (exit $code; $log)"
            Get-Content -LiteralPath $log -Tail 30
        }
    }
} finally { Pop-Location }
Write-Host "Tally: $passed passed, $failed failed, $Iterations runs. Logs: $logs"
if ($failed) { exit 1 }
exit 0
