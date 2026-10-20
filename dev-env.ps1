# Dot-source from any directory: . 'E:/Code Base/pdfcraft/dev-env.ps1'
# All Rust installations, downloaded dependencies and build output stay in this project.
param([switch]$Proxy)
$env:CARGO_HOME = Join-Path $PSScriptRoot '.tools/cargo'
$env:RUSTUP_HOME = Join-Path $PSScriptRoot '.tools/rustup'
$env:CARGO_TARGET_DIR = Join-Path $PSScriptRoot 'target'
$cargoBin = Join-Path $env:CARGO_HOME 'bin'
if (($env:Path -split ';') -notcontains $cargoBin) { $env:Path = $cargoBin + ';' + $env:Path }
if ($Proxy) {
    # Cargo's proxy affects Cargo downloads only, not unrelated applications or system settings.
    $env:CARGO_HTTP_PROXY = 'http://127.0.0.1:10819'
    $env:CARGO_NET_GIT_FETCH_WITH_CLI = 'true'
    $env:GIT_CONFIG_COUNT = '1'
    $env:GIT_CONFIG_KEY_0 = 'http.https://github.com.proxy'
    $env:GIT_CONFIG_VALUE_0 = 'http://127.0.0.1:10819'
}
