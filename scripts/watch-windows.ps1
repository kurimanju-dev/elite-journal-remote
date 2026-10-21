param([Parameter(Mandatory=$true)][string]$Directory)
$ErrorActionPreference = 'Stop'
$watcher = [System.IO.FileSystemWatcher]::new((Resolve-Path -LiteralPath $Directory).Path)
$watcher.NotifyFilter = [System.IO.NotifyFilters]::LastWrite -bor [System.IO.NotifyFilters]::Size -bor [System.IO.NotifyFilters]::FileName
$watcher.Filter = '*'
$subscriptions = @()
foreach ($name in @('Changed', 'Created', 'Renamed', 'Deleted', 'Error')) {
    $subscriptions += Register-ObjectEvent -InputObject $watcher -EventName $name -Action {
        $e = $Event.SourceEventArgs
        Write-Host ('{0:O} {1} {2}' -f [DateTime]::UtcNow, $Event.SourceEventArgs.ChangeType, $e.FullPath)
    }
}
$watcher.EnableRaisingEvents = $true
Write-Host "Watching native Windows events in $Directory. Ctrl+C to stop."
try { while ($true) { Start-Sleep -Milliseconds 100 } }
finally {
    $watcher.Dispose()
    foreach ($s in $subscriptions) { Unregister-Event -SourceIdentifier $s.Name -ErrorAction SilentlyContinue; Remove-Job -Id $s.Id -Force -ErrorAction SilentlyContinue }
}
