# clipkeep installer for Windows. Downloads the latest ready-made release (no Rust needed),
# puts it into %LOCALAPPDATA%\Programs\clipkeep, adds that folder to your PATH and runs
# `clipkeep setup` (autostart + hotkey). No administrator rights needed.
#
#   irm https://raw.githubusercontent.com/OWNER/clipkeep/main/install.ps1 | iex
#
# Options (environment): $env:CLIPKEEP_NO_SETUP = 1    $env:CLIPKEEP_REPO = 'owner/name'

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue' # the progress bar makes Invoke-WebRequest very slow on PowerShell 5

$repo = if ($env:CLIPKEEP_REPO) { $env:CLIPKEEP_REPO } else { 'OWNER/clipkeep' }
$base = if ($env:CLIPKEEP_BASE_URL) { $env:CLIPKEEP_BASE_URL } else { "https://github.com/$repo/releases/latest/download" }
$name = 'clipkeep-windows-x86_64'
$dir = Join-Path $env:LOCALAPPDATA 'Programs\clipkeep'

if (-not [Environment]::Is64BitOperatingSystem) {
    throw "clipkeep: only 64-bit Windows is supported"
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("clipkeep-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    # PowerShell 5 on older Windows does not enable TLS 1.2 by default.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $url = "$base/$name.zip"
    Write-Host "Downloading $url"
    Invoke-WebRequest -Uri $url -OutFile (Join-Path $tmp 'clipkeep.zip') -UseBasicParsing
    Expand-Archive -Path (Join-Path $tmp 'clipkeep.zip') -DestinationPath $tmp -Force
    $src = Join-Path $tmp $name
    if (-not (Test-Path (Join-Path $src 'clipkeep.exe'))) { throw "clipkeep: the archive has no clipkeep.exe" }

    # A running daemon locks the old .exe; stop it first, `setup` starts the new one.
    $old = Join-Path $dir 'clipkeep.exe'
    if (Test-Path $old) {
        try { & $old stop *> $null } catch { }
        Start-Sleep -Milliseconds 700
    }
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    Copy-Item -Path (Join-Path $src '*.exe') -Destination $dir -Force
    Copy-Item -Path (Join-Path $src 'README.md'), (Join-Path $src 'LICENSE') -Destination $dir -Force
    # We downloaded the files ourselves; drop the "came from the internet" mark if any.
    Get-ChildItem -Path $dir -Filter '*.exe' | Unblock-File

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $userPath) { $userPath = '' }
    if (($userPath -split ';') -notcontains $dir) {
        $newPath = (($userPath.TrimEnd(';'), $dir) | Where-Object { $_ }) -join ';'
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        $env:Path = "$env:Path;$dir"
        Write-Host "Added $dir to your PATH (new terminals see it)."
    }
    Write-Host "Installed: $dir\clipkeep.exe"

    if (-not $env:CLIPKEEP_NO_SETUP) {
        & (Join-Path $dir 'clipkeep.exe') setup
    }
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
