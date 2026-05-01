# net-combiner v0.1.8

This release improves live traffic visibility and adds selectable load-balancing behavior for large downloads.

## Changes

- Connection monitor rows now receive live upload/download byte updates instead of waiting for connection close.
- TCP relay accounting now preserves transferred bytes even when a relay exits with a socket error.
- UDP relay flows now report upload/download byte counts as well.
- Adapter cards show per-adapter upload/download totals for the current run.
- Added a user-selectable adapter strategy: sticky per destination IP or per-connection load balancing.
- Reworked adapter selection to prefer the least-loaded weighted adapter by active connections and active traffic, reducing large-download imbalance when servers split traffic across multiple IPs.

## Packages

- Windows x64 setup installer
- macOS Intel package
- macOS Apple Silicon package
- Linux x64 deb package
- Portable archives for all release targets

## Notes

VPN mode still requires administrator or root privileges. On Windows, the installer includes `wintun.dll` and `tun2proxy-bin.exe` beside the application executable. macOS packages are currently unsigned.
