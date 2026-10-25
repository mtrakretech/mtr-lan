"""Control-plane tests only. Never sends keyboard/mouse events or enables a host with active peers."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import tomllib

spec = importlib.util.spec_from_file_location("desktop_test", Path(__file__).with_name("test-desktop.py"))
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("package", type=Path)
    args = parser.parse_args()
    exe = args.package.resolve() / "MTR-LAN-Copy.exe"
    if helper.connect():
        raise RuntimeError("Close existing MTR-LAN-Copy instances first")
    root = Path(os.environ["LOCALAPPDATA"]) / "Temp" / "opencode"
    with tempfile.TemporaryDirectory(prefix="Lan Mouse Network Check ", dir=root) as tmp:
        config = Path(tmp) / "config.toml"
        config.write_text("port = 4242\nfrontend = 'gtk'\n")
        process = subprocess.Popen([str(exe), "--config", str(config)], cwd=tmp)
        stream = None
        responder = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        responder.bind(("127.0.0.2", 0))
        responder.settimeout(0.2)
        stop = threading.Event()
        try:
            stream = helper.wait_for(helper.connect, "service startup")
            first = helper.status(stream)
            assert first["computer_name"] == os.environ["COMPUTERNAME"]
            assert first["role"] == "client" and first["configured"] == 0
            assert first["capture_available"], "Native Windows hook initialization failed"
            print("PASS: actual PC name, local addresses, native host capability and safe Client default")
            helper.send(stream, {"SetIdentity": ["Test desktop host", "host"]})
            helper.wait_for(lambda: helper.status(stream)["role"] == "host", "host role apply")
            assert not helper.status(stream)["sharing"]
            saved = tomllib.loads(config.read_text())
            assert saved["role"] == "host" and saved["device_name"] == "Test desktop host"
            # Disable host capture before any responding peer is configured.
            helper.send(stream, {"SetIdentity": ["Test desktop client", "client"]})
            helper.wait_for(lambda: helper.status(stream)["role"] == "client", "client role apply")
            print("PASS: PC rename and live Host/Client switching persist; no active capture targets used")
            fake_port = responder.getsockname()[1]
            announcement = json.dumps({"magic": "lanmouse-desktop-v3", "name": "Test receiver", "computer_name": "PEER-TEST", "port": fake_port, "role": "client"}).encode()
            responder.sendto(announcement, ("127.0.0.1", 4243))
            helper.wait_for(lambda: any(d["address"] == "127.0.0.2" for d in helper.status(stream)["discovered"]), "LAN announcement")
            assert helper.status(stream)["configured"] == 0
            print("PASS: discovery updates live without auto-adding or trusting a device")
            helper.send(stream, {"AddClient": ["127.0.0.2", fake_port, "Right"]})
            created = helper.receive(stream, "NotifyClientCreate")
            handle = created["handle"]
            assert created["display_name"] == "Test receiver"
            assert not helper.status(stream)["peers"][0]["enabled"]
            helper.send(stream, {"RenamePeer": [handle, "Lab computer"]})
            helper.receive(stream, "NotifyClientUpdate")
            assert tomllib.loads(config.read_text())["clients"][0]["display_name"] == "Lab computer"
            print("PASS: discovered device name is shown; peer nickname persists")

            def answer_pings():
                while not stop.is_set():
                    try:
                        data, address = responder.recvfrom(2048)
                        if data == b"\x04":
                            responder.sendto(b"\x05", address)
                    except socket.timeout:
                        pass
                    except OSError:
                        return
            worker = threading.Thread(target=answer_pings, daemon=True)
            worker.start()
            helper.send(stream, {"ActivateClient": [handle, True]})
            helper.receive(stream, "NotifyClientActivate")
            helper.wait_for(lambda: any(p["handle"] == handle and p["responding"] and p["latency_ms"] is not None for p in helper.status(stream)["peers"]), "heartbeat response and RTT")
            print("PASS: enabled versus responding and RTT update from protocol Ping/Pong")
            stop.set()
            worker.join(timeout=2)
            helper.wait_for(lambda: not next(p for p in helper.status(stream)["peers"] if p["handle"] == handle)["responding"], "offline timeout", timeout=8)
            print("PASS: per-device status becomes unresponsive after heartbeat loss")
            helper.send(stream, {"ActivateClient": [handle, False]})
            helper.receive(stream, "NotifyClientActivate")
            helper.send(stream, {"AddClient": [os.environ["COMPUTERNAME"], 4242, "Left"]})
            named = helper.receive(stream, "NotifyClientCreate")
            helper.wait_for(lambda: any(p["handle"] == named["handle"] and p["addresses"] for p in helper.status(stream)["peers"]), "Windows hostname resolution", timeout=10)
            print("PASS: actual Windows PC name resolves through native name services")
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as malformed:
                for packet in [b"", b"\xff", b"\x00\x00"]:
                    malformed.sendto(packet, ("127.0.0.1", 4242))
            time.sleep(0.3)
            assert helper.status(stream)["role"] == "client"
            helper.send(stream, {"ReleaseInput": []})
            assert not helper.status(stream)["sharing"]
            assert (Path(tmp) / "logs" / "service.log").exists()
            print("PASS: malformed UDP does not kill service; release control and local logs available")
            assert os.environ["COMPUTERNAME"] == first["computer_name"]
            print("All network/control-plane checks passed. No physical input or remote-device behavior was tested.")
        finally:
            stop.set()
            responder.close()
            if stream:
                try:
                    helper.send(stream, {"Shutdown": []})
                    helper.wait_for(lambda: helper.connect() is None, "service stop", timeout=5)
                except (OSError, AssertionError):
                    pass
                stream.close()
            if process.poll() is None:
                process.terminate()
            process.wait(timeout=10)


if __name__ == "__main__":
    main()
