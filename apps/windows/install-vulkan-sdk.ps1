# Installs the LunarG Vulkan SDK on a Windows CI runner so stage-local-asr.sh
# can build ggml's Vulkan backend (ggml-vulkan.dll): CMake finds the headers
# and vulkan-1.lib through VULKAN_SDK, and the shaders compile with its glslc.
# The installer is LunarG's own, pinned by version and SHA-256 (from
# https://vulkan.lunarg.com/sdk/sha/<version>/windows/<file>.json); a
# mismatch stops the build before anything runs.
$ErrorActionPreference = 'Stop'
$version = '1.4.313.2'
$sha256 = '34a921d951858274ca8e470e9d0a3b7624db41216b3908ccea9f73c8a1b7500e'
$file = "vulkansdk-windows-X64-$version.exe"
$installer = Join-Path $env:RUNNER_TEMP $file
$root = "C:\VulkanSDK\$version"

Invoke-WebRequest -Uri "https://sdk.lunarg.com/sdk/download/$version/windows/$file" -OutFile $installer
$actual = (Get-FileHash -Algorithm SHA256 $installer).Hash.ToLowerInvariant()
if ($actual -ne $sha256) { throw "$file is $actual, expected $sha256" }
& $installer --root $root --accept-licenses --default-answer --confirm-command install
if ($LASTEXITCODE -ne 0) { throw "the Vulkan SDK installer exited $LASTEXITCODE" }
if (-not (Test-Path "$root\Bin\glslc.exe")) { throw "the Vulkan SDK has no glslc in $root\Bin" }

Add-Content $env:GITHUB_ENV "VULKAN_SDK=$root"
Add-Content $env:GITHUB_PATH "$root\Bin"
& "$root\Bin\glslc.exe" --version
