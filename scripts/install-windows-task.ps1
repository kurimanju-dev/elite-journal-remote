param(
    [Parameter(Mandatory=$true)][string]$Binary,
    [Parameter(Mandatory=$true)][string]$Connect,
    [Parameter(Mandatory=$true)][string]$KeyFile,
    [string]$Destination,
    [string]$TaskName = 'Elite Journal Relay'
)
$ErrorActionPreference = 'Stop'
$Binary = (Resolve-Path -LiteralPath $Binary).Path
$KeyFile = (Resolve-Path -LiteralPath $KeyFile).Path
function Quote-Argument([string]$Value) {
    if ($Value.Contains('"') -or $Value.Contains("`n") -or $Value.EndsWith('\')) { throw 'Unsupported quote, newline or trailing backslash in argument' }
    return '"' + $Value + '"'
}
$log = Join-Path (Split-Path -Parent $Binary) 'receiver.log'
$arguments = 'receive --connect ' + (Quote-Argument $Connect) + ' --key-file ' + (Quote-Argument $KeyFile) + ' --log-file ' + (Quote-Argument $log)
if ($Destination) { $arguments += ' --destination ' + (Quote-Argument $Destination) }
$user = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
$action = New-ScheduledTaskAction -Execute $Binary -Argument $arguments -WorkingDirectory (Split-Path -Parent $Binary)
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -StartWhenAvailable -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings | Out-Null
Write-Host "Installed task '$TaskName' for $user. Log: $log"
Write-Host "Start with: Start-ScheduledTask -TaskName '$TaskName'"
Write-Host "Remove with: Unregister-ScheduledTask -TaskName '$TaskName'"
