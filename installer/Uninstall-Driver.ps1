# Uninstalls the Stream To Speaker driver package from the Windows
# driver store. Called from the Inno Setup [UninstallRun] section.
#
# pnputil /delete-driver expects the OEM-assigned name (oemNN.inf) that
# the driver store handed out on /add-driver. We don't know that name
# up front, so this script finds every published package that declares
# our hardware ID (Root\StreamToSpeaker) and uninstalls it.

$ErrorActionPreference = "SilentlyContinue"

# Published names (oemNN.inf) of every Stream To Speaker package in the
# driver store. pnputil /enum-drivers is not parsed: its labels
# ("Original Name:", "Published Name:") are translated on non-English
# Windows. Instead: every published package has its INF copied to
# %windir%\INF\oemNN.inf, and ours is the one declaring our hardware ID.
# The same function is in installer/Uninstall-Driver.ps1,
# scripts/Pre-Install.ps1 and scripts/Reset-Install.ps1; keep them equal.
function Get-StreamToSpeakerDriverPackages {
    $infDir = Join-Path $env:windir 'INF'
    @(Get-ChildItem -Path $infDir -Filter 'oem*.inf' -ErrorAction SilentlyContinue |
        Where-Object { Select-String -Path $_.FullName -Pattern 'Root\StreamToSpeaker' -SimpleMatch -Quiet -ErrorAction SilentlyContinue } |
        ForEach-Object { $_.Name } |
        Sort-Object)
}

$removed = 0
foreach ($oemName in (Get-StreamToSpeakerDriverPackages)) {
    Write-Output "Removing driver package $oemName ..."
    & pnputil /delete-driver $oemName /uninstall /force | Out-Null
    $removed++
}

if ($removed -eq 0) {
    Write-Output "No matching driver package found in the driver store."
}
exit 0
