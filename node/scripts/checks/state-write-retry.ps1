# Review B3: a failed write of the chain state is retried and shown on /health. Starts the net on a
# fresh store whose disk refuses writes for its first 20 s: /health must say so (status waiting,
# writeFailing), and once the disk takes writes again the state reaches it and /health is ready.
# Usage (from backend/node): powershell -ExecutionPolicy Bypass -File scripts/checks/state-write-retry.ps1 [acrux.exe]
param([string]$Binary = 'target\release\acrux.exe')
$ErrorActionPreference = 'Stop'
$node = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$store = Join-Path $node 'target\retry-check'
if (Test-Path $store) { Remove-Item -Recurse -Force $store }
$env:ACRUX_STATE_WRITE_FAIL_MS = '20000'
$net = Start-Process -FilePath (Join-Path $node $Binary) -ArgumentList 'net', 'up', '--store', $store -WorkingDirectory $node -WindowStyle Hidden -PassThru -RedirectStandardOutput "$store-out.log" -RedirectStandardError "$store-err.log"
try {
  Start-Sleep 12
  $during = Invoke-RestMethod -Uri http://127.0.0.1:4930/health -TimeoutSec 5
  Write-Output ("while the disk refuses: " + ($during | ConvertTo-Json -Compress))
  Start-Sleep 18
  $after = Invoke-RestMethod -Uri http://127.0.0.1:4930/health -TimeoutSec 5
  Write-Output ("after: " + ($after | ConvertTo-Json -Compress))
  $ok = $during.status -eq 'waiting' -and $during.chainState.writeFailing -eq $true -and $after.status -eq 'ready' -and $after.chainState.writeFailing -eq $false -and $after.chainState.durableCblock -gt 0
} finally {
  Stop-Process -Id $net.Id -Force
}
if ($ok) { Write-Output 'retried and shown: pass'; exit 0 } else { Write-Output 'fail'; exit 1 }
