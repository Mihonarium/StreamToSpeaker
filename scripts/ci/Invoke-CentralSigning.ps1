# Dispatch the central signing workflow (Mihonarium/signing sign.yml) for
# ONE file that lives in an Actions artifact of the CURRENT run ("Source B"
# in sign.yml - works for private repos via that repo's FETCH_TOKEN), wait
# through the human approval gate + completion, then download the signed
# result. Used twice per release by build.yml's sign-release job: once for
# the service exe, once for the installer.
#
# Env contract (set by the workflow): SIGNING_TOKEN (fine-grained PAT,
# Actions R/W on the signing repo), SIGNING_REPO, SIGNING_WORKFLOW,
# SIGNING_REF, GITHUB_REPOSITORY, GITHUB_RUN_ID.

param(
    [Parameter(Mandatory)] [string]$Sha256,
    [Parameter(Mandatory)] [string]$SourceArtifact,
    [Parameter(Mandatory)] [string]$OutputName,
    [Parameter(Mandatory)] [string]$OutFile,
    [string]$SigDescription = "",
    [string]$SigUrl = "",
    [int]$ApprovalTimeoutMinutes = 60
)

$ErrorActionPreference = "Stop"
if (-not $env:SIGNING_TOKEN) { throw "SIGNING_TOKEN is not set" }
$hdr = @{ Authorization = "Bearer $env:SIGNING_TOKEN"; Accept = "application/vnd.github+json" }
$api = "https://api.github.com/repos/$($env:SIGNING_REPO)"

$inputs = @{
    sha256          = $Sha256
    source_repo     = $env:GITHUB_REPOSITORY
    source_run_id   = "$($env:GITHUB_RUN_ID)"
    source_artifact = $SourceArtifact
    output_name     = $OutputName
}
if ($SigDescription) { $inputs.sig_description = $SigDescription }
if ($SigUrl)         { $inputs.sig_url = $SigUrl }
$body = @{ ref = $env:SIGNING_REF; inputs = $inputs } | ConvertTo-Json
Invoke-RestMethod -Method Post -Headers $hdr -Body $body -Uri `
    "$api/actions/workflows/$($env:SIGNING_WORKFLOW)/dispatches"
Write-Host "Dispatched signing of $OutputName (sha256 $Sha256)"

# The run name starts "Sign sha256 <digest> ..." - find ours by digest.
$run = $null
foreach ($i in 1..30) {
    Start-Sleep -Seconds 10
    $runs = (Invoke-RestMethod -Headers $hdr -Uri `
        "$api/actions/workflows/$($env:SIGNING_WORKFLOW)/runs?event=workflow_dispatch&per_page=10").workflow_runs
    $run = $runs | Where-Object { $_.display_title -match $Sha256 } | Select-Object -First 1
    if ($run) { break }
}
if (-not $run) { throw "signing run did not appear within 5 minutes" }
Write-Host "Signing run: $($run.html_url)"
Write-Host "##[notice]Approve the signing run for $OutputName - its run name must show sha256 $Sha256."

$deadline = (Get-Date).AddMinutes($ApprovalTimeoutMinutes)
while ($true) {
    Start-Sleep -Seconds 20
    $run = Invoke-RestMethod -Headers $hdr -Uri "$api/actions/runs/$($run.id)"
    if ($run.status -eq "completed") { break }
    if ((Get-Date) -gt $deadline) {
        throw "timed out after $ApprovalTimeoutMinutes min waiting for the signing run ($($run.status)) - approve it and re-run this job"
    }
}
if ($run.conclusion -ne "success") { throw "signing run concluded '$($run.conclusion)' - see $($run.html_url)" }

$arts = (Invoke-RestMethod -Headers $hdr -Uri "$api/actions/runs/$($run.id)/artifacts").artifacts
$signedArt = $arts | Where-Object { $_.name -eq "signed" } | Select-Object -First 1
if (-not $signedArt) { throw "run has no 'signed' artifact" }
$zip = Join-Path ([IO.Path]::GetTempPath()) "signed-$($run.id).zip"
$dir = Join-Path ([IO.Path]::GetTempPath()) "signed-$($run.id)"
Invoke-WebRequest -Uri $signedArt.archive_download_url -Headers $hdr -OutFile $zip
if (Test-Path $dir) { Remove-Item -Recurse -Force $dir }
Expand-Archive $zip -DestinationPath $dir
$file = Get-ChildItem $dir -Filter $OutputName | Select-Object -First 1
if (-not $file) { throw "signed artifact does not contain $OutputName" }
Move-Item $file.FullName $OutFile -Force
Remove-Item $zip -Force
Write-Host "Signed $OutputName -> $OutFile"
