# Shared pipe-client engine for flux tests. Dot-source this file.
# Two crucial details for .NET Framework pipes:
# - PipeOptions.Asynchronous: a non-overlapped handle serializes I/O, so a
#   pending read blocks concurrent writes on the same handle (deadlock when
#   the pipe is quiet).
# - One persistent ReadAsync, never abandoned: an abandoned timed-out read
#   would steal bytes from later reads.

function New-PipeClient([string]$PipeName) {
    $p = New-Object System.IO.Pipes.NamedPipeClientStream('.', $PipeName, [System.IO.Pipes.PipeDirection]::InOut, [System.IO.Pipes.PipeOptions]::Asynchronous)
    $p.Connect(5000)
    @{
        Pipe    = $p
        Buf     = New-Object byte[] 65536
        Pending = $null
        Bytes   = New-Object 'System.Collections.Generic.List[byte]'
        Out     = New-Object Text.StringBuilder
        Frames  = New-Object 'System.Collections.Generic.List[object]'
        Closed  = $false
    }
}

function Send-Frame($C, [byte]$Type, [byte[]]$Payload) {
    if ($null -eq $Payload) { $Payload = @() }
    $len = [BitConverter]::GetBytes([uint32]$Payload.Length)
    $buf = @([byte]$Type) + $len + $Payload
    $C.Pipe.Write([byte[]]$buf, 0, $buf.Length)
    $C.Pipe.Flush()
}

function Send-Text($C, [string]$s) {
    Send-Frame $C 1 ([Text.Encoding]::UTF8.GetBytes($s))
}

function Parse-Frames($C) {
    while ($C.Bytes.Count -ge 5) {
        $len = [BitConverter]::ToUInt32($C.Bytes.ToArray(), 1)
        if ($C.Bytes.Count -lt 5 + $len) { break }
        $ty = $C.Bytes[0]
        $payload = New-Object byte[] $len
        if ($len -gt 0) { $C.Bytes.CopyTo(5, $payload, 0, [int]$len) }
        $C.Bytes.RemoveRange(0, 5 + [int]$len)
        if ($ty -eq 1) { [void]$C.Out.Append([Text.Encoding]::UTF8.GetString($payload)) }
        else { $C.Frames.Add(@{ Type = $ty; Payload = $payload }) }
    }
}

function Pump($C, [int]$Ms) {
    if ($C.Closed) { return $false }
    $deadline = [DateTime]::UtcNow.AddMilliseconds($Ms)
    $got = $false
    while ($true) {
        if ($null -eq $C.Pending) { $C.Pending = $C.Pipe.ReadAsync($C.Buf, 0, $C.Buf.Length) }
        $remain = [int](($deadline - [DateTime]::UtcNow).TotalMilliseconds)
        if ($remain -lt 1) { $remain = 1 }
        $done = $false
        try { $done = $C.Pending.Wait($remain) } catch { $C.Closed = $true; $C.Pending = $null; break }
        if ($done) {
            $n = $C.Pending.Result
            $C.Pending = $null
            if ($n -le 0) { $C.Closed = $true; break }
            for ($i = 0; $i -lt $n; $i++) { $C.Bytes.Add($C.Buf[$i]) }
            Parse-Frames $C
            $got = $true
        }
        if ([DateTime]::UtcNow -ge $deadline) { break }
    }
    return $got
}

function Pump-Quiet($C, [int]$QuietMs, [int]$MaxMs) {
    $stop = [DateTime]::UtcNow.AddMilliseconds($MaxMs)
    $lastLen = $C.Out.Length
    $lastChange = [DateTime]::UtcNow
    while ([DateTime]::UtcNow -lt $stop) {
        [void](Pump $C 250)
        if ($C.Out.Length -ne $lastLen) { $lastLen = $C.Out.Length; $lastChange = [DateTime]::UtcNow }
        elseif (([DateTime]::UtcNow - $lastChange).TotalMilliseconds -ge $QuietMs) { break }
        if ($C.Closed) { break }
    }
}
