# net-combiner v0.1.6

This release fixes unstable large transfers when multiple selected adapters are active.

## Changes

- Changed egress selection from per-connection round-robin to sticky routing per destination IP.
- Kept parallel range/download connections to the same server on the same source adapter, which avoids server-side and local TCP resets caused by changing source IPs mid-download.
- Preserved adapter failover: if the sticky adapter cannot connect, net-combiner tries the remaining selected adapters.
- Applied the same sticky destination policy to UDP relay socket selection.
- Added a startup log line showing the active egress policy.

## Packages

- Windows x64 setup installer
- macOS Intel package
- macOS Apple Silicon package
- Linux x64 deb package
- Portable archives for all release targets

## Notes

VPN mode still requires administrator or root privileges. On Windows, the installer includes `wintun.dll` and `tun2proxy-bin.exe` beside the application executable. macOS packages are currently unsigned.
