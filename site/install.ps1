# Installs rune.exe from a published release, after checking it against the
# release's SHA256SUMS.
#
#   irm https://rune.heyaadi.com/install.ps1 | iex
#
# Settings: $env:RUNE_VERSION (default: the latest release) and
# $env:RUNE_INSTALL_DIR (default: $env:LOCALAPPDATA\Programs\rune).
$ErrorActionPreference = 'Stop'

# Run as a file, a failure exits 1. Piped into iex, exit would close the
# window the user typed in, so the failure is thrown instead.
$RunAsFile = [bool] $PSCommandPath
function Stop-Install([string] $Message) {
    if ($RunAsFile) {
        [Console]::Error.WriteLine("rune: $Message")
        exit 1
    }
    throw "rune: $Message"
}

$BaseUrl = if ($env:RUNE_INSTALL_BASE_URL) { $env:RUNE_INSTALL_BASE_URL } else { 'https://github.com/AadiXC0DE/Rune/releases' }
$Dir = if ($env:RUNE_INSTALL_DIR) { $env:RUNE_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\rune' }
$Arch = if ($env:RUNE_INSTALL_ARCH) { $env:RUNE_INSTALL_ARCH } else { $env:PROCESSOR_ARCHITECTURE }
if ($Arch -ne 'AMD64') {
    Stop-Install "no release for Windows $Arch"
}
$Target = 'x86_64-pc-windows-msvc'

# The default address is fetched over TLS 1.2, which Windows PowerShell 5.1 does
# not always offer by itself.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$Version = $env:RUNE_VERSION
if (-not $Version) {
    try {
        $Request = [Net.WebRequest]::Create("$BaseUrl/latest")
        $Request.AllowAutoRedirect = $true
        $Response = $Request.GetResponse()
        $Version = $Response.ResponseUri.Segments[-1]
        $Response.Close()
    } catch {
        Stop-Install "could not find the latest release at $BaseUrl/latest"
    }
}
$Version = $Version.TrimStart('v')

$Archive = "rune-$Version-$Target.zip"
$Work = Join-Path ([IO.Path]::GetTempPath()) ("rune-install-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $Work | Out-Null
try {
    $ArchivePath = Join-Path $Work $Archive
    $SumsPath = Join-Path $Work 'SHA256SUMS'
    try {
        Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/download/v$Version/$Archive" -OutFile $ArchivePath
        Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/download/v$Version/SHA256SUMS" -OutFile $SumsPath
    } catch {
        Stop-Install "could not download $Archive for version $Version"
    }

    $Expected = $null
    foreach ($Line in Get-Content -Path $SumsPath) {
        $Fields = $Line -split '\s+', 2
        if ($Fields.Count -eq 2 -and $Fields[1].TrimStart('*') -eq $Archive) {
            $Expected = $Fields[0].ToLowerInvariant()
            break
        }
    }
    $Actual = (Get-FileHash -Algorithm SHA256 -Path $ArchivePath).Hash.ToLowerInvariant()
    if (-not $Expected -or $Expected -ne $Actual) {
        Stop-Install "checksum mismatch for $Archive; nothing was installed"
    }

    $Unpacked = Join-Path $Work 'unpacked'
    Expand-Archive -Path $ArchivePath -DestinationPath $Unpacked
    $Binary = Join-Path $Unpacked 'rune.exe'
    if (-not (Test-Path -PathType Leaf $Binary)) {
        Stop-Install "$Archive holds no rune.exe; nothing was installed"
    }

    try {
        New-Item -ItemType Directory -Force -Path $Dir | Out-Null
        $Staged = Join-Path $Dir ".rune.new.$PID.exe"
        Copy-Item -Path $Binary -Destination $Staged -Force
        Move-Item -Path $Staged -Destination (Join-Path $Dir 'rune.exe') -Force
    } catch {
        Stop-Install "cannot write to $Dir; choose a directory you own with RUNE_INSTALL_DIR"
    }
} finally {
    Remove-Item -Recurse -Force -Path $Work -ErrorAction SilentlyContinue
}

Write-Output "installed rune $Version to $(Join-Path $Dir 'rune.exe')"
$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not (($UserPath -split ';') -contains $Dir)) {
    Write-Output "[Environment]::SetEnvironmentVariable('Path', '$Dir;' + [Environment]::GetEnvironmentVariable('Path', 'User'), 'User')"
}
Write-Output 'next: rune connect'
Write-Output 'uninstall: rune uninstall'
