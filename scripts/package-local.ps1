param(
    [string]$Target = "",
    [switch]$SkipWintun
)

$ErrorActionPreference = "Stop"

$repo = Resolve-Path (Join-Path $PSScriptRoot "..")
Push-Location $repo
try {
    if ([string]::IsNullOrWhiteSpace($Target)) {
        cargo build --release --locked
        $releaseDir = Join-Path $repo "target\release"
    } else {
        cargo build --release --locked --target $Target
        $releaseDir = Join-Path $repo "target\$Target\release"
    }

    $sidecarCandidates = @(
        (Join-Path $repo "target\tun2proxy-sidecar\bin\tun2proxy-bin.exe"),
        (Join-Path $repo "target\tun2proxy-install-test\bin\tun2proxy-bin.exe"),
        (Join-Path $repo "target\release\tun2proxy-bin.exe")
    )
    $sidecar = $sidecarCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1

    if (-not $sidecar) {
        $installArgs = @("install", "tun2proxy", "--locked", "--root", "target\tun2proxy-sidecar")
        if (-not [string]::IsNullOrWhiteSpace($Target)) {
            $installArgs += @("--target", $Target)
        }
        cargo @installArgs
        $sidecar = Join-Path $repo "target\tun2proxy-sidecar\bin\tun2proxy-bin.exe"
    }

    if (-not (Test-Path $sidecar)) {
        throw "tun2proxy-bin.exe was not found. Run: cargo install tun2proxy --locked --root target\tun2proxy-sidecar"
    }

    Copy-Item -LiteralPath $sidecar -Destination (Join-Path $releaseDir "tun2proxy-bin.exe") -Force

    if (-not $SkipWintun) {
        $wintun = Join-Path $releaseDir "wintun.dll"
        if (-not (Test-Path $wintun)) {
            $zip = Join-Path $env:TEMP "wintun-0.14.1.zip"
            $extract = Join-Path $env:TEMP "wintun-0.14.1"
            Invoke-WebRequest https://www.wintun.net/builds/wintun-0.14.1.zip -OutFile $zip
            if (Test-Path $extract) {
                Remove-Item -LiteralPath $extract -Recurse -Force
            }
            Expand-Archive $zip -DestinationPath $extract
            Copy-Item -LiteralPath (Join-Path $extract "wintun\bin\amd64\wintun.dll") -Destination $wintun -Force
        }
    }

    Write-Host "Packaged net-combiner with tun2proxy in $releaseDir"
}
finally {
    Pop-Location
}
