param([string]$Iscc = "ISCC.exe")

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "../..")

if ($env:RUSTTALK_RELEASE_PUBLIC_KEY -cnotmatch '^[a-f0-9]{64}$') {
    throw "Set RUSTTALK_RELEASE_PUBLIC_KEY to the 32-byte Ed25519 public key in hex."
}
$compiler = Get-Command $Iscc -ErrorAction SilentlyContinue
if (!$compiler) {
    throw "Install Inno Setup 6 for the current user and pass the full path to ISCC.exe with -Iscc."
}
$env:RUSTTALK_LOCAL_UPDATE_TEST = $null
$metadata = cargo metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw "Cargo metadata failed." }
$version = ($metadata.packages | Where-Object name -eq "rusttalk-client").version
if ($version -cnotmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' -or
    ($version.Split('.') | Where-Object { [decimal]$_ -gt 65535 })) {
    throw "Release version must be MAJOR.MINOR.PATCH, with components at most 65535."
}
cargo build --release --locked -p rusttalk-client --target x86_64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) { throw "RustTalk build failed." }
$client = Join-Path $metadata.target_directory "x86_64-pc-windows-msvc/release/rusttalk-client.exe"
$output = Join-Path (Get-Location) "dist"
& $compiler.Source "/DVersion=$version" "/DClient=$client" "/O$output" (Join-Path $PSScriptRoot "rusttalk.iss")
if ($LASTEXITCODE -ne 0) { throw "Inno Setup compilation failed." }
