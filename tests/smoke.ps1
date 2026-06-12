# End-to-end smoke test for flux: session lifecycle over the real named pipe.
# Usage: powershell -File tests\smoke.ps1 [-Exe path\to\fx.exe]
param([string]$Exe = "$PSScriptRoot\..\target\debug\fx.exe")
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\pipelib.ps1"
$name = "fxsmoke$PID"
$failed = $false

function Check($cond, $what) {
    if ($cond) { Write-Host "ok   - $what" }
    else { Write-Host "FAIL - $what"; $script:failed = $true }
}

# --- 0. version --------------------------------------------------------------
$v = & $Exe --version
Check ("$v" -match '^flux \d+\.\d+\.\d+$') "fx --version reports the version ($v)"

# --- 1. create a detached session ------------------------------------------
$out = & $Exe $name -d
Check ($LASTEXITCODE -eq 0) "fx $name -d (create detached)"
$out = & $Exe $name -d
Check ($LASTEXITCODE -eq 0 -and "$out" -match 'already running') "fx <name> -d is idempotent"

# --- 2. it shows up in ls ---------------------------------------------------
$ls = (& $Exe ls) -join "`n"
Check ($ls -match [regex]::Escape($name)) "fx ls lists the session"

# --- 3. attach over the pipe, run a command, see its output -----------------
$sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$c = New-PipeClient "flux.$sid.$name"
Send-Frame $c 2 ([byte[]]@(100, 0, 30, 0))   # C_RESIZE -> marks us attached

# Input typed before the first prompt can be dropped by the console host;
# wait for the shell to render and go quiet first.
Pump-Quiet $c 2000 30000
Check ($c.Out.Length -gt 100) "received shell boot output after attach ($($c.Out.Length) chars)"
# win32-input-mode / focus-mode requests must never escape to the user's
# terminal (a terminal honoring ?9001h sends keys/pastes as sequences that
# get corrupted at SSH chunk boundaries).
Check (-not ($c.Out.ToString() -match '\[\?9001|\[\?1004')) "input-mode requests filtered from output"

Send-Frame $c 1 ([Text.Encoding]::UTF8.GetBytes("echo flux-marco-$PID`r"))
$deadline = [DateTime]::UtcNow.AddSeconds(15)
$seen = $false
while (-not $seen -and [DateTime]::UtcNow -lt $deadline -and -not $c.Closed) {
    [void](Pump $c 500)
    if ($c.Out.ToString() -match "flux-marco-$PID") { $seen = $true }
}
Check $seen "shell executed command and produced output through ConPTY"

# --- 4. progressive output with no further input ----------------------------
$mark = $c.Out.Length
Send-Frame $c 1 ([Text.Encoding]::UTF8.GetBytes("1..3 | % { `"tick-`$_`"; Start-Sleep -m 300 }`r"))
$deadline = [DateTime]::UtcNow.AddSeconds(15)
$ticks = $false
while (-not $ticks -and [DateTime]::UtcNow -lt $deadline -and -not $c.Closed) {
    [void](Pump $c 500)
    if ($c.Out.ToString().Substring($mark) -match 'tick-3') { $ticks = $true }
}
Check $ticks "output streams without further input (tick-3 seen)"

# --- 5. info frame -----------------------------------------------------------
Send-Frame $c 5 $null                          # C_INFO
$deadline = [DateTime]::UtcNow.AddSeconds(8)
$info = $null
while ($null -eq $info -and [DateTime]::UtcNow -lt $deadline -and -not $c.Closed) {
    [void](Pump $c 500)
    foreach ($f in $c.Frames) {
        if ($f.Type -eq 4) { $info = [Text.Encoding]::UTF8.GetString($f.Payload) }
    }
}
Check ($null -ne $info -and $info.Split("`t").Length -ge 4) "info frame: $info"
$shellPid = if ($info) { [int]$info.Split("`t")[1] } else { 0 }

# --- 6. the shell is a real, live process -----------------------------------
$proc = if ($shellPid) { Get-Process -Id $shellPid -ErrorAction SilentlyContinue } else { $null }
Check ($null -ne $proc) "shell process (pid $shellPid) is alive and detached"

# --- 7. detach-all from a second connection ----------------------------------
$c2 = New-PipeClient "flux.$sid.$name"
Send-Frame $c2 3 $null                         # C_DETACH_ALL
$deadline = [DateTime]::UtcNow.AddSeconds(8)
$detached = $false
while (-not $detached -and [DateTime]::UtcNow -lt $deadline) {
    [void](Pump $c 500)
    foreach ($f in $c.Frames) { if ($f.Type -eq 3) { $detached = $true } }
    if ($c.Closed) { break }
}
Check $detached "attached client received S_DETACHED after fx-detach"
$c2.Pipe.Dispose()
$c.Pipe.Dispose()

# --- 8. kill ------------------------------------------------------------------
$out = & $Exe kill $name
Check ($LASTEXITCODE -eq 0) "fx kill $name ($out)"
Start-Sleep -Milliseconds 500
$proc = if ($shellPid) { Get-Process -Id $shellPid -ErrorAction SilentlyContinue } else { $null }
Check ($null -eq $proc) "shell process is gone after kill"
$ls = (& $Exe ls) -join "`n"
Check (-not ($ls -match [regex]::Escape($name))) "session no longer listed"

# --- 8b. sessions inherit the shell fx was launched from ---------------------
# fx invoked from cmd.exe must produce a cmd session ('ver' is cmd-only).
$cmdname = "cmdtest$PID"
$env:FXEXE = "$Exe"
cmd /c "`"%FXEXE%`" $cmdname -d" | Out-Null
$c4 = New-PipeClient "flux.$sid.$cmdname"
Send-Frame $c4 2 ([byte[]]@(100, 0, 30, 0))
Pump-Quiet $c4 1500 15000
Send-Frame $c4 1 ([Text.Encoding]::UTF8.GetBytes("ver`r"))
$deadline = [DateTime]::UtcNow.AddSeconds(8)
$isCmd = $false
while (-not $isCmd -and [DateTime]::UtcNow -lt $deadline -and -not $c4.Closed) {
    [void](Pump $c4 400)
    if ($c4.Out.ToString() -match 'Microsoft Windows \[Version') { $isCmd = $true }
}
Check $isCmd "session created from cmd runs cmd (shell inheritance)"
$c4.Pipe.Dispose()
& $Exe kill $cmdname | Out-Null

# --- 9. interactive client under a test ConPTY -------------------------------
# Drives the real interactive client (raw console, VT key forwarding)
# inside a ConPTY via the hidden `fx __pty` bridge.

function New-RawReader($Stream) {
    @{ S = $Stream; Buf = New-Object byte[] 32768; Pending = $null
       Out = New-Object Text.StringBuilder; Closed = $false }
}
function Pump-Raw($R, [int]$Ms) {
    if ($R.Closed) { return }
    $deadline = [DateTime]::UtcNow.AddMilliseconds($Ms)
    while ($true) {
        if ($null -eq $R.Pending) { $R.Pending = $R.S.ReadAsync($R.Buf, 0, $R.Buf.Length) }
        $remain = [int](($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        if ($remain -lt 1) { $remain = 1 }
        $done = $false
        try { $done = $R.Pending.Wait($remain) } catch { $R.Closed = $true; $R.Pending = $null; break }
        if ($done) {
            $n = $R.Pending.Result
            $R.Pending = $null
            if ($n -le 0) { $R.Closed = $true; break }
            [void]$R.Out.Append([Text.Encoding]::UTF8.GetString($R.Buf, 0, $n))
        }
        if ([DateTime]::UtcNow -ge $deadline) { break }
    }
}
function Wait-Output($R, [string]$Pattern, [int]$TimeoutSec) {
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSec)
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($R.Out.ToString() -match $Pattern) { return $true }
        if ($R.Closed) { break }
        Pump-Raw $R 300
    }
    return ($R.Out.ToString() -match $Pattern)
}
function Wait-RawQuiet($R, [int]$QuietMs, [int]$MaxMs) {
    $stop = [DateTime]::UtcNow.AddMilliseconds($MaxMs)
    $lastLen = $R.Out.Length
    $lastChange = [DateTime]::UtcNow
    while ([DateTime]::UtcNow -lt $stop -and -not $R.Closed) {
        Pump-Raw $R 250
        if ($R.Out.Length -ne $lastLen) { $lastLen = $R.Out.Length; $lastChange = [DateTime]::UtcNow }
        elseif (([DateTime]::UtcNow - $lastChange).TotalMilliseconds -ge $QuietMs) { break }
    }
}

$iname = "itest$PID"
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $Exe
# `fx <name>` with no subcommand must attach-or-create.
$psi.Arguments = "__pty 110 30 `"$Exe`" $iname"
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$ip = [System.Diagnostics.Process]::Start($psi)
$rin = $ip.StandardInput.BaseStream
$r = New-RawReader $ip.StandardOutput.BaseStream
function Send-Raw([byte[]]$b) { $rin.Write($b, 0, $b.Length); $rin.Flush() }
function Send-Str([string]$s) { Send-Raw ([Text.Encoding]::UTF8.GetBytes($s)) }

Check (Wait-Output $r 'attached to' 10) "interactive client attached"
Wait-RawQuiet $r 2000 30000    # let the shell reach its prompt

# Ctrl+C must interrupt a running command: a follow-up command can only
# execute within the window if the 30s sleep was actually aborted.
# (Pipeline output from finally blocks is discarded during a stop, so
# asserting on a finally side effect would be unreliable.)
Send-Str "Start-Sleep 30`r"
Start-Sleep -Milliseconds 1500
Send-Raw (, [byte]3)           # Ctrl+C
Start-Sleep -Milliseconds 300
Send-Str "echo ctrlc-ok-$PID`r"
Check (Wait-Output $r "ctrlc-ok-$PID" 10) "Ctrl+C interrupts a running command"

# Arrow keys: Up must recall history through the whole chain.
Send-Str "echo up-marker-$PID`r"
Check (Wait-Output $r "up-marker-$PID" 8) "command output visible interactively"
Wait-RawQuiet $r 800 5000
$before = ([regex]::Matches($r.Out.ToString(), "up-marker-$PID")).Count
Send-Str "$([char]27)[A`r"     # Up, Enter
$deadline = [DateTime]::UtcNow.AddSeconds(8)
$recalled = $false
while (-not $recalled -and [DateTime]::UtcNow -lt $deadline) {
    Pump-Raw $r 300
    if (([regex]::Matches($r.Out.ToString(), "up-marker-$PID")).Count -gt $before) { $recalled = $true }
}
Check $recalled "arrow-up recalls history (VK round-trip works)"

# Backspace must edit the line. Sent as a raw DEL byte — the form every ssh
# client emits — so this also guards the legacy-conhost (Win10) encoding.
Wait-RawQuiet $r 800 5000
Send-Str "echo bsp-okZZ"
Start-Sleep -Milliseconds 400
Send-Raw ([byte[]]@(0x7F, 0x7F))     # two backspaces erase 'ZZ'
Send-Str "K!`r"
Check (Wait-Output $r 'bsp-okK!' 10) "backspace edits the input line"

# Paste: a flood of plain characters must arrive intact. First as one big
# chunk, then dribbled in tiny chunks with delays (SSH packet boundaries).
Wait-RawQuiet $r 800 5000
$mark = $r.Out.Length
Send-Str "echo 'P@ste{Te}st-`$tr!'`r"
Check (Wait-Output $r 'P@ste\{Te\}st-\$tr!' 10) "paste (single chunk) arrives intact"

Wait-RawQuiet $r 800 5000
$mark = $r.Out.Length
$dribble = [Text.Encoding]::UTF8.GetBytes("echo 'Dr1bble-P@ste-OK!'")
for ($i = 0; $i -lt $dribble.Length; $i += 3) {
    $len = [Math]::Min(3, $dribble.Length - $i)
    $rin.Write($dribble, $i, $len)
    $rin.Flush()
    Start-Sleep -Milliseconds 25
}
Send-Str "`r"
Check (Wait-Output $r 'Dr1bble-P@ste-OK!' 10) "paste (dribbled chunks) arrives intact"
Check (-not ($r.Out.ToString().Substring($mark) -match ';\d+;\d+_')) "no key-sequence fragments typed as text"

# Switching: `fx <other>` typed INSIDE a session must swap this client over
# to the other session in place (no nesting, no stacked attachments).
$swname = "swtest$PID"
Wait-RawQuiet $r 800 5000
Send-Str "& '$Exe' $swname`r"
Check (Wait-Output $r "session '$swname'" 25) "client switched to '$swname' in place"
Wait-RawQuiet $r 2500 30000      # let the switched-to shell reach its prompt
Send-Str "echo sw-marker-$PID`r"
Check (Wait-Output $r "sw-marker-$PID" 10) "input lands in the switched-to session"

# Detach via the bare FS byte — exactly how Ctrl+\ / Ctrl+Shift+\ arrives
# over SSH from any OS/terminal (VT carries no modifier info).
Send-Raw (, [byte]0x1C)
Check (Wait-Output $r 'detached' 8) "Ctrl+\ as raw VT byte detaches (ssh path)"
Check ($ip.WaitForExit(8000)) "client process exited after detach"
$ls = (& $Exe ls) -join "`n"
Check ($ls -match [regex]::Escape($iname)) "session survives detach"

# Reattach and detach via the full-fidelity Ctrl+Shift+\ chord (local path).
$psi.Arguments = "__pty 110 30 `"$Exe`" $iname"
$ip2 = [System.Diagnostics.Process]::Start($psi)
$rin2 = $ip2.StandardInput.BaseStream
$r2 = New-RawReader $ip2.StandardOutput.BaseStream
Check (Wait-Output $r2 'attached to' 10) "reattach via fx <name> works"
Wait-RawQuiet $r2 1200 10000
# Replay must not re-ask terminal queries (the terminal's stale answers
# would be typed into the shell, e.g. "27;3$y").
Check (-not ($r2.Out.ToString() -cmatch '\$p|\$y')) "no terminal queries replayed on reattach"
$chord = [Text.Encoding]::UTF8.GetBytes("$([char]27)[220;43;28;1;24;1_$([char]27)[220;43;28;0;24;1_")
$rin2.Write($chord, 0, $chord.Length); $rin2.Flush()
Check (Wait-Output $r2 'detached' 8) "Ctrl+Shift+\ chord detaches (local path)"
[void]$ip2.WaitForExit(8000)
& $Exe kill $iname | Out-Null

# The switch marker must live in the OTHER session's history, proving the
# echo ran there and the session survived our detach.
$c3 = New-PipeClient "flux.$sid.$swname"
Send-Frame $c3 2 ([byte[]]@(100, 0, 30, 0))
[void](Pump $c3 1500)
Check ($c3.Out.ToString() -match "sw-marker-$PID") "marker present in switched session's buffer"
$c3.Pipe.Dispose()
& $Exe kill $swname | Out-Null

if ($failed) { Write-Host "`nSMOKE: FAIL"; exit 1 } else { Write-Host "`nSMOKE: PASS"; exit 0 }
