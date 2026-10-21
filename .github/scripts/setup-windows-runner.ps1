param([switch]$ReleaseTools)
$ErrorActionPreference = 'Stop'
# CI and the manual bootstrap share one implementation.
& (Join-Path $PSScriptRoot '../../scripts/runner-bootstrap-windows.ps1') -BuildOnly:(-not $ReleaseTools)
