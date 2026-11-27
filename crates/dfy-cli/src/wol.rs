// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Dafeiyu Remote contributors

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};

const MAGIC_PACKET_LEN: usize = 102;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacAddr([u8; 6]);

impl MacAddr {
    pub fn parse(text: &str) -> Result<Self> {
        let compact: String = text
            .chars()
            .filter(|ch| !matches!(ch, ':' | '-' | '.'))
            .collect();
        if compact.len() != 12 || !compact.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("MAC 地址必须包含 6 字节十六进制，例如 AA:BB:CC:DD:EE:FF");
        }
        let mut bytes = [0u8; 6];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let start = index * 2;
            *byte =
                u8::from_str_radix(&compact[start..start + 2], 16).context("解析 MAC 地址失败")?;
        }
        if bytes.iter().all(|byte| *byte == 0) {
            bail!("不能使用全零 MAC 地址发送 Wake-on-LAN");
        }
        Ok(Self(bytes))
    }

    pub fn magic_packet(self) -> [u8; MAGIC_PACKET_LEN] {
        let mut packet = [0u8; MAGIC_PACKET_LEN];
        packet[..6].fill(0xff);
        for chunk in packet[6..].chunks_exact_mut(6) {
            chunk.copy_from_slice(&self.0);
        }
        packet
    }
}

impl std::fmt::Display for MacAddr {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [a, b, c, d, e, f] = self.0;
        write!(formatter, "{a:02X}:{b:02X}:{c:02X}:{d:02X}:{e:02X}:{f:02X}")
    }
}

pub fn send(mac: MacAddr, broadcast: IpAddr, port: u16, repeats: u8) -> Result<()> {
    if !(1..=10).contains(&repeats) {
        bail!("WOL 重复次数必须在 1..=10");
    }
    let IpAddr::V4(broadcast) = broadcast else {
        bail!("Wake-on-LAN 当前只支持 IPv4 广播地址");
    };
    if !broadcast.is_broadcast() && broadcast.octets()[3] != 255 {
        bail!("Wake-on-LAN 目标必须是 IPv4 广播地址（例如 255.255.255.255 或 192.168.1.255）");
    }
    let socket =
        UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).context("创建 Wake-on-LAN UDP socket 失败")?;
    socket
        .set_broadcast(true)
        .context("启用 UDP 广播失败，系统可能禁止广播")?;
    let destination = SocketAddr::new(IpAddr::V4(broadcast), port);
    let packet = mac.magic_packet();
    for _ in 0..repeats {
        socket
            .send_to(&packet, destination)
            .context("发送 Wake-on-LAN 数据包失败")?;
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_mac_formats() {
        let expected = MacAddr([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
        for text in [
            "00:11:22:33:44:55",
            "00-11-22-33-44-55",
            "0011.2233.4455",
            "001122334455",
        ] {
            assert_eq!(MacAddr::parse(text).unwrap(), expected, "{text}");
        }
    }

    #[test]
    fn rejects_malformed_and_broadcast_mac_addresses() {
        for text in [
            "",
            "00:11:22",
            "001122334455GG",
            "00:00:00:00:00:00",
            "ff:ff:ff:ff:ff:ff:ff",
        ] {
            assert!(MacAddr::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn builds_standard_magic_packet() {
        let packet = MacAddr([0x01, 0x23, 0x45, 0x67, 0x89, 0xab]).magic_packet();
        assert_eq!(packet.len(), 102);
        assert_eq!(&packet[..6], &[0xff; 6]);
        for chunk in packet[6..].chunks_exact(6) {
            assert_eq!(chunk, &[0x01, 0x23, 0x45, 0x67, 0x89, 0xab]);
        }
    }
}
