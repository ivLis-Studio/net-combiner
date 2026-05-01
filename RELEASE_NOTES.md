# net-combiner v0.1.2

This release focuses on the new wizard UI and cleaner VPN startup behavior.

## Changes

- Reworked the main GUI into a compact step-by-step flow based on the new design reference.
- Fixed the adapter/settings headers so long labels do not collapse into vertical text.
- Removed animated page effects from the intro and run views to reduce UI stutter.
- `tun2proxy-bin.exe` is now spawned without a visible console window on Windows.

## Packages

- Windows x64 setup installer
- macOS Intel package
- macOS Apple Silicon package
- Linux x64 deb package
- Portable archives for all release targets

## Notes

VPN mode still requires administrator or root privileges. On Windows, the installer includes `wintun.dll` and `tun2proxy-bin.exe` beside the application executable. macOS packages are currently unsigned.
