# net-combiner v0.1.7

This release tightens tray shutdown behavior and makes the updater UI reflect the actual version state more clearly.

## Changes

- Fixed tray Quit after the main window has been closed to tray.
- Kept graceful shutdown for the proxy and VPN sidecar before exiting.
- Disabled the update install action when the latest GitHub release matches the running app version.
- Changed the update button label to `Up to date` / `최신 상태` when no newer release is available.

## Packages

- Windows x64 setup installer
- macOS Intel package
- macOS Apple Silicon package
- Linux x64 deb package
- Portable archives for all release targets

## Notes

VPN mode still requires administrator or root privileges. On Windows, the installer includes `wintun.dll` and `tun2proxy-bin.exe` beside the application executable. macOS packages are currently unsigned.
