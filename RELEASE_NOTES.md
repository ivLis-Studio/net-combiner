# net-combiner v0.1.10

This release focuses on runtime stability in tray, proxy, VPN, and updater
flows.

## Changes

- Fixed Windows tray actions after the main window is closed to tray. Tray
  actions now wake and restore the hidden main window before dispatching app
  commands, so Quit works from the tray-only state.
- Added bounded outbound TCP connect attempts. A bad or blackholed adapter no
  longer stalls a new connection until the OS-level TCP timeout.
- Return a SOCKS5 failure reply when a CONNECT request cannot be routed, instead
  of closing the client connection abruptly.
- Cancel UDP response reader tasks when a UDP ASSOCIATE session closes, avoiding
  leaked sockets and stale connection monitor rows.
- Block update installation while proxy or VPN mode is running, preventing
  partial updates when sidecar files are still in use.
- Added focused unit tests for adapter scheduling and SOCKS UDP packet parsing.

## Packages

- Windows x64 setup installer
- macOS Intel package
- macOS Apple Silicon package
- Linux x64 deb package
- Portable archives for all release targets

## Notes

VPN mode still requires administrator or root privileges. On Windows, the
installer includes `wintun.dll` and `tun2proxy-bin.exe` beside the application
executable. macOS packages are currently unsigned.
