$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'release-validation.ps1')
$sha = 'a' * 40
$run = @{head_sha=$sha; head_branch='main'; event='push'; status='completed'; conclusion='success'}
function New-GateJobs {
    @(
        @{name='build · clippy · test · integration (windows-x64)';runner_name='home-workstation-windows-adit-01';status='completed';conclusion='success';labels=@('self-hosted','Windows','X64','home-workstation','repo-adit')},
        @{name='build · clippy · test · integration (linux-x64)';runner_name='GitHub Actions 7';status='completed';conclusion='success';labels=@('ubuntu-24.04')},
        @{name='rpm (fedora-43)';runner_name='GitHub Actions 8';status='completed';conclusion='success';labels=@('ubuntu-24.04')},
        @{name='rpm (fedora-44)';runner_name='GitHub Actions 9';status='completed';conclusion='success';labels=@('ubuntu-24.04')}
    )
}
Assert-AditReleaseVersion '0.1.75' '0.1.76'
Assert-AditReleaseCI 'refs/heads/main' $sha $sha $run (New-GateJobs)
$dispatched=$run.Clone();$dispatched.event='workflow_dispatch'
Assert-AditReleaseCI 'refs/heads/main' $sha $sha $dispatched (New-GateJobs)
$rejected = 0
function Expect-Rejected([scriptblock]$Case) {
    $failed = $false
    try { & $Case } catch { $failed = $true }
    if (-not $failed) { throw 'Unsafe release case was accepted' }
    $script:rejected++
}
foreach ($bad in @('v0.1.76','0.1.75','0.1.77','0.2.0','00.1.76','0.1.76;echo injected')) {
    Expect-Rejected { Assert-AditReleaseVersion '0.1.75' $bad }
}
Expect-Rejected { Assert-AditReleaseCI 'refs/heads/test' $sha $sha $run (New-GateJobs) }
Expect-Rejected { Assert-AditReleaseCI 'refs/heads/main' $sha ('b'*40) $run (New-GateJobs) }
Expect-Rejected { Assert-AditReleaseCI 'refs/heads/main' $sha $sha $null (New-GateJobs) }
foreach ($bad in @('failure','cancelled','skipped')) {
    $badRun = $run.Clone(); $badRun.conclusion=$bad
    Expect-Rejected { Assert-AditReleaseCI 'refs/heads/main' $sha $sha $badRun (New-GateJobs) }
}
$pending=$run.Clone();$pending.status='in_progress'
Expect-Rejected { Assert-AditReleaseCI 'refs/heads/main' $sha $sha $pending (New-GateJobs) }
$other=$run.Clone();$other.head_sha='b'*40
Expect-Rejected { Assert-AditReleaseCI 'refs/heads/main' $sha $sha $other (New-GateJobs) }
$hosted=New-GateJobs;$hosted[0].runner_name='GitHub Actions 1'
Expect-Rejected { Assert-AditReleaseCI 'refs/heads/main' $sha $sha $run $hosted }
$labels=New-GateJobs;$labels[0].labels=@('Windows','X64')
Expect-Rejected { Assert-AditReleaseCI 'refs/heads/main' $sha $sha $run $labels }
$failedJob=New-GateJobs;$failedJob[0].conclusion='skipped'
Expect-Rejected { Assert-AditReleaseCI 'refs/heads/main' $sha $sha $run $failedJob }
Expect-Rejected { Assert-AditReleaseCI 'refs/heads/main' $sha $sha $run @((New-GateJobs)[0]) }
Assert-AditReleaseCI 'refs/heads/main' $sha $sha $run @((New-GateJobs)[0..1])
$rpmFailed=New-GateJobs;$rpmFailed[3].conclusion='failure'
Expect-Rejected { Assert-AditReleaseCI 'refs/heads/main' $sha $sha $run $rpmFailed }
$rpmSelf=New-GateJobs;$rpmSelf[2].labels=@('self-hosted','Linux')
Expect-Rejected { Assert-AditReleaseCI 'refs/heads/main' $sha $sha $run $rpmSelf }
$rpmTwice=New-GateJobs;$rpmTwice[3].name='rpm (fedora-43)'
Expect-Rejected { Assert-AditReleaseCI 'refs/heads/main' $sha $sha $run $rpmTwice }
$pr=$run.Clone();$pr.event='pull_request'
Expect-Rejected { Assert-AditReleaseCI 'refs/heads/main' $sha $sha $pr (New-GateJobs) }
$branch=$run.Clone();$branch.head_branch='feature'
Expect-Rejected { Assert-AditReleaseCI 'refs/heads/main' $sha $sha $branch (New-GateJobs) }
$twoWindows=New-GateJobs;$twoWindows[1]=$twoWindows[0].Clone()
Expect-Rejected { Assert-AditReleaseCI 'refs/heads/main' $sha $sha $run $twoWindows }
$twoLinux=New-GateJobs;$twoLinux[0]=$twoLinux[1].Clone()
Expect-Rejected { Assert-AditReleaseCI 'refs/heads/main' $sha $sha $run $twoLinux }
$otherImage=New-GateJobs;$otherImage[1].labels=@('ubuntu-latest')
Expect-Rejected { Assert-AditReleaseCI 'refs/heads/main' $sha $sha $run $otherImage }
$selfLinux=New-GateJobs;$selfLinux[1].labels=@('self-hosted','ubuntu-24.04')
Expect-Rejected { Assert-AditReleaseCI 'refs/heads/main' $sha $sha $run $selfLinux }
$linuxFailed=New-GateJobs;$linuxFailed[1].conclusion='failure'
Expect-Rejected { Assert-AditReleaseCI 'refs/heads/main' $sha $sha $run $linuxFailed }
Write-Output "Release gate: 4 positive checks and $rejected unsafe cases rejected; no network or publishing."
