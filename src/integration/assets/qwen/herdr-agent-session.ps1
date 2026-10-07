# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=qwen
# HERDR_INTEGRATION_VERSION=2

param([string]$Action = "")

if ($Action -ne "session" -and $Action -ne "usage") { exit 0 }
if ($env:HERDR_ENV -ne "1") { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:HERDR_PANE_ID)) { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:HERDR_SOCKET_PATH)) { exit 0 }

$inputText = [Console]::In.ReadToEnd()
try {
    $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json }
} catch {
    $payload = $null
}

if ($null -eq $payload) { exit 0 }

$herdr = if ([string]::IsNullOrWhiteSpace($env:HERDR_BIN_PATH)) { "herdr" } else { $env:HERDR_BIN_PATH }

# A JSON integer that is a whole number >= 0; booleans and strings never qualify.
function ConvertTo-TokenCount($value) {
    if ($value -is [int] -or $value -is [long]) {
        if ($value -ge 0) { return [long]$value }
    }
    return $null
}

if ($Action -eq "usage") {
    if ($payload.hook_event_name -ne "Stop") { exit 0 }
    $used = ConvertTo-TokenCount $payload.input_tokens
    if ($null -eq $used) { exit 0 }
    $commandArgs = @(
        "pane", "report-context-usage", $env:HERDR_PANE_ID,
        "--source", "herdr:qwen", "--used", [string]$used
    )
    $window = ConvertTo-TokenCount $payload.context_limit
    if ($null -ne $window -and $window -gt 0) {
        $commandArgs += @("--window", [string]$window)
    }
} else {
    if ([string]::IsNullOrWhiteSpace($payload.session_id)) { exit 0 }
    $seq = [DateTime]::UtcNow.Ticks
    $commandArgs = @(
        "pane", "report-agent-session", $env:HERDR_PANE_ID,
        "--source", "herdr:qwen", "--agent", "qwen",
        "--agent-session-id", [string]$payload.session_id,
        "--seq", [string]$seq
    )
    if ($payload.source -in @("startup", "resume", "clear", "compact", "branch")) {
        $commandArgs += @("--session-start-source", [string]$payload.source)
    }
}
try {
    & $herdr @commandArgs 2>$null | Out-Null
} catch {
}
