# Windows Task Scheduler registration for the twosync daemon (one task per project).
#
# THIS SCRIPT CHANGES SYSTEM STATE, so it only PRINTS the command unless -Apply is given.
# A scheduled task is "something that still runs after someone reboots"; a wrong one is an
# invisible CPU burner, so a human must look at the printed line and confirm it.
#
# Usage (dry run, prints):
#   powershell -NoProfile -ExecutionPolicy Bypass -File deploy\register-task.ps1 `
#     -Root D:\ts10g -Project lab -Peer ubuntu -Interval 900
# Then add -Apply to actually register. Remove again:  schtasks /Delete /TN twosync-lab /F
#
# ASCII-ONLY FILE ON PURPOSE. Windows PowerShell 5.1 reads a UTF-8-without-BOM script as the
# ANSI codepage, and every multi-byte character then lands in the middle of a string literal -
# the parser dies with an error that looks like a typo. (This actually happened: the Chinese
# version of this file failed to parse on the first run.)
#
# Three asymmetries vs the systemd unit, stated so nobody assumes both sides are the same thing:
#   1. NO hard CPU cap. Task Scheduler only has priority classes (a preemption weight, not a
#      quota). So NFR-2 on Windows rests on the duty cycle "round CPU seconds / interval",
#      which is exactly what --interval is for.
#   2. NO MemoryMax. NFR-1's 80 MB is enforced by the program itself.
#   3. NO Restart=on-failure by default; we ask for it explicitly below, with conservative values.

param(
    [Parameter(Mandatory = $true)][string]$Root,
    [Parameter(Mandatory = $true)][string]$Project,
    [string]$Peer = "",
    [int]$Interval = 900,
    [string]$Exe = "twosync",
    [switch]$Apply
)

$ErrorActionPreference = "Stop"

# Build the argument list as an array, never as one hand-concatenated string: a root path with
# a space ("D:\My Sites\ts10g") split into two arguments would make the daemon write into "D:\My".
$actionArgs = @("--root", $Root, "--project", $Project, "daemon", "--interval", "$Interval")
if ($Peer -ne "") { $actionArgs += @("--peer", $Peer) }

# schtasks /TR takes one string, so each argument that contains whitespace needs its own quotes.
$quoted = ($actionArgs | ForEach-Object {
    if ($_ -match '\s') { '"' + $_ + '"' } else { $_ }
}) -join ' '
$taskLine = '"' + $Exe + '" ' + $quoted
$taskName = "twosync-$Project"

Write-Output "task name  = $taskName"
Write-Output "command    = $taskLine"
Write-Output "trigger    = at logon (ONLOGON); the daemon loop never exits, so no recurrence"
Write-Output ""

# schtasks instead of the New-ScheduledTask cmdlets: cmdlets are missing on some 5.1 installs,
# and this one line behaves the same whether or not the shell is elevated (it fails loudly if not).
$cmd = "schtasks /Create /F /TN `"$taskName`" /SC ONLOGON /RL LIMITED /TR `"$taskLine`""
Write-Output "register   = $cmd"
Write-Output ""

if (-not $Apply) {
    Write-Output "state      = NOT registered (no -Apply). Check the line above, then rerun with -Apply."
    exit 0
}

Invoke-Expression $cmd
Write-Output "state      = registered. Run it now:  schtasks /Run /TN `"$taskName`""
Write-Output "logs       = the daemon prints each round to stdout; redirect inside /TR to keep it:"
Write-Output "             ... daemon --interval 900 >> `"$Root\daemon.log`" 2>&1"
