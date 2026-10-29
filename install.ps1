# Installs pai on Windows: downloads the release binary for this CPU, checks the Ed25519 signature
# on SHA256SUMS with the pai release key below, checks the binary against SHA256SUMS, and puts it
# in %LOCALAPPDATA%\pai (or $env:PAI_INSTALL_DIR).
#
#   install.ps1                                  install only
#   install.ps1 pair <code> --setup <harness>    then run `pai pair <code> --setup <harness>`:
#                                                the one command the DawonAI web app shows
$ErrorActionPreference = 'Stop'
$paiArgs = @($args)
if ($paiArgs.Count -gt 0 -and $paiArgs[0] -ne 'pair') {
    throw "pai install: the only command this installer runs after installing is ``pai pair``, not ``$($paiArgs[0])``"
}
$ProgressPreference = 'SilentlyContinue'

# The pai release key (release/signing-key.pub), written here by release/embed-key.sh. The same
# key is in pai's README on GitHub and on https://privacyai.example/pai/.
$signingKeySpki = 'MCowBQYDK2VwAyEAAaxr74KfLKGdOR5jPlOo1ngZVAIOOafRkstl+2po5Cs='
$signingKeySsh = 'AAAAC3NzaC1lZDI1NTE5AAAAIAGsa++CnyyhnTkeYz5TqNZ4GVQCDjmn0ZLLZftqaOQr'
# Names this key's signature files, SHA256SUMS.<id>.sig and .sshsig.
$signingKeyId = 'f0a52c411efb5295'

if (-not $signingKeySpki -or -not $signingKeySsh -or -not $signingKeyId) {
    throw "pai install: this copy of install.ps1 has no pai release key, so it can't check the download. Use https://api.privacyai.example/pai/latest/install.ps1"
}

# The model API's host serves the release, through the relay to the model server, so the download
# doesn't pass through Cloudflare, which challenges scripts from datacenter and VPN addresses.
$base = if ($env:PAI_DOWNLOAD_BASE) { $env:PAI_DOWNLOAD_BASE.TrimEnd('/') } else { 'https://api.privacyai.example/pai/latest' }
$dir = if ($env:PAI_INSTALL_DIR) { $env:PAI_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'pai' }

# A 32-bit or emulated PowerShell reports the real CPU in PROCESSOR_ARCHITEW6432.
$cpu = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
$arch = switch ($cpu) {
    'AMD64' { 'x86_64' }
    'ARM64' { 'aarch64' }
    default { throw "pai install: unsupported CPU $cpu" }
}
$asset = "pai-$arch-pc-windows-msvc.exe"

# Runs a program and returns its exit code and output. Standard input, when given, is a file's
# bytes exactly: a PowerShell pipe to a program re-encodes text, which would break a signature.
function Invoke-Tool([string]$exe, [string]$arguments, [string]$stdinFile) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $exe
    $psi.Arguments = $arguments
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.RedirectStandardInput = [bool]$stdinFile
    $p = [System.Diagnostics.Process]::Start($psi)
    $out = $p.StandardOutput.ReadToEndAsync()
    $err = $p.StandardError.ReadToEndAsync()
    if ($stdinFile) {
        $bytes = [IO.File]::ReadAllBytes($stdinFile)
        $p.StandardInput.BaseStream.Write($bytes, 0, $bytes.Length)
        $p.StandardInput.Close()
    }
    $p.WaitForExit()
    [pscustomobject]@{ Code = $p.ExitCode; Output = $out.Result + $err.Result }
}

function Find-Tool([string]$name, [string[]]$candidates) {
    foreach ($c in $candidates) { if ($c -and (Test-Path -LiteralPath $c)) { return $c } }
    $cmd = Get-Command $name -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($cmd) { return $cmd.Source }
    return $null
}

# Windows' own OpenSSH client (an optional feature, installed by default on current Windows).
# `ssh-keygen -Y verify` arrived in OpenSSH 8.1. Sysnative is System32 seen from 32-bit PowerShell.
function Find-SshKeygen {
    $keygen = Find-Tool 'ssh-keygen.exe' @("$env:WINDIR\System32\OpenSSH\ssh-keygen.exe", "$env:WINDIR\Sysnative\OpenSSH\ssh-keygen.exe")
    if (-not $keygen) { return $null }
    $ssh = Join-Path (Split-Path -Parent $keygen) 'ssh.exe'
    if (-not (Test-Path -LiteralPath $ssh)) { return $null }
    $v = Invoke-Tool $ssh '-V' $null
    if ($v.Output -match 'OpenSSH_(?:for_Windows_)?(\d+)\.(\d+)') {
        $major = [int]$Matches[1]; $minor = [int]$Matches[2]
        if ($major -gt 8 -or ($major -eq 8 -and $minor -ge 1)) { return $keygen }
    }
    return $null
}

# OpenSSL 3 or later, such as the one Git for Windows carries.
function Find-OpenSsl {
    $openssl = Find-Tool 'openssl.exe' @("$env:ProgramFiles\Git\usr\bin\openssl.exe")
    if (-not $openssl) { return $null }
    if ((Invoke-Tool $openssl 'version' $null).Output -match '^OpenSSL [3-9]\.') { return $openssl }
    return $null
}

function Assert-Signed([string]$tmp, [string]$sums) {
    $notSigned = "pai install: SHA256SUMS from $base isn't signed by the pai release key, so the download may have been changed; not installing"
    $keygen = Find-SshKeygen
    if ($keygen) {
        $sig = Join-Path $tmp 'SHA256SUMS.sshsig'
        Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS.$signingKeyId.sshsig" -OutFile $sig
        $allowed = Join-Path $tmp 'allowed_signers'
        [IO.File]::WriteAllText($allowed, "pai-release ssh-ed25519 $signingKeySsh`n")
        $r = Invoke-Tool $keygen "-Y verify -f `"$allowed`" -I pai-release -n pai-release -s `"$sig`"" $sums
        if ($r.Code -ne 0) { throw $notSigned }
        Write-Host 'Checked the signature on SHA256SUMS with ssh-keygen.'
        return
    }
    $openssl = Find-OpenSsl
    if ($openssl) {
        $sig = Join-Path $tmp 'SHA256SUMS.sig'
        Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS.$signingKeyId.sig" -OutFile $sig
        $pem = Join-Path $tmp 'release-key.pem'
        [IO.File]::WriteAllText($pem, "-----BEGIN PUBLIC KEY-----`n$signingKeySpki`n-----END PUBLIC KEY-----`n")
        $r = Invoke-Tool $openssl "pkeyutl -verify -pubin -inkey `"$pem`" -rawin -in `"$sums`" -sigfile `"$sig`"" $null
        if ($r.Code -ne 0) { throw $notSigned }
        Write-Host 'Checked the signature on SHA256SUMS with OpenSSL.'
        return
    }
    Write-Warning 'pai install: this computer has neither the Windows OpenSSH client 8.1 or later nor OpenSSL 3, so the signature on SHA256SUMS isn''t checked. Only the checksum, from the same server as the binary, protects this install.'
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("pai-install-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    Write-Host "Downloading $asset from $base"
    $exe = Join-Path $tmp 'pai.exe'
    $sums = Join-Path $tmp 'SHA256SUMS'
    Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $exe
    Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile $sums
    Assert-Signed $tmp $sums

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

if ($paiArgs.Count -gt 0) {
    Write-Host ''
    & (Join-Path $dir 'pai.exe') @paiArgs
    exit $LASTEXITCODE
}
