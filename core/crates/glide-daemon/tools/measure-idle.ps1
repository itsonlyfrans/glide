param(
    [Parameter(Mandatory = $true)][string]$Daemon,
    [Parameter(Mandatory = $true)][string]$DataDir,
    [ValidateRange(5, 60)][int]$Seconds = 10,
    [ValidateSet('mock-platform', 'mock-backends')][string]$Mode = 'mock-platform'
)
$ErrorActionPreference = 'Stop'
$exe = (Resolve-Path -LiteralPath $Daemon).Path
$data = [IO.Path]::GetFullPath($DataDir)
New-Item -ItemType Directory -Force -Path $data | Out-Null
$stdout = Join-Path $data 'idle-stdout.jsonl'
$stderr = Join-Path $data 'idle-stderr.txt'
$arguments = @('--headless', '--data-dir', ('"' + $data + '"'))
if ($Mode -eq 'mock-platform') {
    $probe = [System.Net.Sockets.UdpClient]::new(0)
    $port = [int]$probe.Client.LocalEndPoint.Port
    $probe.Dispose()
    $arguments += @('--mock-platform', '--no-discovery', '--port', [string]$port)
} else {
    $arguments += '--mock-backends'
}
$child = Start-Process -FilePath $exe -WindowStyle Hidden -PassThru -ArgumentList $arguments `
    -RedirectStandardOutput $stdout -RedirectStandardError $stderr
try {
    $readyDeadline = [DateTime]::UtcNow.AddSeconds(10)
    $ready = $false
    while ([DateTime]::UtcNow -lt $readyDeadline) {
        $child.Refresh()
        if ($child.HasExited) { throw 'Daemon exited before emitting ready; startup or OS-keystore initialization failed.' }
        if (Test-Path -LiteralPath $stdout) {
            $first = Get-Content -LiteralPath $stdout -TotalCount 1 -ErrorAction SilentlyContinue
            if ($first) {
                try { $ready = (($first | ConvertFrom-Json).event -eq 'ready') } catch { $ready = $false }
                break
            }
        }
        Start-Sleep -Milliseconds 50
    }
    if (!$ready) { throw 'Daemon did not emit ready within 10 seconds.' }
    Start-Sleep -Seconds 1
    $child.Refresh()
    if ($child.HasExited) { throw 'Daemon exited during warmup.' }
    $cpuBefore = $child.TotalProcessorTime.TotalMilliseconds
    $timer = [Diagnostics.Stopwatch]::StartNew()
    Start-Sleep -Seconds $Seconds
    $child.Refresh()
    if ($child.HasExited) { throw 'Daemon exited during measurement.' }
    $cpuMs = $child.TotalProcessorTime.TotalMilliseconds - $cpuBefore
    [PSCustomObject]@{
        mode = $Mode
        warmup_s = 1
        elapsed_s = $timer.Elapsed.TotalSeconds
        cpu_ms = $cpuMs
        cpu_one_core_percent = 100 * $cpuMs / $timer.Elapsed.TotalMilliseconds
        cpu_all_cores_percent = 100 * $cpuMs / $timer.Elapsed.TotalMilliseconds / [Environment]::ProcessorCount
        working_set_mib = $child.WorkingSet64 / 1MB
        private_bytes_mib = $child.PrivateMemorySize64 / 1MB
    } | ConvertTo-Json
} finally {
    $child.Refresh()
    if (!$child.HasExited) { Stop-Process -InputObject $child }
    $child.WaitForExit()
}
