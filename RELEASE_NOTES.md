# net-combiner v0.1.3

This release fixes tray menu handling after the main window is closed to the tray.

## Changes

- Fixed Windows tray menu actions after closing the main window with `X`.
- Tray menu commands such as Show, Options, Connection activity, and Quit now continue to work while the main window is hidden.
- Adjusted close-to-tray event ordering so a restored window is not immediately hidden again by a stale close request.

## Packages

- Windows x64 setup installer
- macOS Intel package
- macOS Apple Silicon package
- Linux x64 deb package
- Portable archives for all release targets

## Notes

VPN mode still requires administrator or root privileges. On Windows, the installer includes `wintun.dll` and `tun2proxy-bin.exe` beside the application executable. macOS packages are currently unsigned.
