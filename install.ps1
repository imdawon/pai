# Installs pai on Windows: downloads the release binary for this CPU, checks it against
# SHA256SUMS, and puts it in %LOCALAPPDATA%\pai (or $env:PAI_INSTALL_DIR).
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$base = if ($env:PAI_DOWNLOAD_BASE) { $env:PAI_DOWNLOAD_BASE.TrimEnd('/') } else { 'https://privacyai.example/pai/latest' }
$dir = if ($env:PAI_INSTALL_DIR) { $env:PAI_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'pai' }

# A 32-bit or emulated PowerShell reports the real CPU in PROCESSOR_ARCHITEW6432.
$cpu = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
$arch = switch ($cpu) {
    'AMD64' { 'x86_64' }
    'ARM64' { 'aarch64' }
    default { throw "pai install: unsupported CPU $cpu" }
}
$asset = "pai-$arch-pc-windows-msvc.exe"

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("pai-install-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    Write-Host "Downloading $asset from $base"
    $exe = Join-Path $tmp 'pai.exe'
    $sums = Join-Path $tmp 'SHA256SUMS'
    Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $exe
    Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile $sums

    $expected = $null
    foreach ($line in Get-Content $sums) {
        $parts = $line.Trim() -split '\s+', 2
        if ($parts.Count -eq 2 -and $parts[1].TrimStart('*') -eq $asset) { $expected = $parts[0].ToLowerInvariant(); break }
    }
    if (-not $expected) { throw "pai install: SHA256SUMS has no entry for $asset" }
    $actual = (Get-FileHash -Algorithm SHA256 -Path $exe).Hash.ToLowerInvariant()
    if ($actual -ne $expected) { throw "pai install: checksum mismatch for $asset (expected $expected, got $actual); not installing" }

    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $target = Join-Path $dir 'pai.exe'
    Move-Item -Force -Path $exe -Destination $target
    Write-Host "Installed $(& $target --version) to $target"

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not (($userPath -split ';') -contains $dir)) {
        $newPath = if ($userPath) { "$userPath;$dir" } else { $dir }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        $env:Path = "$env:Path;$dir"
        Write-Host "Added $dir to your PATH. Open a new terminal to use pai there."
    }
}
finally {
    Remove-Item -Recurse -Force -Path $tmp -ErrorAction SilentlyContinue
}
