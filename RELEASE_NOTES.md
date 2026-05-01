# net-combiner v0.1.9

This release refines the run dashboard and footer behavior.

## Changes

- Added a top-of-run adapter traffic table showing total upload/download per adapter for the current run.
- Made adapter traffic totals independent of the visible connection list so pruning old rows does not affect the totals.
- Collapsed the footer brand and GitHub URL onto one line.
- Hid footer update controls when the running version is already current.

## Packages

- Windows x64 setup installer
- macOS Intel package
- macOS Apple Silicon package
- Linux x64 deb package
- Portable archives for all release targets

## Notes

VPN mode still requires administrator or root privileges. On Windows, the installer includes `wintun.dll` and `tun2proxy-bin.exe` beside the application executable. macOS packages are currently unsigned.
