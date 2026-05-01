# net-combiner v0.1.1

This release is a polish update for the first public build.

## Changes

- Windows builds now use the GUI subsystem, so launching the desktop app no longer opens a console window.
- The Windows executable now embeds the net-combiner icon resource. Installer-created Start Menu and Desktop shortcuts use that icon explicitly.
- The connection monitor now opens on active connections by default and includes filters for active, recent, and all rows.
- README files no longer mention earlier implementation references.

## Packages

- Windows x64 setup installer
- macOS Intel package
- macOS Apple Silicon package
- Linux x64 deb package
- Portable archives for all release targets

## Notes

VPN mode requires administrator or root privileges. On Windows, the installer includes `wintun.dll` and `tun2proxy-bin.exe` beside the application executable. macOS packages are currently unsigned.
