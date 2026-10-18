param([Parameter(Mandatory)][string]$Qemu)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (-not $IsWindows -or $env:GITHUB_ACTIONS -ne 'true' -or
    $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows' -or
    -not $env:RUNNER_TEMP -or -not $env:GITHUB_SHA) {
    throw 'Matching helpers are restricted to a disposable Windows GitHub runner'
}
$runnerRoot = [IO.Path]::GetFullPath($env:RUNNER_TEMP).TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
$evidence = [IO.Path]::GetFullPath((Join-Path $env:RUNNER_TEMP 'adit-netem-matching-helpers'))
if (-not $evidence.StartsWith($runnerRoot, [StringComparison]::OrdinalIgnoreCase) -or
    (Test-Path -LiteralPath $evidence)) { throw 'Fresh private RUNNER_TEMP helper directory required' }
[void][IO.Directory]::CreateDirectory($evidence)
$utf8 = [Text.UTF8Encoding]::new($false)
$packageUrl = 'https://repo.msys2.org/mingw/mingw64/mingw-w64-x86_64-glib2-2.88.3-1-any.pkg.tar.zst'
$packageSha256 = '5aa850934d0216f5b7237bd54c3ec660ebfc3a42fd992bc7abbd611bd9cc7663'
$qemuSha256 = '47d57a6072e0bb3bd98f87926eb129eb1736dfe818c67b3b81ef7ce4edd0b3cd'
$glibSha256 = '7b5c19f5540857ec2288323f3b1654cbe5655fc3ea21417a25da69bece489eec'
$expectedHelpers = @(
    [ordered]@{ name='gspawn-win64-helper.exe'; bytes=26808; sha256='87113f2bf18d694910a1e6791282aea4da3611d41a8397434e43fed6deec72a5' },
    [ordered]@{ name='gspawn-win64-helper-console.exe'; bytes=26185; sha256='c8734364a23a2af11660efba3d4c9b82bcdc604f15816cfd92ed2697ee30cdaf' }
)
$record = [ordered]@{
    outcome='failed'; phase='guard_passed'; source_sha=$env:GITHUB_SHA; run_id=$env:GITHUB_RUN_ID
    scope='Only two matching official GLib helper EXEs copied next to the exact QEMU DLL for the existing fake-only cmd lifecycle calibration. No DLL replacement, PATH/global install, protection/network changes, real VNC or credentials.'
    preparation_executes_vendor_binaries=$false; copied_helpers=@(); selected_helpers=@()
    prior_gpg=[ordered]@{
        source_sha='175ab8136362e29269ea326bbba596db8c8d67e0'; run_id=39151046882; artifact_id=12144993696
        run_url='https://github.com/weironz/adit/actions/runs/39151046882'
        artifact_github_reported_digest='sha256:a50a2caeb523f46d1761b046f37aa28cd129541bb7be67d682bbfd40b260edca'
        keyring_commit='5d249077a0cfeec9dbf51515e5e01da413d28859'
        verified_developer_fingerprint='5F944B027F7FE2091985AA2EFA11531AA0AA7F57'; trust='TRUST_FULLY 0 pgp'
        note='Package hash was measured after successful default-GPG signature verification with pinned official master trust anchors in that run. This Windows preparation strictly matches those authenticated bytes; it does not repeat GPG verification.'
    }
    package=[ordered]@{
        url=$packageUrl; signature_url=($packageUrl + '.sig'); expected_sha256=$packageSha256
        checksum_origin='Prior signature-verified package measurement, not an MSYS2 prepublished checksum'
        source_commit='716ee21e59851aa4d15e34c8353580334276b0e6'
        source_url='https://github.com/msys2/MINGW-packages/blob/716ee21e59851aa4d15e34c8353580334276b0e6/mingw-w64-glib2/PKGBUILD'
        license='LGPL-2.1-or-later'; license_package_path='mingw64/share/licenses/glib2/COPYING'
        license_metadata_artifact=12144993696; license_metadata_file='COPYING.txt'
    }
}
function Write-HelperRecord {
    [IO.File]::WriteAllText((Join-Path $evidence 'result.json'), ($record | ConvertTo-Json -Depth 12) + "`n", $utf8)
}
function Set-HelperPhase([string]$Name) {
    $record.phase = $Name
    [IO.File]::AppendAllText((Join-Path $evidence 'phases.txt'), ([DateTimeOffset]::UtcNow.ToString('o') + ' ' + $Name + "`n"), $utf8)
    Write-HelperRecord
}
function Get-Sha256([string]$Path) {
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}
function Read-HelperPeMetadata([byte[]]$Bytes) {
    if ($Bytes.Length -lt 64 -or $Bytes[0] -ne 0x4d -or $Bytes[1] -ne 0x5a) { throw 'Missing DOS header' }
    $pe = [long][BitConverter]::ToUInt32($Bytes, 60)
    if ($pe -gt $Bytes.Length - 24 -or [BitConverter]::ToUInt32($Bytes, [int]$pe) -ne 0x4550) { throw 'Invalid PE header offset/signature' }
    $machine = [BitConverter]::ToUInt16($Bytes, [int]$pe + 4)
    $sections = [BitConverter]::ToUInt16($Bytes, [int]$pe + 6)
    $optionalSize = [BitConverter]::ToUInt16($Bytes, [int]$pe + 20)
    $optional = $pe + 24
    if ($machine -ne 0x8664 -or $optionalSize -lt 128 -or
        $optional + $optionalSize -gt $Bytes.Length -or
        [BitConverter]::ToUInt16($Bytes, [int]$optional) -ne 0x20b) { throw 'Expected AMD64 PE32+ optional header' }
    $sectionStart = $optional + $optionalSize
    if ($sections -eq 0 -or $sections -gt 96 -or $sectionStart + 40 * $sections -gt $Bytes.Length) { throw 'Invalid PE section table' }
    $importRva = [long][BitConverter]::ToUInt32($Bytes, [int]$optional + 120)
    $importSize = [long][BitConverter]::ToUInt32($Bytes, [int]$optional + 124)
    if ([BitConverter]::ToUInt32($Bytes, [int]$optional + 108) -lt 2 -or $importRva -eq 0 -or $importSize -lt 20) { throw 'Missing bounded PE import directory' }
    function Convert-RvaToOffset([long]$Rva) {
        for ($i = 0; $i -lt $sections; $i++) {
            $section = [int]($sectionStart + 40 * $i)
            $virtualSize = [long][BitConverter]::ToUInt32($Bytes, $section + 8)
            $virtualAddress = [long][BitConverter]::ToUInt32($Bytes, $section + 12)
            $rawSize = [long][BitConverter]::ToUInt32($Bytes, $section + 16)
            $rawPointer = [long][BitConverter]::ToUInt32($Bytes, $section + 20)
            if ($Rva -ge $virtualAddress -and $Rva -lt $virtualAddress + [Math]::Max($virtualSize, $rawSize)) {
                $offset = $rawPointer + $Rva - $virtualAddress
                if ($Rva - $virtualAddress -ge $rawSize -or $offset -ge $Bytes.Length) { throw 'PE RVA is not backed by file data' }
                return [int]$offset
            }
        }
        throw 'PE RVA outside section table'
    }
    $imports = @()
    $terminated = $false
    for ($i = 0; $i -lt 32 -and 20 * ($i + 1) -le $importSize; $i++) {
        $descriptor = Convert-RvaToOffset ($importRva + 20 * $i)
        if ($descriptor -gt $Bytes.Length - 20) { throw 'Truncated import descriptor' }
        $nonzero = $false
        for ($j = 0; $j -lt 20; $j++) { if ($Bytes[$descriptor + $j] -ne 0) { $nonzero = $true } }
        if (-not $nonzero) { $terminated = $true; break }
        $nameRva = [long][BitConverter]::ToUInt32($Bytes, $descriptor + 12)
        $nameOffset = Convert-RvaToOffset $nameRva
        $nameEnd = $nameOffset
        while ($nameEnd -lt $Bytes.Length -and $nameEnd - $nameOffset -lt 256 -and $Bytes[$nameEnd] -ne 0) { $nameEnd++ }
        if ($nameEnd -ge $Bytes.Length -or $Bytes[$nameEnd] -ne 0 -or $nameEnd -eq $nameOffset) { throw 'Unterminated PE import name' }
        $imports += [Text.Encoding]::ASCII.GetString($Bytes, $nameOffset, $nameEnd - $nameOffset)
    }
    if (-not $terminated) { throw 'Import descriptor table has no bounded terminator' }
    $actual = @($imports | ForEach-Object { $_.ToLowerInvariant() } | Sort-Object)
    $expected = @('kernel32.dll','libglib-2.0-0.dll','msvcrt.dll','shell32.dll')
    if (($actual -join '|') -ne ($expected -join '|')) { throw 'Helper imports differ from the signature-verified AMD64/MSVCRT source metadata' }
    return [ordered]@{ machine='0x8664'; optional_magic='0x20b'; imports=$imports; expected_runtime='mingw64/MSVCRT' }
}
try {
    Set-HelperPhase 'verify_exact_existing_qemu_and_glib'
    $expectedRoot = [IO.Path]::GetFullPath((Join-Path $env:RUNNER_TEMP 'adit-netem-qemu-tools/extracted'))
    $qemuPath = [IO.Path]::GetFullPath($Qemu)
    if ($qemuPath -ne (Join-Path $expectedRoot 'qemu-system-x86_64.exe') -or
        -not $qemuPath.StartsWith($runnerRoot, [StringComparison]::OrdinalIgnoreCase)) { throw 'QEMU must have the pinned fresh extraction layout' }
    foreach ($path in @($expectedRoot, $qemuPath)) {
        if ((Get-Item -LiteralPath $path).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Reparse points are not accepted for the fixed QEMU layout' }
    }
    $glibPath = Join-Path $expectedRoot 'libglib-2.0-0.dll'
    $record.existing_qemu = [ordered]@{ path=$qemuPath; sha256=(Get-Sha256 $qemuPath) }
    $record.existing_glib = [ordered]@{ path=$glibPath; sha256=(Get-Sha256 $glibPath) }
    if ($record.existing_qemu.sha256 -ne $qemuSha256 -or $record.existing_glib.sha256 -ne $glibSha256) { throw 'Exact QEMU executable and GLib DLL hashes are required' }
    foreach ($item in $expectedHelpers) {
        if (Test-Path -LiteralPath (Join-Path $expectedRoot $item.name)) { throw 'Refuse to replace an existing helper' }
    }
    Set-HelperPhase 'download_previously_signature_verified_package'
    $archive = Join-Path $evidence 'glib2.pkg.tar.zst'
    Invoke-WebRequest -Uri $packageUrl -OutFile $archive -TimeoutSec 25
    $record.package.actual_sha256 = Get-Sha256 $archive
    $record.package.actual_bytes = (Get-Item -LiteralPath $archive).Length
    if ($record.package.actual_sha256 -ne $packageSha256 -or $record.package.actual_bytes -ne 5402063) { throw 'Package differs from the exact signature-verified bytes' }
    Set-HelperPhase 'decompress_package_without_execution'
    $tarRoot = Join-Path $evidence 'tar-container'
    & 7z x $archive "-o$tarRoot" -y 2>&1 | Out-File -LiteralPath (Join-Path $evidence 'decompression.txt') -Encoding utf8
    if ($LASTEXITCODE -ne 0) { throw "Package decompression failed: $LASTEXITCODE" }
    $tarFiles = @(Get-ChildItem -LiteralPath $tarRoot -File -Recurse)
    if ($tarFiles.Count -ne 1 -or $tarFiles[0].Extension -ne '.tar') { throw 'Expected exactly one decompressed tar container' }
    & 7z l -slt $tarFiles[0].FullName 2>&1 | Out-File -LiteralPath (Join-Path $evidence 'package-listing.txt') -Encoding utf8
    if ($LASTEXITCODE -ne 0) { throw "Package metadata listing failed: $LASTEXITCODE" }
    Set-HelperPhase 'extract_only_two_fixed_helper_entries'
    $selectedRoot = Join-Path $evidence 'selected'
    $entries = @($expectedHelpers | ForEach-Object { 'mingw64/bin/' + $_.name })
    & 7z x $tarFiles[0].FullName "-o$selectedRoot" -spd -y @entries 2>&1 | Out-File -LiteralPath (Join-Path $evidence 'extraction.txt') -Encoding utf8
    if ($LASTEXITCODE -ne 0) { throw "Selected helper extraction failed: $LASTEXITCODE" }
    $selectedFiles = @(Get-ChildItem -LiteralPath $selectedRoot -File -Recurse)
    if ($selectedFiles.Count -ne 2) { throw 'Extraction must contain only the two fixed helper files' }
    foreach ($item in $expectedHelpers) {
        $path = Join-Path $selectedRoot ('mingw64/bin/' + $item.name)
        $hash = Get-Sha256 $path
        $bytes = [IO.File]::ReadAllBytes($path)
        if ($hash -ne $item.sha256 -or $bytes.Length -ne $item.bytes) { throw 'Helper differs from the exact signature-verified source metadata' }
        $pe = Read-HelperPeMetadata $bytes
        $record.selected_helpers += [ordered]@{ package_path=('mingw64/bin/' + $item.name); extracted_path=$path; bytes=$bytes.Length; sha256=$hash; pe=$pe }
    }
    Set-HelperPhase 'copy_two_verified_exes_exclusively_next_to_exact_dll'
    foreach ($item in $record.selected_helpers) {
        $destination = Join-Path $expectedRoot ([IO.Path]::GetFileName($item.package_path))
        # CreateNew is mandatory: even an intervening existing file must fail,
        # never become an implicit overwrite or a dependency search-path change.
        $output = [IO.File]::Open($destination, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try {
            $inputFile = [IO.File]::OpenRead($item.extracted_path)
            try { $inputFile.CopyTo($output) } finally { $inputFile.Dispose() }
        } finally { $output.Dispose() }
        $destinationHash = Get-Sha256 $destination
        $record.copied_helpers += [ordered]@{ path=$destination; bytes=(Get-Item -LiteralPath $destination).Length; sha256=$destinationHash }
        Write-HelperRecord
        if ($destinationHash -ne $item.sha256) { throw 'Copied helper hash mismatch' }
    }
    if ((Get-Sha256 $qemuPath) -ne $qemuSha256 -or (Get-Sha256 $glibPath) -ne $glibSha256) { throw 'QEMU/DLL changed during helper preparation' }
    $record.outcome = 'prepared_matching_helpers_for_fake_calibration'
    Set-HelperPhase 'complete_no_execution_in_preparation'
} catch {
    $record.outcome = 'failed'
    $record.error = $_.Exception.Message
    [IO.File]::WriteAllText((Join-Path $evidence 'failure.txt'), ($_ | Out-String), $utf8)
    throw
} finally {
    Write-HelperRecord
}
