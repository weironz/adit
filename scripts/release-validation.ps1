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
    # ci.yml is dispatch-only (7c48002), so a manual run on main counts as much as a push run.
    if (-not $Run -or $Run.head_sha -cne $Sha -or $Run.head_branch -cne 'main' -or @('push', 'workflow_dispatch') -cnotcontains $Run.event) { throw 'No CI run on main for the exact commit' }
    if ($Run.status -cne 'completed' -or $Run.conclusion -cne 'success') { throw 'Exact-commit CI must be completed/success; pending, skipped and failed runs cannot release' }
    # Validate both job identity and its pinned hosted image.
    $windows = 0; $linux = 0; $rpm = 0; $names = @{}
    foreach ($job in $Jobs) {
        if ($job.status -cne 'completed' -or $job.conclusion -cne 'success') { throw 'Every CI job must have succeeded' }
        $name = [string]$job.name
        if ($names.ContainsKey($name)) { throw "Duplicate CI job: $name" }
        $names[$name] = $true
        $labels = @($job.labels)
        if ($job.runner_group_name -cne 'GitHub Actions' -or $labels.Count -ne 1) { throw 'Expected a GitHub-hosted runner with one image label' }
        if ($labels[0] -ceq 'windows-2025' -and $name.EndsWith('(windows-x64)')) {
            $windows++
        } elseif ($labels[0] -ceq 'ubuntu-24.04' -and $name.EndsWith('(linux-x64)')) {
            $linux++
        } elseif ($labels[0] -ceq 'ubuntu-24.04' -and $name -cmatch '^rpm \(fedora-\d+\)$') {
            $rpm++
        } else { throw 'CI job used an unexpected platform or runner' }
    }
    if ($windows -ne 1 -or $linux -ne 1) { throw 'CI job used an unexpected or duplicate runner' }
    # The Fedora rpm jobs are paused in ci.yml; when they come back, require them:
    # if ($rpm -lt 1) { throw 'Expected the Fedora rpm CI jobs' }
}
