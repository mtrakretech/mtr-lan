MTR-LAN-COPY 0.7.3 - WINDOWS DESKTOP V7 (LOCAL EXPERIMENTAL HOST BACKEND)

Extract the whole ZIP and open MTR-LAN-Copy.exe directly. No CMD, installer or
administrator privileges required. Keep the bundled DLLs and data folders.

TWO-PC SETUP
1. Run desktop v7 on BOTH Windows PCs. Default role is Client (capture disabled).
2. On the PC providing keyboard/mouse: This PC > Host > Save name and role,
   then confirm the trusted-LAN warning. On the other PC choose Client.
3. Discover lists compatible running apps. Add the intended PC, or manually
   enter its Windows hostname/FQDN or IPv4 address in Connections.
4. Enable each other on BOTH PCs. Discovered devices are never auto-enabled.
5. On the host set the client's screen position (the side the client monitor
   is physically on relative to it), then Save changes. Once the peer shows
   Responding, move the pointer to that outer desktop edge.

COMING BACK AND RELEASING (BOTH DIRECTIONS SUPPORTED; FIXED IN V6)
- On the receiving PC: push the pointer against the screen edge that faces
  the sending PC and HOLD it there for about half a second. The sending PC
  regains the pointer and keyboard immediately. The gesture stays armed for
  a few seconds after shared input pauses. This works only while remote
  input is being shared, so ordinary local mouse use never triggers it.
- On the sending PC at any time: Ctrl + Alt + Shift + F12 releases. The
  original Ctrl + Alt + Shift + Windows chord also works. After releasing,
  the pointer parks just inside the screen edge and re-entry is paused for
  a moment, so the cursor no longer sticks to the screen edges.

HOST CAPTURE IS EXPERIMENTAL
- Native Windows low-level hooks forward physical mouse/keyboard input only
  in explicit Host mode, to enabled responding peers. Injected local input is
  not forwarded. Pointer parks centrally during sharing and is restored on
  release. Only outer virtual-desktop edges are used on multi-monitor systems.
- Queue overload, heartbeat loss or the return gesture releases local input.
  The daemon is owned by a kill-on-close Windows Job Object. No input-event
  text is logged.
- Local unit/control-plane tests are NOT a physical two-computer input test.
- Secure desktops, Windows sign-in screens, elevated/admin windows and some
  games may not accept injected input. SendInput failures are logged without
  busy-looping.
- Clipboard/file transfer, encrypted pairing and unattended remote
  installation are NOT implemented. This is not a complete commercial KVM.

NAMES, DISCOVERY AND ROUTERS
- This PC shows the Windows computer name and local private IPv4 addresses.
  Display-name changes affect this app only; Windows itself is not renamed.
- Per-computer nicknames are editable under Display name > Save changes.
- Manual PC-name resolution uses Windows native name services, then DNS.
  A friendly app nickname is not necessarily a resolvable DNS hostname.
- Compatible desktop apps keep announcing via UDP 4243 / multicast
  239.255.42.42, TTL 1 (v7 is wire-compatible with desktop v3/v4/v5/v6). Only
  compatible running apps are discoverable, not every Windows PC.
- Switches normally pass same-LAN discovery. Routers/VLANs/guest Wi-Fi/client
  isolation may block it. Use manual hostname/IP and working UDP routing.
  Configured peers also receive unicast announcements; no subnet scans occur.
- Discovery is unauthenticated metadata, NOT pairing or proof of trust.

LIVE STATUS AND DIAGNOSTICS
- Enabled, Responding, Role mismatch and Sharing are distinct states.
  Heartbeats refresh peer reachability/RTT; stale responses time out.
- Overview, This PC, Discover and Connections update while the app runs.
- Live logs shows the last 64 KiB of local service/app logs, refreshed each
  second. Files are logs/service.log and logs/app.log beside the chosen
  config. Keyboard text is never recorded.

DESKTOP AND PORTABLE SETTINGS
- 18 px default text; Settings > Appearance supports 16/18/20 px live.
- Close hides to tray by default. Tray double-click restores; Quit exits fully.
- Windows startup is opt-in under Settings > Windows integration. It creates
  only HKCU Run/MTRLANCopyPortable and starts minimized. Disable before moving
  folders, then re-enable at the new location. No Windows service is installed.
- portable.flag selects adjacent config.toml, regardless of working folder.
  --config overrides it. Peer/port/name/role changes save atomically.
- AUTOSAVE (V7): every peer edit (PC name/IP, UDP port, screen position,
  nickname, enable switch) saves itself within about a second of typing;
  collapsing a row saves too, and popup confirmations show what was saved.
  The Windows PC name saves itself when typing pauses. The Host role keeps
  an explicit, confirmed save because enabling input capture is
  security-relevant.
- desktop-settings.toml stores tray/font preferences. Actual registry state
  controls startup; a copied settings file never enables it automatically.

SECURITY: TRUSTED PRIVATE LANS ONLY
Input and discovery traffic are unencrypted and unauthenticated. IP allowlists
are not cryptographic authentication. Never forward these ports to the Internet
or use public/untrusted networks. Enabling a host can expose keystrokes on LAN.

Source: https://github.com/mtrakretech/mtr-lan (fork name: MTR-LAN-Copy)
SOURCE-COMMIT.txt identifies the source revision used for this build. Local
changes and build/test scripts are included in the accompanying source-v7 ZIP.
GPL: LICENSE.txt.
Native dependency licenses: share/licenses. Build tools live separately in .tools.
