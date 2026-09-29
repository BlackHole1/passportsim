# Installs the PassportSim CLI on Windows x64 from the project's GitHub Releases.
#
#   irm https://passportsim.bugs.cc/install.ps1 | iex
#
# It reads the release's SHA256SUMS.txt, takes the one archive named
# passportsim-<version>-windows-x64.zip, checks its SHA-256, unpacks the package into
# %LOCALAPPDATA%\Programs\passportsim\<version>\ and puts that directory on the user PATH,
# replacing the entry of any other version this script installed. It needs no administrator
# rights. Windows PowerShell 5.1 and PowerShell 7 both run it.
#
# Environment:
#   PASSPORTSIM_VERSION         the release to install, such as 0.1.0 (default: the latest release)
#   PASSPORTSIM_NO_MODIFY_PATH  set to 1 to leave the user PATH alone
#   PASSPORTSIM_RELEASE_URL     for testing only: a URL holding the release's files, used instead
#                               of the GitHub download URL; needs PASSPORTSIM_VERSION
#
# Errors are thrown rather than ending with `exit`, which would close the window `iex` runs in.

function Install-PassportSim {
    # An installer run by hand talks to the console; what it prints is not pipeline data.
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '')]
    param()

    $ErrorActionPreference = 'Stop'
    # Windows PowerShell 5.1 redraws its progress bar for every block, which makes a download
    # many times slower.
    $ProgressPreference = 'SilentlyContinue'

    $repo = 'BlackHole1/passportsim'
    $prefix = 'passportsim-install:'

    if ($PSVersionTable.PSEdition -eq 'Core' -and -not $IsWindows) {
        throw "$prefix this installer is for Windows x64; on macOS (Apple silicon) run: curl -fsSL https://passportsim.bugs.cc/install.sh | sh"
    }
    # The machine's own architecture: a 32-bit or emulated PowerShell reports its own instead.
    $arch = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Environment').PROCESSOR_ARCHITECTURE
    if ($arch -ne 'AMD64') {
        throw "$prefix PassportSim runs on Windows only on x64, and this machine is $arch; the browser version at https://passportsim.bugs.cc needs no install"
    }

    if ($PSVersionTable.PSEdition -ne 'Core') {
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    }

    $override = $env:PASSPORTSIM_RELEASE_URL
    if ($env:PASSPORTSIM_VERSION) {
        $version = $env:PASSPORTSIM_VERSION -replace '^v', ''
    } elseif ($override) {
        throw "$prefix PASSPORTSIM_RELEASE_URL needs PASSPORTSIM_VERSION, the version of the release it holds"
    } else {
        try {
            $release = Invoke-RestMethod -UseBasicParsing -Headers @{ 'User-Agent' = 'passportsim-install' } `
                -Uri "https://api.github.com/repos/$repo/releases/latest"
        } catch {
            throw "$prefix cannot read the latest release of https://github.com/$repo ($($_.Exception.Message)); if it has no published release yet, set PASSPORTSIM_VERSION to install a pre-release"
        }
        $version = "$($release.tag_name)" -replace '^v', ''
    }
    if ($version -notmatch '^[0-9A-Za-z.+-]+$') {
        throw "$prefix '$version' is not a version, such as 0.1.0"
    }

    if ($override) {
        $base = $override.TrimEnd('/')
    } else {
        $base = "https://github.com/$repo/releases/download/v$version"
    }
    $root = Join-Path $env:LOCALAPPDATA 'Programs\passportsim'
    $dest = Join-Path $root $version

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("passportsim-install-" + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    $partial = Join-Path $root (".partial-" + [Guid]::NewGuid().ToString('N'))
    try {
        Write-Host "$prefix installing PassportSim $version from $base"
        $sums = Join-Path $tmp 'SHA256SUMS.txt'
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS.txt" -OutFile $sums
        } catch {
            throw "$prefix cannot download $base/SHA256SUMS.txt ($($_.Exception.Message)); is $version a published release?"
        }

        # `sha256sum` lines: `<hex>  <name>`, or `<hex> *<name>` in binary mode.
        $matched = @(Get-Content -LiteralPath $sums | ForEach-Object {
            if ($_.TrimEnd("`r") -match '^([0-9A-Fa-f]{64}) [ *](passportsim-[^/\\]*-windows-x64\.zip)$') {
                [pscustomobject]@{ Hash = $Matches[1].ToLowerInvariant(); Name = $Matches[2] }
            }
        })
        if ($matched.Count -ne 1) {
            throw "$prefix SHA256SUMS.txt of $version lists $($matched.Count) archives named passportsim-<version>-windows-x64.zip, expected one"
        }
        $asset = $matched[0].Name
        $zip = Join-Path $tmp $asset
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $zip
        } catch {
            throw "$prefix cannot download $base/$asset ($($_.Exception.Message))"
        }
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $zip).Hash.ToLowerInvariant()
        if ($actual -ne $matched[0].Hash) {
            throw "$prefix $asset has SHA-256 $actual, but SHA256SUMS.txt says $($matched[0].Hash); nothing was installed"
        }
        Write-Host "$prefix verified $asset (sha256 $actual)"

        # Unpack beside the destination, so the final move stays on one volume.
        New-Item -ItemType Directory -Force -Path $partial | Out-Null
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        [IO.Compression.ZipFile]::ExtractToDirectory($zip, $partial)
        $top = @(Get-ChildItem -LiteralPath $partial -Force)
        if ($top.Count -ne 1 -or -not (Test-Path -LiteralPath (Join-Path $top[0].FullName 'passportsim.exe') -PathType Leaf)) {
            throw "$prefix $asset does not hold one package directory with passportsim.exe"
        }
        if (Test-Path -LiteralPath $dest) {
            try {
                Remove-Item -LiteralPath $dest -Recurse -Force
            } catch {
                throw "$prefix cannot replace $dest ($($_.Exception.Message)); stop PassportSim (passportsim serve --stop) and run again"
            }
        }
        Move-Item -LiteralPath $top[0].FullName -Destination $dest
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $partial -Recurse -Force -ErrorAction SilentlyContinue
    }

    $exe = Join-Path $dest 'passportsim.exe'
    Write-Host "$prefix installed the package in $dest"
    & $exe --version
    if ($LASTEXITCODE -ne 0) {
        throw "$prefix the installed binary did not run: $exe --version"
    }

    if ($env:PASSPORTSIM_NO_MODIFY_PATH -eq '1') {
        Write-Host "$prefix PASSPORTSIM_NO_MODIFY_PATH is set, so the user PATH is unchanged. Add $dest to it, or run $exe"
        return
    }

    # Read and write the registry value itself: [Environment]::SetEnvironmentVariable would store
    # the user PATH expanded, losing every %VARIABLE% in it.
    $key = Get-Item -LiteralPath 'HKCU:\Environment'
    $raw = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    $rootPrefix = $root.TrimEnd('\') + '\'
    $kept = @()
    $present = $false
    foreach ($entry in ($raw -split ';')) {
        if ($entry -eq '') { continue }
        $expanded = [Environment]::ExpandEnvironmentVariables($entry).TrimEnd('\')
        if ($expanded -ieq $dest) {
            $present = $true
            $kept += $entry
        } elseif (-not $expanded.StartsWith($rootPrefix, [StringComparison]::OrdinalIgnoreCase)) {
            $kept += $entry
        }
    }
    if (-not $present) {
        $kept += $dest
    }
    $updated = $kept -join ';'
    if ($updated -ne $raw) {
        New-ItemProperty -Path 'HKCU:\Environment' -Name 'Path' -Value $updated -PropertyType ExpandString -Force | Out-Null
        # Setting any user variable through .NET broadcasts WM_SETTINGCHANGE, so windows opened
        # from now on see the new PATH.
        [Environment]::SetEnvironmentVariable('PASSPORTSIM_INSTALL_BROADCAST', '1', 'User')
        [Environment]::SetEnvironmentVariable('PASSPORTSIM_INSTALL_BROADCAST', $null, 'User')
        Write-Host "$prefix added $dest to the user PATH"
    } else {
        Write-Host "$prefix $dest is already on the user PATH"
    }
    # This session too, when `iex` runs the script in the user's own shell.
    $session = @($env:Path -split ';' | Where-Object {
        $_ -ne '' -and -not $_.TrimEnd('\').StartsWith($rootPrefix, [StringComparison]::OrdinalIgnoreCase)
    })
    $env:Path = (@($dest) + $session) -join ';'
    Write-Host "$prefix open a new terminal and run: passportsim --help"
}

Install-PassportSim
