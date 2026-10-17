#!/usr/bin/env python3
"""Stand-in for wss://iot-prod.anovaculinary.io, to see whether the oven
will talk to a server it has no reason to trust.

Point iot-prod.anovaculinary.io at this machine in the LAN's DNS, then run:

    tools/oven-cloud-probe.py --cert-dir <dir> --log <file>

It terminates TLS with a self-signed certificate (generated into --cert-dir on
first run), logs the TLS handshake, the WebSocket upgrade request (decoding the
JWT in its Authorization header) and every frame the oven sends. Like the real
cloud, it sends CMD_APO_HEALTHCHECK every 10s so the oven keeps the session
open; nothing else is ever sent to the oven.

To send the oven a command, write one JSON message per line to the control
port (localhost only), e.g.

    echo '{"id": "...", "type": "CMD_APO_STOP"}' | nc 127.0.0.1 8765

Each line goes to every connected oven as-is.

Standard library only.
"""

import argparse
import base64
import datetime
import hashlib
import json
import os
import socket
import ssl
import struct
import subprocess
import sys
import threading
import uuid

HOST = "iot-prod.anovaculinary.io"
WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

log_lock = threading.Lock()
log_file = None

# (sock, send_lock, peer) for every oven currently connected.
sessions = set()
sessions_lock = threading.Lock()


def log(peer, msg):
    line = f"{datetime.datetime.now().isoformat(timespec='milliseconds')} [{peer}] {msg}"
    with log_lock:
        print(line, flush=True)
        if log_file:
            log_file.write(line + "\n")
            log_file.flush()


def ensure_cert(cert_dir):
    cert = os.path.join(cert_dir, "cert.pem")
    key = os.path.join(cert_dir, "key.pem")
    if not (os.path.exists(cert) and os.path.exists(key)):
        os.makedirs(cert_dir, exist_ok=True)
        subprocess.run(
            ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
             "-keyout", key, "-out", cert, "-days", "3650",
             "-subj", f"/CN={HOST}", "-addext", f"subjectAltName=DNS:{HOST}"],
            check=True, capture_output=True)
    return cert, key


def make_context(cert, key):
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(cert, key)
    # Be as accommodating as possible to older TLS clients.
    ctx.minimum_version = ssl.TLSVersion.TLSv1
    ctx.set_ciphers("ALL:@SECLEVEL=0")

    def on_sni(_sslobj, server_name, _ctx):
        log("tls", f"SNI: {server_name!r}")

    ctx.sni_callback = on_sni
    return ctx


def decode_jwt(token):
    token = token.removeprefix("Bearer ").strip()
    parts = token.split(".")
    out = []
    for p in parts[:2]:
        try:
            out.append(json.loads(base64.urlsafe_b64decode(p + "=" * (-len(p) % 4))))
        except Exception as e:  # not a JWT after all; just report it
            out.append(f"<undecodable: {e}>")
    return out


def recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise EOFError
        buf += chunk
    return buf


def send_frame(sock, lock, opcode, payload):
    # Server-to-client frames are unmasked.
    header = bytes([0x80 | opcode])
    n = len(payload)
    if n < 126:
        header += bytes([n])
    elif n < 1 << 16:
        header += bytes([126]) + struct.pack(">H", n)
    else:
        header += bytes([127]) + struct.pack(">Q", n)
    with lock:
        sock.sendall(header + payload)


def healthchecks(sock, lock, peer, stop):
    while not stop.wait(10):
        msg = json.dumps({"id": str(uuid.uuid4()), "type": "CMD_APO_HEALTHCHECK"})
        try:
            send_frame(sock, lock, 0x1, msg.encode())
            log(peer, f"-> {msg}")
        except OSError:
            return


def handle(raw, addr, ctx):
    peer = f"{addr[0]}:{addr[1]}"
    log(peer, "TCP connect")
    try:
        sock = ctx.wrap_socket(raw, server_side=True)
    except (ssl.SSLError, OSError) as e:
        log(peer, f"TLS handshake FAILED: {e!r}")
        raw.close()
        return
    log(peer, f"TLS ok: {sock.version()} {sock.cipher()}")

    stop = threading.Event()
    lock = threading.Lock()
    try:
        req = b""
        while b"\r\n\r\n" not in req:
            chunk = sock.recv(4096)
            if not chunk:
                raise EOFError
            req += chunk
        head, _, rest = req.partition(b"\r\n\r\n")
        lines = head.decode(errors="replace").split("\r\n")
        log(peer, f"request: {lines[0]}")
        headers = {}
        for h in lines[1:]:
            k, _, v = h.partition(":")
            headers[k.strip().lower()] = v.strip()
            log(peer, f"  {h}")
        if "authorization" in headers:
            log(peer, f"  JWT decoded: {json.dumps(decode_jwt(headers['authorization']))}")

        key = headers.get("sec-websocket-key")
        if not key:
            log(peer, "not a WebSocket upgrade; closing")
            return
        accept = base64.b64encode(hashlib.sha1((key + WS_GUID).encode()).digest()).decode()
        resp = ("HTTP/1.1 101 Switching Protocols\r\n"
                "Upgrade: websocket\r\nConnection: Upgrade\r\n"
                f"Sec-WebSocket-Accept: {accept}\r\n")
        if "sec-websocket-protocol" in headers:
            resp += f"Sec-WebSocket-Protocol: {headers['sec-websocket-protocol'].split(',')[0].strip()}\r\n"
        sock.sendall((resp + "\r\n").encode())
        log(peer, "WebSocket upgraded")
        if rest:
            log(peer, f"(had {len(rest)} bytes after the request head; ignored)")

        threading.Thread(target=healthchecks, args=(sock, lock, peer, stop), daemon=True).start()
        session = (sock, lock, peer)
        with sessions_lock:
            sessions.add(session)

        message = None  # fragments of a text message still arriving
        while True:
            b0, b1 = recv_exact(sock, 2)
            fin, opcode = b0 >> 7, b0 & 0x0F
            n = b1 & 0x7F
            if n == 126:
                n = struct.unpack(">H", recv_exact(sock, 2))[0]
            elif n == 127:
                n = struct.unpack(">Q", recv_exact(sock, 8))[0]
            mask = recv_exact(sock, 4) if b1 & 0x80 else None
            data = recv_exact(sock, n)
            if mask:
                data = bytes(c ^ mask[i % 4] for i, c in enumerate(data))
            if opcode in (0x0, 0x1):
                message = data if opcode == 0x1 else (message or b"") + data
                if fin:
                    log(peer, f"<- {message.decode(errors='replace')}")
                    message = None
            elif opcode == 0x2:
                log(peer, f"<- binary (fin={fin}) {data.hex()}")
            elif opcode == 0x8:
                code = struct.unpack(">H", data[:2])[0] if len(data) >= 2 else None
                log(peer, f"<- close code={code} reason={data[2:].decode(errors='replace')!r}")
                send_frame(sock, lock, 0x8, data[:2])
                return
            elif opcode == 0x9:
                log(peer, f"<- ping {data.hex()}")
                send_frame(sock, lock, 0xA, data)
            elif opcode == 0xA:
                log(peer, f"<- pong {data.hex()}")
            else:
                log(peer, f"<- unknown opcode {opcode}: {data.hex()}")
    except EOFError:
        log(peer, "connection closed by oven")
    except (ssl.SSLError, OSError) as e:
        log(peer, f"connection error: {e!r}")
    finally:
        stop.set()
        with sessions_lock:
            sessions.discard((sock, lock, peer))
        sock.close()


def control(port):
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", port))
    srv.listen()
    log("probe", f"control port on 127.0.0.1:{port}")
    while True:
        conn, _ = srv.accept()
        with conn, conn.makefile("rw") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    json.loads(line)
                except ValueError as e:
                    f.write(f"not JSON: {e}\n")
                    f.flush()
                    continue
                with sessions_lock:
                    targets = list(sessions)
                for sock, lock, peer in targets:
                    try:
                        send_frame(sock, lock, 0x1, line.encode())
                        log(peer, f"-> (control) {line}")
                    except OSError as e:
                        log(peer, f"control send failed: {e!r}")
                f.write(f"sent to {len(targets)} oven(s)\n")
                f.flush()


def main():
    global log_file
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bind", default="0.0.0.0")
    ap.add_argument("--port", type=int, default=443)
    ap.add_argument("--cert-dir", required=True, help="where the self-signed cert lives (created if missing)")
    ap.add_argument("--log", help="also append the log to this file")
    ap.add_argument("--control-port", type=int, default=8765)
    args = ap.parse_args()

    if args.log:
        log_file = open(args.log, "a")
    cert, key = ensure_cert(args.cert_dir)
    ctx = make_context(cert, key)

    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind((args.bind, args.port))
    srv.listen()
    log("probe", f"listening on {args.bind}:{args.port} as {HOST}")
    threading.Thread(target=control, args=(args.control_port,), daemon=True).start()
    while True:
        raw, addr = srv.accept()
        threading.Thread(target=handle, args=(raw, addr, ctx), daemon=True).start()


if __name__ == "__main__":
    sys.exit(main())
