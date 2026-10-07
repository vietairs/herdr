# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=codex
# HERDR_INTEGRATION_VERSION=9

param([string]$Action = "")

if ($Action -ne "session" -and $Action -ne "usage") { exit 0 }
if ($env:HERDR_ENV -ne "1") { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:HERDR_PANE_ID)) { exit 0 }

$inputText = [Console]::In.ReadToEnd()
try {
    $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json }
} catch {
    exit 0
}

# Session reports keep tolerating a payload without an event name; usage
# reports only run from the Stop hook.
if ($Action -eq "usage") {
    if ($payload.hook_event_name -cne "Stop") { exit 0 }
} elseif ($payload.hook_event_name -and $payload.hook_event_name -ne "SessionStart") {
    exit 0
}

$sessionId = $payload.session_id
if ([string]::IsNullOrWhiteSpace($sessionId)) { exit 0 }
if ([string]::IsNullOrWhiteSpace($payload.transcript_path)) { exit 0 }
if (-not [string]::IsNullOrWhiteSpace($env:CODEX_THREAD_ID) -and $env:CODEX_THREAD_ID -ne $sessionId) { exit 0 }

$herdr = if ([string]::IsNullOrWhiteSpace($env:HERDR_BIN_PATH)) { "herdr" } else { $env:HERDR_BIN_PATH }

if ($Action -eq "usage") {
    # Milliseconds since the epoch for an ISO-8601 string (or a date the JSON
    # parser already converted), else $null.
    function ConvertTo-UnixMilliseconds($value) {
        if ($value -is [DateTimeOffset]) { return $value.ToUnixTimeMilliseconds() }
        if ($value -is [DateTime]) { return ([DateTimeOffset]$value.ToUniversalTime()).ToUnixTimeMilliseconds() }
        if ($value -isnot [string]) { return $null }
        try {
            return [DateTimeOffset]::Parse($value, [Globalization.CultureInfo]::InvariantCulture, [Globalization.DateTimeStyles]::AssumeUniversal).ToUnixTimeMilliseconds()
        } catch {
            return $null
        }
    }

    function Test-Count($value) {
        return $value -is [int] -or $value -is [long]
    }

    $rolloutPath = $payload.transcript_path
    if ($rolloutPath -isnot [string] -or -not (Test-Path -LiteralPath $rolloutPath -PathType Leaf)) { exit 0 }
    try {
        $lines = @(Get-Content -LiteralPath $rolloutPath -Tail 400 -Encoding UTF8)
    } catch {
        exit 0
    }

    # Newest token_count event with a usable reading wins.
    for ($index = $lines.Count - 1; $index -ge 0; $index--) {
        if ([string]::IsNullOrWhiteSpace($lines[$index])) { continue }
        try { $entry = $lines[$index] | ConvertFrom-Json } catch { continue }
        if ($entry -isnot [System.Management.Automation.PSCustomObject]) { continue }
        if ($entry.type -cne "event_msg" -or $entry.payload.type -cne "token_count") { continue }
        $info = $entry.payload.info
        if ($info -isnot [System.Management.Automation.PSCustomObject]) { continue }
        $used = $info.last_token_usage.input_tokens
        if (-not (Test-Count $used) -or $used -lt 0) { continue }
        $observedMs = ConvertTo-UnixMilliseconds $entry.timestamp
        if ($null -eq $observedMs) { continue }
        try {
            $usageArgs = @(
                "pane",
                "report-context-usage",
                $env:HERDR_PANE_ID,
                "--source",
                "herdr:codex",
                "--used",
                "$used"
            )
            $window = $info.model_context_window
            if ((Test-Count $window) -and $window -gt 0) {
                $usageArgs += @("--window", "$window")
            }
            $usageArgs += @("--observed-at", "$observedMs")
            & $herdr @usageArgs 2>$null | Out-Null
        } catch {
        }
        break
    }
    exit 0
}

$seq = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
try {
    $args = @(
        "pane",
        "report-agent-session",
        $env:HERDR_PANE_ID,
        "--source",
        "herdr:codex",
        "--agent",
        "codex",
        "--seq",
        "$seq",
        "--agent-session-id",
        "$sessionId"
    )
    if ($payload.hook_event_name -eq "SessionStart" -and $payload.source -is [string] -and -not [string]::IsNullOrWhiteSpace($payload.source)) {
        $args += @("--session-start-source", "$($payload.source)")
    }
    & $herdr @args 2>$null | Out-Null
} catch {
}
