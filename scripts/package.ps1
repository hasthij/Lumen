# Builds the release binaries, then the self-contained installer, and a portable zip.
# Output goes to dist\. Run from the repository root:  powershell -File scripts\package.ps1
$ErrorActionPreference = "Stop"
cargo build --release -p lumen -p lumen-tap
cargo build --release -p lumen-setup
New-Item -ItemType Directory -Force dist | Out-Null
Copy-Item target\release\lumen-setup.exe dist\LumenSetup.exe -Force
Compress-Archive -Path target\release\lumen.exe, target\release\lumen_tap.dll -DestinationPath dist\Lumen-portable.zip -Force
Get-FileHash dist\LumenSetup.exe, dist\Lumen-portable.zip | Format-Table Hash, Path -AutoSize
