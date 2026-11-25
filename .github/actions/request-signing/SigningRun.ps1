# Which run of the signing workflow a signing request waits on. Dot-sourced
# by action.yml; tested in .github/scripts/ReleaseChain.Tests.ps1.
#
# The dispatch API returns no run id, and a signing run is known only by
# the SHA-256 in its run name. A re-run of a signing job asks for the same
# bytes (same SHA-256) as the attempt before it, so:
#   * a run for that SHA-256 that is still queued or waiting for approval is
#     followed, not asked for again (approve it, and the re-run picks it up);
#   * a run for it that already succeeded is reused (its signed artifact is
#     fetched and verified like a new one), so no second approval is needed;
#   * otherwise a new run is dispatched, and only a run that was not there
#     before the dispatch is taken for it.

function Test-SigningRunFor($run, [string]$sha) {
    return ([string]$run.display_title).ToLower().Contains($sha.ToLower())
}

# The newest run of `$Runs` for `$Sha`, and what to do with it:
# @{ Action = "follow" | "reuse" | "dispatch"; Run = <run or $null> }.
function Select-SigningRun([object[]]$Runs, [string]$Sha) {
    $mine = @($Runs | Where-Object { Test-SigningRunFor $_ $Sha } |
        Sort-Object { [datetime]$_.created_at } -Descending)
    if (-not $mine) { return @{ Action = "dispatch"; Run = $null } }
    $run = $mine[0]
    if ($run.status -ne "completed") { return @{ Action = "follow"; Run = $run } }
    if ($run.conclusion -eq "success") { return @{ Action = "reuse"; Run = $run } }
    # Rejected, cancelled or failed: that request is over; ask again.
    return @{ Action = "dispatch"; Run = $null }
}

# The run a dispatch made: the newest run for `$Sha` whose id was not in
# `$Known` (the runs listed before dispatching), or $null.
function Find-DispatchedRun([object[]]$Runs, [string]$Sha, [object[]]$Known) {
    $ids = @($Known | ForEach-Object { [string]$_ })
    return @($Runs | Where-Object { (Test-SigningRunFor $_ $Sha) -and ($ids -notcontains [string]$_.id) } |
        Sort-Object { [datetime]$_.created_at } -Descending | Select-Object -First 1)[0]
}

# A GET of the GitHub API that rides out a transient failure (a 5xx, a
# rate limit, a dropped connection) instead of failing a job a person has
# already approved. `$Invoke` stands in for Invoke-RestMethod in tests.
function Invoke-GitHubGet([string]$Uri, [hashtable]$Headers, [scriptblock]$Invoke = { param($u, $h) Invoke-RestMethod -Headers $h -Uri $u },
                          [int[]]$Waits = @(5, 15, 30, 60)) {
    for ($i = 0; ; $i++) {
        try {
            return & $Invoke $Uri $Headers
        } catch {
            $code = $null
            if ($_.Exception.Response) { $code = [int]$_.Exception.Response.StatusCode }
            $transient = (-not $code) -or $code -ge 500 -or $code -eq 429
            if (-not $transient -or $i -ge $Waits.Count) { throw }
            Write-Host "GET $Uri failed ($(if ($code) { "HTTP $code" } else { $_.Exception.Message })); retrying in $($Waits[$i]) s"
            Start-Sleep -Seconds $Waits[$i]
        }
    }
}
