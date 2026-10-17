# Provision a Windows golden image for building native code: Git, the MSVC
# build tools and Windows SDK for this machine's architecture, LLVM, and Rust
# through rustup.
#
#   [images.windows.build]
#   kind      = "unattended-install"
#   disk_size = "80G"
#   provision = "~/path/to/windows-devtools.ps1"
#
# Every installer is fetched directly rather than through winget, which a
# freshly installed image may not have registered yet. Whatever is already
# installed is left alone, so the script can also be run on a live VM with
# `vitro exec --script`. Expect it to add half an hour and around 15 GB.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$arm = $env:PROCESSOR_ARCHITECTURE -eq 'ARM64'
$downloads = Join-Path $env:TEMP 'vitro-provision'
New-Item -ItemType Directory -Force -Path $downloads | Out-Null

function Get-Installer([string]$Url, [string]$Name) {
    $path = Join-Path $downloads $Name
    # Write-Host, not Write-Output: anything output becomes part of what the
    # function returns, and the caller would get an array instead of a path.
    Write-Host "downloading $Url"
    Invoke-WebRequest -Uri $Url -OutFile $path -UseBasicParsing
    $path
}

# The newest release asset whose name matches, from a GitHub repository.
function Get-ReleaseAsset([string]$Repo, [string]$Pattern) {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" `
        -Headers @{ 'User-Agent' = 'vitro-provision' }
    $asset = $release.assets | Where-Object { $_.name -match $Pattern } | Select-Object -First 1
    if (-not $asset) { throw "no asset of $Repo's latest release matches $Pattern" }
    Get-Installer $asset.browser_download_url $asset.name
}

function Invoke-Installer([string]$Path, [string[]]$Arguments) {
    $process = Start-Process -FilePath $Path -ArgumentList $Arguments -Wait -PassThru
    # 3010: installed, and wants a restart, which the build's shutdown gives it.
    if ($process.ExitCode -notin 0, 3010) {
        throw "$(Split-Path -Leaf $Path) exited with $($process.ExitCode)"
    }
}

function Add-MachinePath([string]$Dir) {
    $path = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    if (($path -split ';') -notcontains $Dir) {
        [Environment]::SetEnvironmentVariable('Path', "$path;$Dir", 'Machine')
    }
}

# Git. Its installer puts Git\cmd on the machine PATH itself.
if (-not (Test-Path "$env:ProgramFiles\Git\cmd\git.exe")) {
    $pattern = if ($arm) { '^Git-.*-arm64\.exe$' } else { '^Git-.*-64-bit\.exe$' }
    $git = Get-ReleaseAsset 'git-for-windows/git' $pattern
    Invoke-Installer $git '/VERYSILENT', '/NORESTART', '/NOCANCEL', '/SP-', '/SUPPRESSMSGBOXES'
}

# The MSVC build tools for this architecture, and a Windows SDK. Rust's MSVC
# target links with them, and so does anything built with cl or clang-cl.
$tools = if ($arm) { 'Microsoft.VisualStudio.Component.VC.Tools.ARM64' } else { 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64' }
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$present = (Test-Path $vswhere) -and (& $vswhere -products * -requires $tools -property installationPath)
if (-not $present) {
    $vs = Get-Installer 'https://aka.ms/vs/17/release/vs_buildtools.exe' 'vs_buildtools.exe'
    Invoke-Installer $vs '--quiet', '--wait', '--norestart', '--nocache',
        '--add', 'Microsoft.VisualStudio.Workload.VCTools',
        '--add', $tools,
        '--add', 'Microsoft.VisualStudio.Component.Windows11SDK.26100',
        '--includeRecommended'
}

# LLVM. Its silent install does not touch PATH, so bin is added here.
if (-not (Test-Path "$env:ProgramFiles\LLVM\bin\clang.exe")) {
    $pattern = if ($arm) { '^LLVM-.*-woa64\.exe$' } else { '^LLVM-.*-win64\.exe$' }
    $llvm = Get-ReleaseAsset 'llvm/llvm-project' $pattern
    Invoke-Installer $llvm '/S'
}
Add-MachinePath "$env:ProgramFiles\LLVM\bin"

# Rust, for the guest account: rustup puts ~\.cargo\bin on the user PATH.
if (-not (Test-Path "$env:USERPROFILE\.cargo\bin\rustup.exe")) {
    $triple = if ($arm) { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
    $rustup = Get-Installer "https://static.rust-lang.org/rustup/dist/$triple/rustup-init.exe" 'rustup-init.exe'
    Invoke-Installer $rustup '-y', '--profile', 'minimal', '--default-toolchain', 'stable'
}

Remove-Item -Recurse -Force $downloads -ErrorAction SilentlyContinue

# Report from the PATH the next session will have, not this one's.
$env:Path = [Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' +
    [Environment]::GetEnvironmentVariable('Path', 'User')
git --version
clang --version | Select-Object -First 1
cargo --version
