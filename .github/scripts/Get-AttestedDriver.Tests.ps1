#requires -Version 7
# Pester 5 tests of Get-AttestedDriver.ps1 against a fake `gh` (which
# driver-v* release a build bundles, and that found=false means only "no
# certified or attested release records this source hash"), and of
# Assert-MicrosoftSigned.ps1. Run by
# build.yml's chain-tests-powershell job:
#   Invoke-Pester -Path .github/scripts/Get-AttestedDriver.Tests.ps1 -Output Detailed -CI

BeforeAll {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $Script = Join-Path $PSScriptRoot "Get-AttestedDriver.ps1"
    $Hash = "5" * 64
    $Other = "6" * 64

    # A driver package zip holding the given {name = bytes}.
    function New-Zip([hashtable]$files) {
        $dir = Join-Path $TestDrive ([guid]::NewGuid().ToString("N"))
        New-Item -ItemType Directory -Path $dir | Out-Null
        foreach ($name in $files.Keys) { [IO.File]::WriteAllBytes((Join-Path $dir $name), $files[$name]) }
        $zip = "$dir.zip"
        [System.IO.Compression.ZipFile]::CreateFromDirectory($dir, $zip)
        return [IO.File]::ReadAllBytes($zip)
    }

    function Get-Sha256([byte[]]$bytes) {
        $sha = [Security.Cryptography.SHA256]::Create()
        return ([BitConverter]::ToString($sha.ComputeHash($bytes)) -replace "-", "").ToLower()
    }

    function New-Package {
        New-Zip @{ "StreamToSpeaker.inf" = [byte[]](1, 2); "StreamToSpeaker.sys" = [byte[]](3, 4); "StreamToSpeaker.cat" = [byte[]](5, 6) }
    }

    # A driver release: its manifest and its assets ({name = bytes}).
    function New-DriverRelease([string]$version, [string]$kind, [string]$sourceHash, [byte[]]$zip, [switch]$NoManifest) {
        $build = [int]($version -split "\.")[-1]
        $signed = "StreamToSpeaker-Driver-$version-Signed.zip"
        $m = [ordered]@{ schema = 1; driver_version = $version; driver_build = $build; source_hash = $sourceHash
            attested = ($kind -eq "attested"); certified = ($kind -eq "certified")
            signed_zip = $signed; signed_zip_sha256 = (Get-Sha256 $zip) }
        $assets = @{ $signed = $zip }
        if (-not $NoManifest) { $assets["manifest.json"] = [Text.Encoding]::UTF8.GetBytes(($m | ConvertTo-Json)) }
        return @{ tag_name = "driver-v$version"; draft = $false; assets = $assets }
    }

    # The fake gh: `gh api --paginate .../releases ...` lists $Releases as
    # the script's --jq would print them; `gh release download` writes an
    # asset. $Fail names calls that fail (exit code 1).
    function gh {
        $a = @($args)
        if ($a[0] -eq "api" -and ($a -join " ") -like "*releases?per_page=100*") {
            if ($Fail -contains "list") { $global:LASTEXITCODE = 1; return }
            $global:LASTEXITCODE = 0
            foreach ($r in $Releases) {
                [ordered]@{ tag_name = $r.tag_name; draft = $r.draft; assets = @($r.assets.Keys) } | ConvertTo-Json -Compress
            }
            return
        }
        if ($a[0] -eq "release" -and $a[1] -eq "download") {
            $tag = $a[2]
            $pattern = $a[[array]::IndexOf($a, "--pattern") + 1]
            $out = $a[[array]::IndexOf($a, "--output") + 1]
            $r = $Releases | Where-Object { $_.tag_name -eq $tag }
            if ($Fail -contains "download:$tag/$pattern" -or -not $r -or -not $r.assets.ContainsKey($pattern)) {
                $global:LASTEXITCODE = 1; return
            }
            [IO.File]::WriteAllBytes($out, $r.assets[$pattern])
            $global:LASTEXITCODE = 0
            return
        }
        throw "unexpected gh call: $($a -join ' ')"
    }

    function Invoke-Lookup([switch]$WarnOnError) {
        $env:GITHUB_OUTPUT = Join-Path $TestDrive ([guid]::NewGuid().ToString("N") + ".out")
        $out = Join-Path $TestDrive ([guid]::NewGuid().ToString("N"))
        & $Script -SourceHash $Hash -OutDir $out -Repo "owner/repo" -WarnOnError:$WarnOnError | Out-Null
        $outputs = @{}
        if (Test-Path $env:GITHUB_OUTPUT) {
            foreach ($line in Get-Content $env:GITHUB_OUTPUT) { $k, $v = $line -split "=", 2; $outputs[$k] = $v }
        }
        return $outputs
    }
}

Describe "Get-AttestedDriver.ps1" {
    BeforeEach {
        $Fail = @()
        $Zip = New-Package
    }

    It "prefers a certified driver over an attested one with a higher build" {
        $Releases = @(
            (New-DriverRelease "1.1.0.250" "attested" $Hash $Zip),
            (New-DriverRelease "1.1.1.211" "certified" $Hash $Zip))
        $o = Invoke-Lookup
        $o.found | Should -Be "true"
        $o.kind | Should -Be "certified"
        $o.tag | Should -Be "driver-v1.1.1.211"
    }

    It "takes the highest build within one kind" {
        $Releases = @(
            (New-DriverRelease "1.1.0.204" "attested" $Hash $Zip),
            (New-DriverRelease "1.1.0.230" "attested" $Hash $Zip))
        (Invoke-Lookup).tag | Should -Be "driver-v1.1.0.230"
    }

    It "answers found=false when no certified or attested release records the source hash" {
        $Releases = @(
            (New-DriverRelease "1.1.1.211" "certified" $Other $Zip),
            (New-DriverRelease "1.1.2.140" "none" $Hash $Zip))
        (Invoke-Lookup).found | Should -Be "false"
    }

    It "skips a release without a manifest" {
        $Releases = @((New-DriverRelease "1.1.1.211" "certified" $Hash $Zip -NoManifest))
        (Invoke-Lookup).found | Should -Be "false"
    }

    It "stops when a manifest can't be downloaded, instead of answering found=false" {
        $Releases = @((New-DriverRelease "1.1.1.211" "certified" $Hash $Zip))
        $Fail = @("download:driver-v1.1.1.211/manifest.json")
        { Invoke-Lookup } | Should -Throw "*manifest.json*"
    }

    It "stops when the release list can't be read; -WarnOnError makes that found=false" {
        $Releases = @((New-DriverRelease "1.1.1.211" "certified" $Hash $Zip))
        $Fail = @("list")
        { Invoke-Lookup } | Should -Throw "*listing the releases*"
        (Invoke-Lookup -WarnOnError).found | Should -Be "false"
    }

    It "fails on a package that does not hash to its manifest, even with -WarnOnError" {
        $rel = New-DriverRelease "1.1.1.211" "certified" $Hash $Zip
        $rel.assets["StreamToSpeaker-Driver-1.1.1.211-Signed.zip"] = (New-Zip @{ "StreamToSpeaker.inf" = [byte[]](9) })
        $Releases = @($rel)
        { Invoke-Lookup -WarnOnError } | Should -Throw "*refusing to use it*"
    }

    It "extracts exactly the package files and reports the release" {
        $Releases = @((New-DriverRelease "1.1.1.211" "certified" $Hash $Zip))
        $o = Invoke-Lookup
        $o.version | Should -Be "1.1.1.211"
        $o.build | Should -Be "211"
        foreach ($name in "StreamToSpeaker.inf", "StreamToSpeaker.sys", "StreamToSpeaker.cat") {
            Join-Path (Split-Path $o.zip) $name | Should -Exist
        }
    }
}

Describe "Assert-MicrosoftSigned.ps1" {
    BeforeAll {
        $Assert = Join-Path $PSScriptRoot "Assert-MicrosoftSigned.ps1"
        $env:GITHUB_STEP_SUMMARY = $null
    }

    It "accepts a file Microsoft signed" {
        { & $Assert -Path (Join-Path $env:SystemRoot "System32\notepad.exe") } | Should -Not -Throw
    }

    It "refuses an unsigned file" {
        $file = Join-Path $TestDrive "devcon.exe"
        [IO.File]::WriteAllBytes($file, [byte[]](77, 90, 0, 0))
        { & $Assert -Path $file } | Should -Throw "*no valid Authenticode signature*"
    }
}
