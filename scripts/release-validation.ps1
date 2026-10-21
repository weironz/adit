# Pure validation shared by the production gate and its offline negative cases.
function Get-AditWorkspaceVersion([string]$Root) {
    $versions = @()
    foreach ($relative in @('Cargo.toml', 'crates/adit-rdp/Cargo.toml', 'crates/adit-rdp-proto/Cargo.toml')) {
        $text = [IO.File]::ReadAllText((Join-Path $Root $relative))
        $match = [regex]::Matches($text, '(?m)^version = "([0-9]+\.[0-9]+\.[0-9]+)"\r?$')
        if ($match.Count -ne 1) { throw "Expected exactly one package version in $relative" }
        $versions += $match[0].Groups[1].Value
    }
    if (@($versions | Select-Object -Unique).Count -ne 1) { throw 'Workspace, RDP helper and IPC versions differ' }
    return $versions[0]
}
function Assert-AditReleaseVersion([string]$Current, [string]$Requested) {
    if ($Requested -cnotmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$') { throw 'Release version must be X.Y.Z without a leading v or zero padding' }
    $old = [version]$Current
    $expected = '{0}.{1}.{2}' -f $old.Major, $old.Minor, ($old.Build + 1)
    if ($Requested -cne $expected) { throw "Only the next patch version $expected can be released" }
}
function Assert-AditReleaseCI([string]$Ref, [string]$Sha, [string]$MainSha, $Run, $Jobs) {
    if ($Ref -cne 'refs/heads/main') { throw 'Release is allowed from main only' }
    if ($Sha -cne $MainSha) { throw 'Main advanced since this workflow started' }
    if (-not $Run -or $Run.head_sha -cne $Sha -or $Run.head_branch -cne 'main' -or $Run.event -cne 'push') { throw 'No push CI run for the exact main commit' }
    if ($Run.status -cne 'completed' -or $Run.conclusion -cne 'success') { throw 'Exact-commit CI must be completed/success; pending, skipped and failed runs cannot release' }
    $expected = @{
        'home-workstation-windows-adit-01' = 'Windows'
        'home-workstation-linux-adit-01' = 'Linux'
    }
    if (@($Jobs).Count -ne $expected.Count) { throw 'Expected both self-hosted x64 CI jobs' }
    $seen = @{}
    foreach ($job in $Jobs) {
        if (-not $expected.ContainsKey([string]$job.runner_name) -or $seen.ContainsKey([string]$job.runner_name)) { throw 'CI job used an unexpected or duplicate runner' }
        if ($job.status -cne 'completed' -or $job.conclusion -cne 'success') { throw 'Every CI job must have succeeded' }
        foreach ($label in @('self-hosted', 'X64', 'home-workstation', 'repo-adit', $expected[$job.runner_name])) {
            if (@($job.labels) -cnotcontains $label) { throw "CI runner label missing: $label" }
        }
        $seen[$job.runner_name] = $true
    }
}
