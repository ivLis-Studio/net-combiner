# net-combiner v0.1.5

This release tightens the default VPN workflow and adds runtime safeguards for long-running desktop use.

## Changes

- Changed the default mode selection to VPN mode.
- Replaced the theme text toggle with a compact vector icon toggle.
- Reworked the run-page route graphic so it reflects the actual selected adapter count and selected source names.
- Added rotating log files capped at 2 MiB, keeping the last three rotated logs beside the active log.
- Added a Windows single-instance guard. Starting a second GUI instance now shows an alert and exits the new instance.
- Added a runtime adapter watchdog. If a selected adapter disappears or changes while proxy/VPN is running, net-combiner stops the route and shows an alert.

## Packages

- Windows x64 setup installer
- macOS Intel package
- macOS Apple Silicon package
- Linux x64 deb package
- Portable archives for all release targets

## Notes

VPN mode still requires administrator or root privileges. On Windows, the installer includes `wintun.dll` and `tun2proxy-bin.exe` beside the application executable. macOS packages are currently unsigned.
