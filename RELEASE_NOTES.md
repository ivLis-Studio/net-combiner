# net-combiner v0.1.4

This release makes update management visible from the main window footer.

## Changes

- Added a persistent footer with `ivLis-Studio` and the GitHub repository URL.
- Added a main-window auto-update panel with current version, status, update check, and install controls.
- Connected the footer controls to the existing GitHub Releases updater so users can install the latest portable release from inside the app.
- Localized update status messages for English and Korean.

## Packages

- Windows x64 setup installer
- macOS Intel package
- macOS Apple Silicon package
- Linux x64 deb package
- Portable archives for all release targets

## Notes

VPN mode still requires administrator or root privileges. On Windows, the installer includes `wintun.dll` and `tun2proxy-bin.exe` beside the application executable. macOS packages are currently unsigned.
