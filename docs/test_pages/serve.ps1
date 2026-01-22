param(
  [int]$Port = 8000
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

Write-Host "Serving test pages at http://127.0.0.1:$Port"
Write-Host "Press Ctrl+C to stop."

python -m http.server $Port
