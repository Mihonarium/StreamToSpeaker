# Uninstalls the Stream To Speaker driver package from the Windows
# driver store. Called from the Inno Setup [UninstallRun] section.
#
# pnputil /delete-driver expects the OEM-assigned name (oemNN.inf) that
# the driver store handed out on /add-driver. We don't know that name
# up front, so this script enumerates the driver store, finds the entry
# whose "Original Name" matches StreamToSpeaker.inf, and uninstalls it.

$ErrorActionPreference = "SilentlyContinue"

$enumOutput = & pnputil /enum-drivers 2>&1 | Out-String
if (-not $enumOutput) {
    Write-Output "pnputil enum-drivers returned no output; driver may already be gone."
    exit 0
}

# pnputil output groups each driver in a stanza with blank-line
# separators. We split on blank lines and look for ones containing
# our INF.
$stanzas = $enumOutput -split "`r?`n`r?`n"
$removed = 0
foreach ($stanza in $stanzas) {
    if ($stanza -match "Original Name:\s*StreamToSpeaker\.inf") {
        if ($stanza -match "Published Name:\s*(oem\d+\.inf)") {
            $oemName = $matches[1]
            Write-Output "Removing driver package $oemName ..."
            & pnputil /delete-driver $oemName /uninstall /force | Out-Null
            $removed++
        }
    }
}

if ($removed -eq 0) {
    Write-Output "No matching driver package found in the driver store."
}
exit 0
