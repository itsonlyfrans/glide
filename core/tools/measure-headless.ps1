param(
    [Parameter(Mandatory=$true)][string]$Executable,
    [ValidateSet('native','mock-platform','mock-backends')][string]$Mode = 'native',
    [int]$IdleSeconds = 30,
    [int]$SampleSeconds = 10
)
$ErrorActionPreference = 'Stop'
if ($IdleSeconds -lt 30 -or $SampleSeconds -lt 1) { throw 'Use at least 30 seconds idle and one second sample.' }
$enginePath = (Resolve-Path -LiteralPath $Executable).Path
$taskDirectory = Join-Path ([IO.Path]::GetTempPath()) ('glide-headless-measure-' + [guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($taskDirectory) | Out-Null
$stderrPath = Join-Path $taskDirectory 'stderr.log'
$stdoutPath = Join-Path $taskDirectory 'stdout.log'
$process = $null
$pipe = $null
try {
    $arguments = '--headless --data-dir "' + $taskDirectory + '"'
    if ($Mode -ne 'native') { $arguments += ' --' + $Mode }
    $process = Start-Process -FilePath $enginePath -ArgumentList $arguments -PassThru -WindowStyle Hidden -RedirectStandardError $stderrPath -RedirectStandardOutput $stdoutPath
    $metadataPath = Join-Path $taskDirectory 'ipc.json'
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    while (!(Test-Path -LiteralPath $metadataPath)) {
        if ($process.HasExited -or [DateTime]::UtcNow -gt $deadline) { throw 'Engine did not publish control metadata; inspect local stderr.log. No footprint was measured.' }
        Start-Sleep -Milliseconds 50
    }
    Start-Sleep -Seconds $IdleSeconds
    if ($process.HasExited) { throw 'Engine exited during idle warmup.' }
    if ((Get-Content -LiteralPath $stderrPath -Raw) -match 'native tray unavailable') { throw 'Tray creation failed; cannot report a with-tray footprint.' }
    $process.Refresh()
    $cpuBefore = $process.TotalProcessorTime.TotalMilliseconds
    $clock = [Diagnostics.Stopwatch]::StartNew()
    Start-Sleep -Seconds $SampleSeconds
    $process.Refresh()
    $elapsed = $clock.Elapsed.TotalSeconds
    $cpuMilliseconds = $process.TotalProcessorTime.TotalMilliseconds - $cpuBefore
    $measurement = [ordered]@{
        mode = $Mode
        pid = $process.Id
        idle_seconds = $IdleSeconds
        sample_seconds = [Math]::Round($elapsed, 3)
        working_set_mib = [Math]::Round($process.WorkingSet64 / 1MB, 2)
        private_bytes_mib = [Math]::Round($process.PrivateMemorySize64 / 1MB, 2)
        cpu_ms = [Math]::Round($cpuMilliseconds, 3)
        cpu_percent_one_core = [Math]::Round($cpuMilliseconds / ($elapsed * 10), 4)
        cpu_percent_host = [Math]::Round($cpuMilliseconds / ($elapsed * 10 * [Environment]::ProcessorCount), 4)
        tray_created = $true
    }
    $metadata = Get-Content -LiteralPath $metadataPath -Raw | ConvertFrom-Json
    $pipeName = $metadata.endpoint.Substring('\\.\pipe\'.Length)
    $pipe = New-Object IO.Pipes.NamedPipeClientStream('.', $pipeName, [IO.Pipes.PipeDirection]::InOut, [IO.Pipes.PipeOptions]::Asynchronous)
    $pipe.Connect(2000)
    $utf8 = New-Object Text.UTF8Encoding($false)
    $writer = New-Object IO.StreamWriter($pipe, $utf8, 1024, $true)
    $reader = New-Object IO.StreamReader($pipe, $utf8, $false, 1024, $true)
    $writer.AutoFlush = $true
    $writer.WriteLine((@{auth=$metadata.token} | ConvertTo-Json -Compress))
    $auth = $reader.ReadLineAsync()
    if (!$auth.Wait(2000) -or (($auth.Result | ConvertFrom-Json).auth -ne 'ok')) { throw 'Control authentication failed.' }
    $shutdownClock = [Diagnostics.Stopwatch]::StartNew()
    $writer.WriteLine('{"id":1,"method":"app.shutdown","params":{}}')
    do {
        $reply = $reader.ReadLineAsync()
        if (!$reply.Wait(1500) -or !$reply.Result) { throw 'Missing shutdown response.' }
        $frame = $reply.Result | ConvertFrom-Json
    } while ($frame.id -ne 1)
    $remainingMilliseconds = [Math]::Max(0, 2000 - [int]$shutdownClock.ElapsedMilliseconds)
    if (!$frame.ok -or !$process.WaitForExit($remainingMilliseconds) -or $shutdownClock.ElapsedMilliseconds -gt 2000) { throw 'Shutdown failed or exceeded its exit deadline.' }
    $measurement.shutdown_ms = [Math]::Round($shutdownClock.Elapsed.TotalMilliseconds, 1)
    $measurement | ConvertTo-Json
} finally {
    if ($pipe) { $pipe.Dispose() }
    if ($process -and !$process.HasExited) { $process.Kill(); $process.WaitForExit() }
    # Resolve and check the exact temporary target before any recursive cleanup.
    $resolvedTask = [IO.Path]::GetFullPath($taskDirectory)
    $temporaryRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
    if (!$resolvedTask.StartsWith($temporaryRoot, [StringComparison]::OrdinalIgnoreCase) -or !(Split-Path $resolvedTask -Leaf).StartsWith('glide-headless-measure-')) { throw 'Unsafe temporary cleanup path.' }
    Remove-Item -LiteralPath $resolvedTask -Recurse -Force
}
