# MTR-LAN-Copy

**MTR-LAN-Copy is a Windows desktop app for sharing one keyboard and mouse
between trusted PCs on a local network.** It is developed and maintained by
[`mtrakretech`](https://github.com/mtrakretech) and distributed as a portable
application.

> **Security:** Network input and discovery traffic are currently unencrypted
> and unauthenticated. Use only on a trusted private LAN. Never expose the app's
> ports to the public internet. Windows Host capture is experimental and should
> be enabled only when you understand that keyboard and mouse input will be
> forwarded to enabled peers.

## Download

Get the latest Windows x64 portable package from
[MTR-LAN-Copy Releases](https://github.com/mtrakretech/mtr-lan/releases).
Extract the **entire ZIP** and run `MTR-LAN-Copy.exe`. No installer or
administrator privileges are required. Keep the accompanying DLLs and data
folders next to the executable.

The current release is [desktop v7](https://github.com/mtrakretech/mtr-lan/releases/tag/v7).
See [`packaging/PORTABLE-README.txt`](packaging/PORTABLE-README.txt) and
[`packaging/TWO-PC-SETUP.txt`](packaging/TWO-PC-SETUP.txt) for setup instructions.

## What it does

- Provides a graphical Windows app with a notification-area (tray) icon.
- Offers explicit **Host** and **Client** roles. Client mode is the safe default;
  local capture is disabled until Host mode is deliberately selected.
- Finds compatible running MTR-LAN-Copy peers on the local network and also
  supports adding peers by Windows computer name or IP address.
- Lets you configure peer position and enable the connection on each PC.
- Saves display names and peer settings, and shows live connection health and
  local diagnostic logs.
- Supports returning the pointer to the sending PC at the receiving PC's
  facing edge, plus an emergency keyboard release chord.
- Can be run portably from an extracted folder; startup with Windows is
  optional and off by default.

The app's **display name** is the name shown inside MTR-LAN-Copy. It does not
rename the Windows computer itself.

## Quick setup (two Windows PCs)

1. Download and extract the same v7 portable ZIP on both PCs; launch
   `MTR-LAN-Copy.exe` on each.
2. On the PC whose physical keyboard and mouse will be shared, open **This PC**,
   select **Host**, choose a display name, and save. Read and confirm the Host
   mode warning. Leave the other PC in **Client** mode.
3. In **Connections**, add each PC on the other and enable the peer on both
   sides. Discovery does not automatically trust or enable devices.
4. Set each peer's screen position to match the physical arrangement. Wait for
   both PCs to report the peer is responding.
5. Move the pointer to the configured outer screen edge to begin sharing.
   Hold it against the facing edge on the receiving PC to return control. The
   emergency release is **Ctrl + Alt + Shift + F12** on the sending PC.

Allow the app through Windows Firewall on **Private networks** if prompted.
The app uses UDP ports 4242/4243 for peer traffic and loopback TCP 5252 for its
local GUI/service connection by default. Do not configure port forwarding to
the internet.

## Important limitations

- Physical two-PC keyboard/mouse operation has not been verified as part of
  this release's automated tests; test carefully before relying on it.
- Host input capture is experimental. Secure desktops, sign-in screens,
  elevated applications, and some games may reject injected input.
- Peer traffic is plaintext and has no cryptographic pairing or authentication.
- Discovery advertises metadata and is not proof that a device is trusted.
- Clipboard/file transfer and unattended remote installation are not provided.
- Same-LAN discovery can be blocked by guest Wi-Fi, VLANs, router settings, or
  client isolation. Add a peer manually by IP or computer name if needed.

## Build from source (Windows)

This repository includes scripts for the local MSYS2 UCRT64 toolchain used to
build and package the Windows app. From PowerShell at the repository root:

```powershell
./scripts/build-windows.ps1 -Test
./scripts/package-windows.ps1
```

The first command builds the Windows release and runs the Rust unit and
documentation tests. The second creates portable and source archives under
`dist/`. Python integration checks can be run with:

```powershell
python ./scripts/test-network.py ./dist/MTR-LAN-Copy-0.7.3-windows-x64-portable-v7
python ./scripts/test-desktop.py ./dist/MTR-LAN-Copy-0.7.3-windows-x64-portable-v7
./scripts/test-portable.ps1
```

These checks exercise the control plane, application lifecycle, and packaged
startup; they do **not** replace a real two-PC input test.

## License and acknowledgements

MTR-LAN-Copy is distributed under the
[GNU General Public License v3.0 or later](LICENSE). This project includes and
modifies code from the GPL-licensed
[Lan Mouse project](https://github.com/esteganobvio/lan-mouse); its license and
applicable copyright notices are retained. The MTR-LAN-Copy Windows interface,
portable packaging, and project-specific additions are maintained in this
repository.
