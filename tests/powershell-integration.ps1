param(
    [string]$IntegrationPath = (Join-Path $PSScriptRoot '../shell/shellIntegration.ps1')
)

$ErrorActionPreference = 'Stop'
$env:ISTERM_TESTING = '0'
$Global:PromptCalls = 0
function Global:Prompt { $Global:PromptCalls++; return 'custom> ' }
$original = $function:Prompt

. $IntegrationPath
1..3 | ForEach-Object { . $IntegrationPath }
if (-not [object]::ReferenceEquals($Global:__IsOriginalPrompt, $original)) {
    throw 'Repeated sourcing replaced the original prompt with the integration wrapper'
}
$result = Prompt
if ($Global:PromptCalls -ne 1) { throw 'The original prompt must run exactly once' }
foreach ($marker in @('PS', 'PE', 'CWD;')) {
    if ([regex]::Matches($result, [regex]::Escape("]6973;$marker")).Count -ne 1) {
        throw "Expected exactly one $marker marker: $result"
    }
}
if (-not $result.Contains('custom> ')) { throw 'The custom prompt was lost' }

# A profile may install a different prompt between integration loads.
function Global:Prompt { return 'replacement> ' }
$replacement = $function:Prompt
. $IntegrationPath
. $IntegrationPath
if (-not [object]::ReferenceEquals($Global:__IsOriginalPrompt, $replacement)) {
    throw 'A newly installed user prompt was not captured'
}
if (-not (Prompt).Contains('replacement> ')) { throw 'The replacement prompt was lost' }

$env:ISTERM_TESTING = '1'
function Global:Prompt { throw 'The testing prompt should replace this function' }
. $IntegrationPath
. $IntegrationPath
if (-not (Prompt).Contains('PS > ')) { throw 'The deterministic testing prompt was lost' }
Write-Output 'PowerShell integration regression passed'
