# Install (or update) Sard on Windows from the latest GitHub release:
#
#   irm https://raw.githubusercontent.com/parodyband/StrangeApeRemoteDesktop/main/install.ps1 | iex
#
# Installs into %LOCALAPPDATA%\Programs\Sard (no administrator rights), adds a
# Start menu entry and puts sard.exe on your PATH, then starts the tray app.
# The download is checked against the SHA-256 in the release manifest.
#
# Options, as environment variables (set them before piping into iex):
#   $env:SARD_NO_START = 1     don't start Sard after installing
#   $env:SARD_UNINSTALL = 1    remove Sard again (pairings in %APPDATA%\sard stay)
#   $env:SARD_RELEASE_URL      where latest.txt is (testing)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'  # Invoke-WebRequest is very slow with the progress bar
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$repo = 'parodyband/StrangeApeRemoteDesktop'
$base = if ($env:SARD_RELEASE_URL) { $env:SARD_RELEASE_URL.TrimEnd('/') } else { "https://github.com/$repo/releases/latest/download" }
$dir = Join-Path $env:LOCALAPPDATA 'Programs\Sard'
$exe = Join-Path $dir 'sard.exe'
$launcher = Join-Path $dir 'sardw.exe'
$shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'Sard.lnk'

function Stop-Sard {
    Get-Process -Name sard, sardw -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and $_.Path.StartsWith($dir, [StringComparison]::OrdinalIgnoreCase) } |
        ForEach-Object { $_ | Stop-Process -Force; $_.WaitForExit(5000) | Out-Null }
}

function Set-UserPath([bool]$add) {
    $path = [Environment]::GetEnvironmentVariable('Path', 'User')
    $parts = @($path -split ';' | Where-Object { $_ -and $_.TrimEnd('\') -ne $dir.TrimEnd('\') })
    if ($add) { $parts += $dir }
    [Environment]::SetEnvironmentVariable('Path', ($parts -join ';'), 'User')
}

if ($env:SARD_UNINSTALL) {
    Stop-Sard
    Remove-Item -Force -ErrorAction SilentlyContinue $shortcut
    Remove-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'Sard' -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $dir
    Set-UserPath $false
    Write-Host "Sard removed (your pairings in $env:APPDATA\sard are kept)."
    return
}

if (-not [Environment]::Is64BitOperatingSystem) { throw 'Sard needs 64-bit Windows 10 or 11.' }
if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { Write-Warning 'There is no ARM64 build of Sard yet; the x64 build runs under emulation without GPU video acceleration.' }

$manifest = (Invoke-WebRequest -UseBasicParsing "$base/latest.txt").Content
if ($manifest -is [byte[]]) { $manifest = [Text.Encoding]::UTF8.GetString($manifest) }
$lines = $manifest -split "`n" | ForEach-Object { $_.Trim() }
if ($lines[0] -ne 'sard-update 1') { throw "unexpected release manifest from $base" }
$version = ($lines | Where-Object { $_ -like 'version *' } | Select-Object -First 1) -replace '^version ', ''
$asset = $lines | Where-Object { $_ -like 'asset windows-x86_64 *' } | Select-Object -First 1
if (-not $asset) { throw "release $version has no Windows build" }
$f = $asset -split '\s+'
$sha, $size, $url = $f[2].ToLower(), [int64]$f[3], $f[4]
if ($url -notmatch '^https://' -and -not $env:SARD_RELEASE_URL) { throw "refusing to download $url" }

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("sard-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    $zip = Join-Path $tmp 'sard.zip'
    Write-Host "Downloading Sard $version ..."
    Invoke-WebRequest -UseBasicParsing $url -OutFile $zip
    if ((Get-Item $zip).Length -ne $size) { throw 'download has the wrong size; not installing' }
    if ((Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower() -ne $sha) { throw 'download does not match the SHA-256 in the release manifest; not installing' }
    Expand-Archive -Path $zip -DestinationPath (Join-Path $tmp 'x')
    $src = Get-ChildItem -Directory (Join-Path $tmp 'x') | Select-Object -First 1
    if (-not $src -or -not (Test-Path (Join-Path $src.FullName 'sard.exe'))) { throw 'unexpected archive layout' }

    Stop-Sard
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    Copy-Item -Force (Join-Path $src.FullName '*') $dir
    # Leftovers of a self-update.
    Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $dir 'sard.exe.old'), (Join-Path $dir 'sardw.exe.old')
} finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $tmp
}

$target = if (Test-Path $launcher) { $launcher } else { $exe }
$shell = New-Object -ComObject WScript.Shell
$lnk = $shell.CreateShortcut($shortcut)
$lnk.TargetPath = $target
if ($target -eq $exe) { $lnk.Arguments = 'app'; $lnk.WindowStyle = 7 }
$lnk.WorkingDirectory = $dir
$lnk.IconLocation = "$exe,0"
$lnk.Description = 'Sard remote desktop'
$lnk.Save()
Set-UserPath $true

Write-Host "Installed Sard $version in $dir"
Write-Host "Find it in the Start menu and the notification area (system tray). In a new terminal: sard help"
Write-Host "Windows Firewall asks whether Sard may accept connections the first time it shares this PC: allow it on private networks."
if (-not $env:SARD_NO_START) {
    if ($target -eq $launcher) { Start-Process -FilePath $launcher } else { Start-Process -FilePath $exe -ArgumentList 'app' -WindowStyle Hidden }
}
