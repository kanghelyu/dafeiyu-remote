#!/usr/bin/env python3
# SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
# SPDX-FileCopyrightText: 2026 Dafeiyu Remote contributors
# Copyright (c) 2026 Dafeiyu Remote contributors
"""dfy-relay 端到端验证（仅 Python 标准库，适配 3.13）。

验证内容：
  1. 两个 UDP 伪端点（role0/role1）用同一 tag + HMAC 完成 bind，收到 BindOk；
  2. 双向互发若干 1KB 包，校验包体逐字节一致；
  3. 发送侧灌包粗测吞吐（pps / Mbps，基于接收窗口统计）；
  4. 结束时发 Bye 让中继立即回收会话。

用法：
  python scripts/e2e.py --relay 127.0.0.1:21117 --key "base64:...." --count 10
"""

import argparse
import base64
import hashlib
import hmac
import os
import socket
import struct
import sys
import threading
import time

MAGIC = b"DFYR"
VERSION = 1
HEADER = 6
TAG_LEN = 16
NONCE_LEN = 16
AUTH_LEN = 32

ROLE_INIT = 0
ROLE_RESP = 1

KIND_BIND = 1
KIND_BIND_OK = 2
KIND_BIND_ERR = 3
KIND_BYE = 4


def parse_key(text):
    """接受 base64:... / hex:... / 裸 64 hex，与 Rust 端 RelayKey::parse 一致。"""
    text = text.strip()
    if text.startswith("base64:"):
        return base64.b64decode(text[7:])
    if text.startswith("hex:"):
        return bytes.fromhex(text[4:])
    if len(text) == 64:
        return bytes.fromhex(text)
    raise SystemExit(f"无法解析 relay_key（需 base64:/hex:/64 位 hex）：{text[:16]}…")


def compute_auth(key, tag, role, nonce, ts_ms):
    msg = tag + bytes([role]) + nonce + struct.pack(">Q", ts_ms)
    return hmac.new(key, msg, hashlib.sha256).digest()


def bind_packet(key, tag, role, nonce=None, ts_ms=None):
    nonce = nonce or os.urandom(NONCE_LEN)
    ts_ms = ts_ms if ts_ms is not None else int(time.time() * 1000)
    auth = compute_auth(key, tag, role, nonce, ts_ms)
    return (
        MAGIC
        + bytes([VERSION, KIND_BIND])
        + tag
        + bytes([role])
        + nonce
        + struct.pack(">Q", ts_ms)
        + auth
    )


def kind_of(pkt):
    if len(pkt) >= HEADER and pkt[:4] == MAGIC and pkt[4] == VERSION:
        return pkt[5]
    return None


class Peer:
    """一个 UDP 伪端点：绑定本机随机端口，向中继发 bind，之后靠中继成对转发。"""

    def __init__(self, relay, key, tag, role):
        self.relay = relay
        self.key = key
        self.tag = tag
        self.role = role
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        # 显式绑定，保证 Windows 上 getsockname 在首包前也有效。
        self.sock.bind(("0.0.0.0", 0))
        try:
            self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 * 1024 * 1024)
        except OSError:
            pass
        self.sock.settimeout(2.0)

    def local(self):
        return self.sock.getsockname()

    def bind(self, timeout=2.0):
        self.sock.settimeout(timeout)
        self.sock.sendto(
            bind_packet(self.key, self.tag, self.role), self.relay
        )
        data, _peer = self.sock.recvfrom(2048)
        kind = kind_of(data)
        if kind == KIND_BIND_ERR:
            raise SystemExit(f"bind 被拒绝（code={data[22]}），tag={self.tag.hex()[:8]}")
        if kind != KIND_BIND_OK:
            raise SystemExit(f"期望 BindOk，收到 kind={kind} len={len(data)}")
        if data[HEADER : HEADER + TAG_LEN] != self.tag:
            raise SystemExit("BindOk 的 tag 与请求不匹配")
        expires = struct.unpack(">I", data[HEADER + TAG_LEN : HEADER + TAG_LEN + 4])[0]
        return expires

    def send(self, payload):
        self.sock.sendto(payload, self.relay)

    def recv(self, timeout=None):
        if timeout is not None:
            self.sock.settimeout(timeout)
        data, _peer = self.sock.recvfrom(65535)
        return data

    def bye(self):
        try:
            self.sock.sendto(MAGIC + bytes([VERSION, KIND_BYE]) + self.tag, self.relay)
        except OSError:
            pass


def parse_addr(text):
    host, _, port = text.rpartition(":")
    if not host or not port.isdigit():
        raise SystemExit(f"--relay 地址非法（示例 127.0.0.1:21117）：{text}")
    return host, int(port)


def main():
    ap = argparse.ArgumentParser(description="dfy-relay 端到端验证（标准库实现）")
    ap.add_argument("--relay", default="127.0.0.1:21117", help="中继 UDP 数据面地址")
    ap.add_argument("--key", required=True, help="relay_key（base64:... / hex:... / 64 hex）")
    ap.add_argument("--count", type=int, default=10, help="双向各发多少包做一致性校验")
    ap.add_argument("--size", type=int, default=1024, help="每包字节数")
    ap.add_argument(
        "--throughput-packets", type=int, default=5000, help="吞吐测试发送包数"
    )
    ap.add_argument(
        "--pace-us",
        type=int,
        default=500,
        help="每 16 包插入的发送间隔（微秒），0 表示全速突发",
    )
    ap.add_argument("--no-throughput", action="store_true", help="跳过吞吐测试")
    args = ap.parse_args()

    relay = parse_addr(args.relay)
    key = parse_key(args.key)
    if len(key) != 32:
        raise SystemExit(f"relay_key 必须是 32 字节，实际 {len(key)}")
    tag = os.urandom(TAG_LEN)
    print(f"relay={relay[0]}:{relay[1]} tag={tag.hex()[:8]}…")

    a = Peer(relay, key, tag, ROLE_INIT)
    b = Peer(relay, key, tag, ROLE_RESP)
    print(f"  端点A(role0)={a.local()[0]}:{a.local()[1]}  端点B(role1)={b.local()[0]}:{b.local()[1]}")

    expires = a.bind()
    b.bind()
    print(f"[1] 双端 bind 成功，BindOk expires={expires}s")

    payload = bytes((i % 251) for i in range(args.size))
    t0 = time.perf_counter()
    for i in range(args.count):
        a.send(payload)
        got = b.recv(2.0)
        if got != payload:
            raise SystemExit(f"A→B 第 {i} 包不一致：{len(got)} 字节")
        b.send(payload)
        got = a.recv(2.0)
        if got != payload:
            raise SystemExit(f"B→A 第 {i} 包不一致：{len(got)} 字节")
    t1 = time.perf_counter()
    rtt_ms = (t1 - t0) * 1000.0 / (args.count * 2)
    print(
        f"[2] 双向 {args.count}×{args.size}B 全部一致（平均单程 {rtt_ms:.3f} ms）"
    )

    if not args.no_throughput:
        n = args.throughput_packets
        blob = bytes((i % 251) for i in range(args.size))
        recv_pkts = [0]
        recv_bytes = [0]
        t_window = {}
        done = threading.Event()

        def rx():
            b.sock.settimeout(0.3)
            first = last = None
            while True:
                try:
                    data, _ = b.sock.recvfrom(65535)
                except socket.timeout:
                    break
                now = time.perf_counter()
                if first is None:
                    first = now
                last = now
                recv_pkts[0] += 1
                recv_bytes[0] += len(data)
            t_window["first"] = first
            t_window["last"] = last
            done.set()

        th = threading.Thread(target=rx, daemon=True)
        th.start()
        s0 = time.perf_counter()
        for i in range(n):
            a.send(blob)
            # 发送侧留出极小间隔：避免 Python 接收线程先成为瓶颈，
            # 这样得到的 pps 是"无丢失可持续转发率"，也是更可信的下限。
            if args.pace_us > 0 and (i + 1) % 16 == 0:
                time.sleep(args.pace_us / 1_000_000.0)
        send_elapsed = time.perf_counter() - s0
        done.wait(5.0)
        if t_window.get("first") and t_window.get("last"):
            span = max(t_window["last"] - t_window["first"], 1e-9)
            pps = recv_pkts[0] / span
            mbps = recv_bytes[0] * 8 / span / 1e6
        else:
            pps = mbps = 0.0
        loss = 100.0 * (n - recv_pkts[0]) / n
        print(
            f"[3] 吞吐：发送 {n} 包（{send_elapsed*1000:.1f}ms，{n/send_elapsed:.0f} pps 发送侧）"
        )
        print(
            f"    中继转发接收 {recv_pkts[0]} 包 / {recv_bytes[0]} B → {pps:.0f} pps，"
            f"{mbps:.1f} Mbps（丢包 {loss:.1f}%）"
        )
        if loss > 5.0:
            print(
                "    注：有丢包时 pps 只反映本 Python 接收端的消费上限（GIL/单线程 recvfrom），"
                "不是中继上限；可增大 --pace-us 降低发送速率再测。"
            )
        if recv_pkts[0] == 0:
            raise SystemExit("吞吐测试未收到任何转发包")

    a.bye()
    b.bye()
    print("[4] Bye 已发送，会话回收")
    print("E2E OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())