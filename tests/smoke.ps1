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
$dotname = "dot.name.$PID"
$out = & $Exe $dotname -d
Check ($LASTEXITCODE -eq 0) "session names may contain periods"
& $Exe kill $dotname | Out-Null

# --- 2. it shows up in ls ---------------------------------------------------
$ls = (& $Exe ls) -join "`n"
Check ($ls -match [regex]::Escape($name)) "fx ls lists the session"
# Bare fx lists sessions too (1.2.0: it no longer attaches 'main').
Check (((& $Exe) -join "`n") -match [regex]::Escape($name)) "bare fx lists sessions"

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

# No attach banner anymore: liveness = session output starts flowing.
Wait-RawQuiet $r 2000 30000    # let the shell reach its prompt
Check ($r.Out.Length -gt 200) "interactive client attached (session output flowing)"

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
# Switching is silent, so verify via the CLIENTS column.
function Get-Clients([string]$SessName) {
    $rows = (& $Exe ls) -join "`n"
    if ($rows -match "(?m)^$([regex]::Escape($SessName))\s+\d+\s+(\d+)\s") { return [int]$Matches[1] }
    return -1
}
$swname = "swtest$PID"
Wait-RawQuiet $r 800 5000
Send-Str "& '$Exe' $swname`r"
$deadline = [DateTime]::UtcNow.AddSeconds(25)
while ((Get-Clients $swname) -ne 1 -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 300 }
Check ((Get-Clients $swname) -eq 1) "fx <name> in-session switches the client to '$swname'"
Wait-RawQuiet $r 2500 30000      # let the switched-to shell reach its prompt
Send-Str "echo sw-marker-$PID`r"
Check (Wait-Output $r "sw-marker-$PID" 10) "input lands in the switched-to session"

# Cycle keys: Alt+. hops to a neighboring session, Alt+, hops back. Over
# ssh an Alt chord arrives as ESC + the char in one write; conhost turns
# that into a single Alt-flagged record. Verified via the CLIENTS column:
# our one client leaves swtest (count 0) and returns (count 1). Which
# neighbor it visits doesn't matter — the user may have real sessions
# running.
Wait-RawQuiet $r 800 5000
Send-Raw ([byte[]]@(0x1B, 0x2E))     # Alt+. as ESC-prefix bytes — the ssh form
$deadline = [DateTime]::UtcNow.AddSeconds(10)
while ((Get-Clients $swname) -ne 0 -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 300 }
Check ((Get-Clients $swname) -eq 0) "Alt+. (ESC-prefix bytes) cycles to the next session"
Start-Sleep -Milliseconds 800
Send-Raw ([byte[]]@(0x1B, 0x2C))     # Alt+, as ESC-prefix bytes — the ssh form
$deadline = [DateTime]::UtcNow.AddSeconds(10)
while ((Get-Clients $swname) -ne 1 -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 300 }
Check ((Get-Clients $swname) -eq 1) "Alt+, (ESC-prefix bytes) cycles back to the previous session"

# vim safety: Esc followed by ',' at human speed must NOT cycle — the two
# bytes arrive in separate reads, so conhost emits two plain records and
# both forward to the session as keystrokes. ORDERING MATTERS: this must
# run BEFORE any win32-input-mode sequence (ESC[..._) goes through this
# conhost — seeing one permanently flips its parser into a regime where a
# lone ESC latches forever and fuses with the next byte (win32-input
# terminals never send bare ESC). The ssh path never carries win32
# sequences, so the pure-VT regime is the faithful model. A real cycle
# would move our client off swtest durably, so poll for a definitive 0;
# single-shot reads would turn transient `fx ls` hiccups into false FAILs.
Send-Raw (, [byte]0x1B)
Start-Sleep -Milliseconds 400
Send-Raw (, [byte]0x2C)
$evac = $false
$deadline = [DateTime]::UtcNow.AddSeconds(3)
while ([DateTime]::UtcNow -lt $deadline) {
    if ((Get-Clients $swname) -eq 0) { $evac = $true; break }
    Start-Sleep -Milliseconds 250
}
Check (-not $evac) "Esc-then-comma (separate writes) does not cycle"
Send-Raw (, [byte]3)         # Ctrl+C clears the ',' typed at the prompt
Wait-RawQuiet $r 800 5000

Start-Sleep -Milliseconds 800
Send-Raw ([byte[]]@(0x1B, 0x2E))     # forward again...
$deadline = [DateTime]::UtcNow.AddSeconds(10)
while ((Get-Clients $swname) -ne 0 -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 300 }
Start-Sleep -Milliseconds 800
# Alt+, as a full-fidelity local record with uc=0 (vk 188 ',', LEFT_ALT) —
# exercises the VK fallback branch, not the char match. NB: this is the
# first win32-input-mode sequence through this conhost, and it flips the
# conhost's lone-Esc heuristic for good — plain-VT Esc-timing tests must
# stay ABOVE this line.
Send-Str "$([char]27)[188;51;0;1;2;1_$([char]27)[188;51;0;0;2;1_"
$deadline = [DateTime]::UtcNow.AddSeconds(10)
while ((Get-Clients $swname) -ne 1 -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 300 }
Check ((Get-Clients $swname) -eq 1) "Alt+, (local chord) cycles back to the previous session"
Wait-RawQuiet $r 1500 15000

# Alt+/ (ESC-prefix bytes) opens the name prompt; a blank Enter is
# ignored, then a typed name + Enter creates that session and switches to
# it silently (no banner). Prove the switch by running a marker command
# that must land in the NEW session's buffer (verified at the end via its
# pipe).
Send-Raw ([byte[]]@(0x1B, 0x2F))
Send-Str "`r"                # blank entry must be ignored
Send-Str "kbsess$PID`r"
$deadline = [DateTime]::UtcNow.AddSeconds(15)
$created = $false
while (-not $created -and [DateTime]::UtcNow -lt $deadline) {
    Start-Sleep -Milliseconds 300
    if (((& $Exe ls) -join "`n") -match "(?m)^kbsess$PID\s") { $created = $true }
}
Check $created "Alt+/ prompt creates the named session"
Wait-RawQuiet $r 2500 30000  # let the new session's shell reach its prompt
$mark = $r.Out.Length
Send-Str "echo kbland-$PID`r"
$deadline = [DateTime]::UtcNow.AddSeconds(10)
$landed = $false
while (-not $landed -and [DateTime]::UtcNow -lt $deadline -and -not $r.Closed) {
    Pump-Raw $r 300
    if ($r.Out.ToString().Substring($mark) -match "kbland-$PID") { $landed = $true }
}
Check $landed "client is attached and interactive after the silent switch"
Wait-RawQuiet $r 1000 10000

# Freed C0 byte: bare 0x1C (the old Ctrl+\ detach) must now pass through
# to the session as a keystroke instead of detaching.
Send-Raw (, [byte]0x1C)
Start-Sleep -Milliseconds 1500
Check (-not $ip.HasExited) "bare 0x1C no longer detaches (freed back to apps)"
Send-Raw (, [byte]3)         # clear the prompt line
Wait-RawQuiet $r 800 5000

# Detach via ESC+backslash — exactly how Alt+\ arrives over SSH from any
# OS/terminal.
Send-Raw ([byte[]]@(0x1B, 0x5C))
Check (Wait-Output $r 'detached' 8) "Alt+\ (ESC-prefix bytes) detaches (ssh path)"
Check ($ip.WaitForExit(8000)) "client process exited after detach"
$ls = (& $Exe ls) -join "`n"
Check ($ls -match [regex]::Escape($iname)) "session survives detach"

# Reattach and detach via the full-fidelity Alt+\ chord (local path).
$psi.Arguments = "__pty 110 30 `"$Exe`" $iname"
$ip2 = [System.Diagnostics.Process]::Start($psi)
$rin2 = $ip2.StandardInput.BaseStream
$r2 = New-RawReader $ip2.StandardOutput.BaseStream
Wait-RawQuiet $r2 1500 15000
Check ($r2.Out.Length -gt 200) "reattach via fx <name> works (replay received)"

# `fx ls` inside a session must highlight the current session in green.
$mark2 = $r2.Out.Length
$lsCmd = [Text.Encoding]::UTF8.GetBytes("& '$Exe' ls`r")
$rin2.Write($lsCmd, 0, $lsCmd.Length); $rin2.Flush()
$deadline = [DateTime]::UtcNow.AddSeconds(10)
$greenRow = $false
while (-not $greenRow -and [DateTime]::UtcNow -lt $deadline -and -not $r2.Closed) {
    Pump-Raw $r2 300
    if ($r2.Out.ToString().Substring($mark2) -match "$([char]27)\[32m$iname") { $greenRow = $true }
}
Check $greenRow "fx ls highlights the current session in green"
Wait-RawQuiet $r2 1000 8000
# Replay must not re-ask terminal queries (the terminal's stale answers
# would be typed into the shell, e.g. "27;3$y").
Check (-not ($r2.Out.ToString() -cmatch '\$p|\$y')) "no terminal queries replayed on reattach"
$chord = [Text.Encoding]::UTF8.GetBytes("$([char]27)[220;43;92;1;2;1_$([char]27)[220;43;92;0;2;1_")
$rin2.Write($chord, 0, $chord.Length); $rin2.Flush()
Check (Wait-Output $r2 'detached' 8) "Alt+\ chord detaches (local path)"
[void]$ip2.WaitForExit(8000)
& $Exe kill $iname | Out-Null

# The switch marker must live in the OTHER session's history, proving the
# echo ran there and the session survived our detach.
$c3 = New-PipeClient "flux.$sid.$swname"
Send-Frame $c3 2 ([byte[]]@(100, 0, 30, 0))
[void](Pump $c3 1500)
Check ($c3.Out.ToString() -match "sw-marker-$PID") "marker present in switched session's buffer"
$c3.Pipe.Dispose()

# The kbland marker must be in the prompt-created session's buffer, proving
# the silent switch really landed there.
$c5 = New-PipeClient "flux.$sid.kbsess$PID"
Send-Frame $c5 2 ([byte[]]@(100, 0, 30, 0))
[void](Pump $c5 1500)
Check ($c5.Out.ToString() -match "kbland-$PID") "marker present in prompt-created session's buffer"
$c5.Pipe.Dispose()

& $Exe kill $swname | Out-Null
& $Exe kill "kbsess$PID" | Out-Null

if ($failed) { Write-Host "`nSMOKE: FAIL"; exit 1 } else { Write-Host "`nSMOKE: PASS"; exit 0 }
