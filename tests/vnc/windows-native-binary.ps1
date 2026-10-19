param([Parameter(Mandatory)][string]$Directory,[switch]$Produce)
$ErrorActionPreference='Stop'
if ($env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows') { throw 'Disposable hosted Windows only' }
$objects=@(git ls-tree -r HEAD -- crates Cargo.toml Cargo.lock .cargo assets build.rs rust-toolchain rust-toolchain.toml)
if ($LASTEXITCODE -ne 0 -or $objects.Count -lt 10) {throw 'Cannot fingerprint native build inputs'}
$digest=[Security.Cryptography.SHA256]::Create()
try {$fingerprint=[Convert]::ToHexString($digest.ComputeHash([Text.Encoding]::UTF8.GetBytes(($objects -join "`n")))).ToLowerInvariant()}
finally {$digest.Dispose()}
$compiler=(rustc -Vv) -join "`n"
if ($LASTEXITCODE -ne 0) {throw 'Cannot identify compiler'}
$manifestPath=Join-Path $Directory 'provenance.json'
$exe=Join-Path $Directory 'vnc_native_acceptance.exe'
if ($Produce) {
 New-Item -ItemType Directory -Path $Directory -ErrorAction Stop | Out-Null
 Copy-Item -LiteralPath 'target/debug/examples/vnc_native_acceptance.exe' -Destination $exe
 @{source_sha=$env:GITHUB_SHA;producer_run=$env:GITHUB_RUN_ID;rust_inputs_sha256=$fingerprint;compiler=$compiler;binary_sha256=(Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant()} |
  ConvertTo-Json | Set-Content -LiteralPath $manifestPath -Encoding UTF8
}
$manifest=Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
if ($manifest.rust_inputs_sha256 -cne $fingerprint -or $manifest.compiler -cne $compiler -or
 $manifest.binary_sha256 -cne (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant() -or
 @(Get-ChildItem -LiteralPath $Directory -Force).Count -ne 2) {throw 'Native executable, compiler or full build-input fingerprint mismatch; rebuild instead'}
Write-Output "Verified native binary from run $($manifest.producer_run), commit $($manifest.source_sha); current Rust build inputs identical."
