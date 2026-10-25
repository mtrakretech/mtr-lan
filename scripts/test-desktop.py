"""Windows integration checks using only Python's standard library; test config is isolated."""
import argparse
import ctypes
from ctypes import wintypes
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import tempfile
import time
import tomllib

user32 = ctypes.WinDLL("user32", use_last_error=True)
CALLBACK = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
user32.EnumWindows.argtypes = [CALLBACK, wintypes.LPARAM]
user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
user32.GetWindowTextW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
user32.GetClassNameW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
user32.IsWindowVisible.argtypes = [wintypes.HWND]
user32.PostMessageW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]


def windows(pid):
    found = []
    @CALLBACK
    def visit(hwnd, _):
        owner = wintypes.DWORD()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        if owner.value == pid:
            title, cls = ctypes.create_unicode_buffer(256), ctypes.create_unicode_buffer(256)
            user32.GetWindowTextW(hwnd, title, 256)
            user32.GetClassNameW(hwnd, cls, 256)
            found.append((hwnd, title.value, cls.value, bool(user32.IsWindowVisible(hwnd))))
        return True
    user32.EnumWindows(visit, 0)
    return found


def wait_for(predicate, description, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.1)
    raise AssertionError("Timed out: " + description)


def read_exact(stream, size):
    result = b""
    while len(result) < size:
        part = stream.recv(size - len(result))
        if not part:
            raise EOFError("service closed IPC stream")
        result += part
    return result


def send(stream, event):
    data = json.dumps(event).encode()
    stream.sendall(struct.pack(">Q", len(data)) + data)


def receive(stream, name):
    deadline = time.monotonic() + 10
    while True:
        if time.monotonic() > deadline:
            raise TimeoutError("Waiting for service notification: " + name)
        size, = struct.unpack(">Q", read_exact(stream, 8))
        assert size <= 1024 * 1024
        event = json.loads(read_exact(stream, size))
        if isinstance(event, dict) and name in event:
            return event[name]


def connect():
    try:
        return socket.create_connection(("127.0.0.1", 5252), timeout=3)
    except OSError:
        return None


def status(stream):
    send(stream, {"Status": []})
    return receive(stream, "Status")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("package", type=Path)
    args = parser.parse_args()
    exe = args.package.resolve() / "MTR-LAN-Copy.exe"
    pe = exe.read_bytes()
    offset, = struct.unpack_from("<I", pe, 0x3C)
    subsystem, = struct.unpack_from("<H", pe, offset + 24 + 68)
    assert subsystem == 2, "EXE must use Windows GUI subsystem, not console subsystem"
    print("PASS: PE Windows GUI subsystem (no automatic console)")
    prior = connect()
    if prior:
        prior.close()
        raise RuntimeError("Close existing MTR-LAN-Copy before isolated integration tests")
    temporary_root = Path(os.environ["LOCALAPPDATA"]) / "Temp" / "opencode"
    with tempfile.TemporaryDirectory(prefix="Lan Mouse Desktop Test ", dir=temporary_root) as tmp:
        config = Path(tmp) / "config.toml"
        config.write_text("port = 4242\nfrontend = 'gtk'\n")
        # Desktop preferences share the selected config directory, never the user's real config.
        preferences = Path(tmp) / "desktop-settings.toml"
        preferences.write_text("close_to_tray = true\nstart_minimized = false\n")
        process = subprocess.Popen([str(exe), "--config", str(config)], cwd=tmp)
        stream = None
        try:
            stream = wait_for(connect, "service ready")
            main_window = wait_for(lambda: next((w for w in windows(process.pid) if w[1] == "MTR-LAN-Copy" and w[3]), None), "main window")
            tray = wait_for(lambda: next((w for w in windows(process.pid) if w[2] == "tray_icon_app"), None), "native tray icon window")
            assert not any(w[2] == "ConsoleWindowClass" for w in windows(process.pid))
            initial = status(stream)
            assert initial["port"] == 4242 and initial["configured"] == 0
            print("PASS: visible GUI, native tray HWND, running service and live status")
            for name in ["127.0.0.1", "127.0.0.2"]:
                send(stream, {"AddClient": [name, 4242, "Right"]})
                receive(stream, "NotifyClientCreate")
            wait_for(lambda: len(tomllib.loads(config.read_text()).get("clients", [])) == 2, "peer autosave")
            assert status(stream)["configured"] == 2
            print("PASS: peer autosave, including multiple peers on the same side")
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as reserve:
                reserve.bind(("127.0.0.1", 0))
                port = reserve.getsockname()[1]
            send(stream, {"ChangePort": port})
            receive(stream, "NotifyPortChange")
            assert status(stream)["port"] == port
            assert tomllib.loads(config.read_text())["port"] == port
            print("PASS: port rebind and persistence")
            before = status(stream)["packets_received"]
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
                probe.sendto(b"\x04", ("127.0.0.1", port))  # Protocol Ping; never injects input.
            wait_for(lambda: status(stream)["packets_received"] > before, "live packet counter")
            print("PASS: live datagram counter updates on a local control packet")
            send(stream, {"ChangePort": 0})
            receive(stream, "NotifyError")
            assert status(stream)["port"] == port
            malformed = connect()
            malformed.sendall(struct.pack(">Q", 65537))
            malformed.close()
            assert status(stream)["port"] == port
            print("PASS: invalid port rejected; oversized IPC frame does not crash service")
            user32.PostMessageW(main_window[0], 0x10, 0, 0)  # WM_CLOSE
            wait_for(lambda: not any(w[1] == "MTR-LAN-Copy" and w[3] for w in windows(process.pid)), "close-to-tray")
            assert process.poll() is None
            user32.PostMessageW(tray[0], 6002, 0, 0x203)  # tray-icon Windows double-click notification
            wait_for(lambda: any(w[1] == "MTR-LAN-Copy" and w[3] for w in windows(process.pid)), "tray double-click restore")
            print("PASS: close hides to tray; tray double-click restores")
            user32.PostMessageW(main_window[0], 0x10, 0, 0)
            wait_for(lambda: not any(w[1] == "MTR-LAN-Copy" and w[3] for w in windows(process.pid)), "hide before second launch")
            second = subprocess.Popen([str(exe), "--config", str(config)])
            assert second.wait(timeout=10) == 0
            wait_for(lambda: any(w[1] == "MTR-LAN-Copy" and w[3] for w in windows(process.pid)), "second launch restores first")
            print("PASS: duplicate launch restores existing GUI and exits")
            send(stream, {"Shutdown": []})
            wait_for(lambda: connect() is None, "daemon shutdown")
            # The GUI intentionally stays alive to explain service failure, rather than disappearing.
            time.sleep(0.4)
            assert process.poll() is None
            print("PASS: GUI survives service disconnect for error/status reporting")
        finally:
            if stream:
                if process.poll() is None:
                    try:
                        send(stream, {"Shutdown": []})
                        time.sleep(0.2)
                    except OSError:
                        pass
                stream.close()
            if process.poll() is None:
                process.terminate()
            process.wait(timeout=10)
        process = subprocess.Popen([str(exe), "--config", str(config), "--minimized"], cwd=tmp)
        stream = None
        try:
            stream = wait_for(connect, "minimized service startup")
            tray = wait_for(lambda: next((w for w in windows(process.pid) if w[2] == "tray_icon_app"), None), "minimized tray")
            assert not any(w[1] == "MTR-LAN-Copy" and w[3] for w in windows(process.pid))
            user32.PostMessageW(tray[0], 6002, 0, 0x203)
            wait_for(lambda: any(w[1] == "MTR-LAN-Copy" and w[3] for w in windows(process.pid)), "minimized launch restores from tray")
            print("PASS: --minimized starts tray-only and can be restored")
            process.terminate()
            process.wait(timeout=10)
            wait_for(lambda: connect() is None, "forced parent exit cleans up daemon")
            print("PASS: Windows job object removes daemon after forced GUI termination")
        finally:
            if stream:
                stream.close()
            if process.poll() is None:
                process.terminate()
            process.wait(timeout=10)
        preferences.write_text("close_to_tray = false\nstart_minimized = false\n")
        process = subprocess.Popen([str(exe), "--config", str(config)], cwd=tmp)
        stream = None
        try:
            stream = wait_for(connect, "service restart")
            restored = status(stream)
            assert restored["port"] == port and restored["configured"] == 2
            main_window = wait_for(lambda: next((w for w in windows(process.pid) if w[1] == "MTR-LAN-Copy" and w[3]), None), "restarted GUI")
            user32.PostMessageW(main_window[0], 0x10, 0, 0)
            assert process.wait(timeout=10) == 0
            wait_for(lambda: connect() is None, "child daemon cleanup")
            print("PASS: settings survive restart; close-to-exit stops both GUI and daemon")
        finally:
            if stream:
                stream.close()
            if process.poll() is None:
                process.terminate()
            process.wait(timeout=10)
    print("All desktop integration checks passed; Windows startup was NOT enabled or modified.")


if __name__ == "__main__":
    main()
