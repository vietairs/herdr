# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=claude
# HERDR_INTEGRATION_VERSION=12

param([string]$Action = "")

if ($Action -ne "session" -and $Action -ne "cache") { exit 0 }
if ($env:HERDR_ENV -ne "1") { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:HERDR_PANE_ID)) { exit 0 }

$inputText = [Console]::In.ReadToEnd()
try {
    $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json }
} catch {
    exit 0
}

$propertyNames = @($payload.PSObject.Properties.Name)
if ((Test-Path Env:CURSOR_VERSION) -or $propertyNames -ccontains "cursor_version") { exit 0 }
if (-not ($propertyNames -ccontains "hook_event_name") -or $payload.hook_event_name -isnot [string]) { exit 0 }
if ($Action -eq "session") {
    if ($payload.hook_event_name -cne "SessionStart") { exit 0 }
} elseif ($payload.hook_event_name -cne "Stop" -and $payload.hook_event_name -cne "PostToolUse") {
    exit 0
}
if (-not [string]::IsNullOrWhiteSpace($payload.agent_id)) { exit 0 }

$herdr = if ([string]::IsNullOrWhiteSpace($env:HERDR_BIN_PATH)) { "herdr" } else { $env:HERDR_BIN_PATH }

if ($Action -eq "cache") {
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

    function Test-PositiveInteger($value) {
        return ($value -is [int] -or $value -is [long]) -and $value -gt 0
    }

    function Get-TokenCount($value) {
        if (Test-PositiveInteger $value) { return [long]$value }
        return [long]0
    }

    # A real main-thread assistant entry with usage and a timestamp, else $null.
    function Get-MainChainAssistant($entry) {
        if ($entry.type -cne "assistant" -or $entry.isSidechain -eq $true) { return $null }
        $message = $entry.message
        if ($null -eq $message -or $message.model -ceq "<synthetic>") { return $null }
        $usage = $message.usage
        if ($null -eq $usage) { return $null }
        $ms = ConvertTo-UnixMilliseconds $entry.timestamp
        if ($null -eq $ms) { return $null }
        return @{ Usage = $usage; Ms = $ms }
    }

    $transcriptPath = $payload.transcript_path
    if ($transcriptPath -isnot [string] -or [string]::IsNullOrWhiteSpace($transcriptPath)) { exit 0 }
    if (-not (Test-Path -LiteralPath $transcriptPath -PathType Leaf)) { exit 0 }
    try {
        $lines = @(Get-Content -LiteralPath $transcriptPath -Tail 200 -Encoding UTF8)
    } catch {
        exit 0
    }

    $entries = New-Object System.Collections.Generic.List[object]
    foreach ($line in $lines) {
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        try { $parsed = $line | ConvertFrom-Json } catch { continue }
        if ($parsed -is [System.Management.Automation.PSCustomObject]) { $entries.Add($parsed) }
    }

    $cacheRequestMs = $null
    $cacheTtl = $null
    for ($index = $entries.Count - 1; $index -ge 0; $index--) {
        $found = Get-MainChainAssistant $entries[$index]
        if ($null -eq $found) { continue }
        $usage = $found.Usage
        if (-not ((Test-PositiveInteger $usage.cache_creation_input_tokens) -or (Test-PositiveInteger $usage.cache_read_input_tokens))) { continue }
        # The countdown counts from the end of Claude's last response, matching
        # statusline tools such as tokenline. The timestamp is floored to whole
        # seconds so both show the same second.
        $cacheRequestMs = [long][math]::Floor($found.Ms / 1000) * 1000
        if (Test-PositiveInteger $usage.cache_creation.ephemeral_1h_input_tokens) {
            $cacheTtl = 3600
        } elseif (Test-PositiveInteger $usage.cache_creation.ephemeral_5m_input_tokens) {
            $cacheTtl = 300
        }
        break
    }

    if ($null -ne $cacheRequestMs) {
        try {
            $cacheArgs = @(
                "pane",
                "report-prompt-cache",
                $env:HERDR_PANE_ID,
                "--source",
                "herdr:claude",
                "--last-request-at",
                "$cacheRequestMs"
            )
            if ($null -ne $cacheTtl) {
                $cacheArgs += @("--ttl", "$cacheTtl")
            }
            & $herdr @cacheArgs 2>$null | Out-Null
        } catch {
        }
    }

    # Hooks never see the model's window; only tokens are reported. A compact
    # boundary means earlier usage is stale, so nothing is reported after one.
    $usedTokens = $null
    $observedMs = $null
    for ($index = $entries.Count - 1; $index -ge 0; $index--) {
        $entry = $entries[$index]
        if ($entry.type -ceq "system" -and $entry.subtype -ceq "compact_boundary") { break }
        $found = Get-MainChainAssistant $entry
        if ($null -eq $found) { continue }
        $usage = $found.Usage
        if ($usage.input_tokens -isnot [int] -and $usage.input_tokens -isnot [long]) { continue }
        $usedTokens = (Get-TokenCount $usage.input_tokens) + (Get-TokenCount $usage.cache_creation_input_tokens) + (Get-TokenCount $usage.cache_read_input_tokens)
        $observedMs = $found.Ms
        break
    }

    if ($null -ne $usedTokens) {
        try {
            $contextArgs = @(
                "pane",
                "report-context-usage",
                $env:HERDR_PANE_ID,
                "--source",
                "herdr:claude",
                "--used",
                "$usedTokens",
                "--observed-at",
                "$observedMs"
            )
            & $herdr @contextArgs 2>$null | Out-Null
        } catch {
        }
    }
    exit 0
}

$sessionId = $payload.session_id
if ([string]::IsNullOrWhiteSpace($sessionId)) { exit 0 }

$seq = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
try {
    $args = @(
        "pane",
        "report-agent-session",
        $env:HERDR_PANE_ID,
        "--source",
        "herdr:claude",
        "--agent",
        "claude",
        "--seq",
        "$seq",
        "--agent-session-id",
        "$sessionId"
    )
    if ($payload.transcript_path -is [string] -and -not [string]::IsNullOrWhiteSpace($payload.transcript_path)) {
        $args += @("--agent-session-path", "$($payload.transcript_path)")
    }
    if ($payload.hook_event_name -eq "SessionStart" -and $payload.source -is [string] -and -not [string]::IsNullOrWhiteSpace($payload.source)) {
        $args += @("--session-start-source", "$($payload.source)")
    }
    & $herdr @args 2>$null | Out-Null
} catch {
}
