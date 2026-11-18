# Review B3: a transfer reported finalized survives a crash. Starts the net on a fresh store with a
# slow state disk, sends a transfer until it is reported finalized, kills the net at once, starts it
# again on the same store and reads the transfer back. Exit 0 when it survived.
# Usage (from backend/node): powershell -ExecutionPolicy Bypass -File scripts/checks/final-survives-crash.ps1 [acrux.exe]
param([string]$Binary = 'target\release\acrux.exe')
$ErrorActionPreference = 'Stop'
$node = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$sdk = Join-Path (Split-Path -Parent $node) 'sdk'
$store = Join-Path $node 'target\crash-check'
if (Test-Path $store) { Remove-Item -Recurse -Force $store }
$env:ACRUX_STATE_WRITE_DELAY_MS = '4000'

function Start-Net {
  $p = Start-Process -FilePath (Join-Path $node $Binary) -ArgumentList 'net', 'up', '--store', $store -WorkingDirectory $node -WindowStyle Hidden -PassThru -RedirectStandardOutput "$store-out.log" -RedirectStandardError "$store-err.log"
  for ($i = 0; $i -lt 60; $i++) {
    Start-Sleep 1
    try { Invoke-RestMethod -Uri http://127.0.0.1:4930 -Method Post -ContentType 'application/json' -Body '{"jsonrpc":"2.0","id":1,"method":"getSlot","params":[]}' -TimeoutSec 2 | Out-Null; return $p } catch {}
  }
  throw 'the net did not start'
}

$net = Start-Net
Push-Location $sdk
try {
  $sent = (node test/final-survives-crash.mjs send | Select-Object -Last 1).Split(' ')
  Stop-Process -Id $net.Id -Force
  Write-Output "finalized in $($sent[2]) ms; net killed at once"
  $net.WaitForExit()
  $net = Start-Net
  Start-Sleep 5
  node test/final-survives-crash.mjs check $sent[0] $sent[1]
  $result = $LASTEXITCODE
} finally {
  Pop-Location
  Stop-Process -Id $net.Id -Force -ErrorAction SilentlyContinue
}
exit $result
