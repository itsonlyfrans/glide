//! Wake-on-LAN: wake a paired computer that has gone to sleep, when the person reaches for it.
//!
//! While two computers are connected, Glide notes the other one's network card address (its MAC) from this
//! computer's own address table; nothing extra is sent between them. When that computer is asleep and the cursor is
//! pushed toward the place it has on the desk, Glide broadcasts the standard Wake-on-LAN "magic packet" on the local
//! network. The sleeping computer must allow it (macOS: "Wake for network access"; Windows: the network adapter's
//! Wake on Magic Packet option), and laptops on battery often ignore it.

use glide_platform::Point;
use glide_proto::ipc::{LayoutDevice, Peer};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};

/// The first real hardware address in some text: `a4-83-e7-0a-1b-2c` (Windows) or `a4:83:e7:a:1b:2c` (macOS, which
/// drops leading zeros). Returned as `aa:bb:cc:dd:ee:ff`; broadcast and all-zero addresses are ignored.
pub fn parse_mac(text: &str) -> Option<String> {
    text.split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .find_map(|token| {
            let parts: Vec<&str> = token.split(['-', ':']).collect();
            if parts.len() != 6
                || parts.iter().any(|p| {
                    p.is_empty() || p.len() > 2 || !p.chars().all(|c| c.is_ascii_hexdigit())
                })
            {
                return None;
            }
            let bytes: Vec<u8> = parts
                .iter()
                .filter_map(|p| u8::from_str_radix(p, 16).ok())
                .collect();
            if bytes.iter().all(|b| *b == 0xff) || bytes.iter().all(|b| *b == 0) {
                return None;
            }
            Some(
                bytes
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(":"),
            )
        })
}

/// Reads the hardware address for `ip` from this computer's own address table (no network traffic).
pub fn lookup_mac(ip: Ipv4Addr) -> Option<String> {
    #[cfg(windows)]
    let output = {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("arp")
            .args(["-a", &ip.to_string()])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW: never flash a console window
            .output()
            .ok()?
    };
    #[cfg(not(windows))]
    let output = std::process::Command::new("/usr/sbin/arp")
        .args(["-n", &ip.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    // Only trust the line that names this exact address.
    text.lines()
        .filter(|line| {
            line.split(|c: char| c.is_whitespace() || c == '(' || c == ')')
                .any(|t| t == ip.to_string())
        })
        .find_map(parse_mac)
}

/// The Wake-on-LAN magic packet: six 0xFF bytes followed by the address sixteen times.
pub fn magic_packet(mac: &str) -> Option<[u8; 102]> {
    let bytes: Vec<u8> = mac
        .split(':')
        .filter_map(|p| u8::from_str_radix(p, 16).ok())
        .collect();
    if bytes.len() != 6 {
        return None;
    }
    let mut packet = [0xffu8; 102];
    for chunk in packet[6..].chunks_mut(6) {
        chunk.copy_from_slice(&bytes);
    }
    Some(packet)
}

/// Broadcasts the magic packet on the local network (and to the last known subnet), on the usual ports 9 and 7.
pub fn send(mac: &str, last_ip: Option<Ipv4Addr>) -> std::io::Result<()> {
    let packet = magic_packet(mac).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid hardware address")
    })?;
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    let mut targets = vec![Ipv4Addr::BROADCAST];
    if let Some(ip) = last_ip {
        let [a, b, c, _] = ip.octets();
        targets.push(Ipv4Addr::new(a, b, c, 255));
    }
    let mut sent = false;
    for target in targets {
        for port in [9, 7] {
            sent |= socket
                .send_to(&packet, SocketAddr::from((target, port)))
                .is_ok();
        }
    }
    if sent {
        Ok(())
    } else {
        Err(std::io::Error::other(
            "the wake-up packet could not be sent",
        ))
    }
}

/// The last IPv4 address a peer was reached at, from its `address` ("192.168.1.30:24800").
pub fn peer_ip(peer: &Peer) -> Option<Ipv4Addr> {
    let address = peer.address.as_deref()?;
    address
        .parse::<SocketAddr>()
        .ok()
        .and_then(|a| match a.ip() {
            std::net::IpAddr::V4(v4) => Some(v4),
            std::net::IpAddr::V6(_) => None,
        })
}

/// An asleep, wakeable peer whose place on the desk contains `p` (desk coordinates). Uses its last known screens, or
/// the same 1920x1080 stand-in the desk uses for a computer whose screens are not known.
pub fn sleeping_peer_at<'a>(
    p: Point,
    layout: &[LayoutDevice],
    peers: &'a [Peer],
) -> Option<&'a Peer> {
    peers
        .iter()
        .filter(|peer| !peer.online && peer.wake_mac.is_some())
        .find(|peer| {
            let Some(origin) = layout.iter().find(|d| d.device_id == peer.device_id) else {
                return false;
            };
            let screens: Vec<(f64, f64, f64, f64)> = if peer.last_monitors.is_empty() {
                vec![(0.0, 0.0, 1920.0, 1080.0)]
            } else {
                let min_x = peer
                    .last_monitors
                    .iter()
                    .map(|m| m.x)
                    .fold(f64::INFINITY, f64::min);
                let min_y = peer
                    .last_monitors
                    .iter()
                    .map(|m| m.y)
                    .fold(f64::INFINITY, f64::min);
                peer.last_monitors
                    .iter()
                    .map(|m| (m.x - min_x, m.y - min_y, m.w, m.h))
                    .collect()
            };
            screens.iter().any(|(x, y, w, h)| {
                let left = origin.x + x;
                let top = origin.y + y;
                p.x >= left && p.x < left + w && p.y >= top && p.y < top + h
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use glide_platform::{Monitor, Os};
    use glide_proto::ipc::Connection;

    // Windows' arp prints dashes, macOS drops leading zeros: both must give the same address, and the broadcast
    // entry Windows lists for every network must never be picked.
    #[test]
    fn hardware_addresses_are_read_from_both_systems_address_tables() {
        let windows = "\nInterface: 192.168.1.5 --- 0x7\n  Internet Address      Physical Address      Type\n  192.168.1.30          a4-83-e7-0a-1b-2c     dynamic\n";
        assert_eq!(parse_mac(windows).as_deref(), Some("a4:83:e7:0a:1b:2c"));
        let mac = "? (192.168.1.30) at a4:83:e7:a:1b:2c on en0 ifscope [ethernet]";
        assert_eq!(parse_mac(mac).as_deref(), Some("a4:83:e7:0a:1b:2c"));
        assert_eq!(
            parse_mac("192.168.1.255         ff-ff-ff-ff-ff-ff     static"),
            None
        );
        assert_eq!(parse_mac("? (192.168.1.30) at (incomplete) on en0"), None);
    }

    // A wrong packet silently wakes nothing; pin the exact format.
    #[test]
    fn magic_packet_is_six_ff_bytes_then_the_address_sixteen_times() {
        let packet = magic_packet("a4:83:e7:0a:1b:2c").expect("packet");
        assert_eq!(&packet[..6], &[0xff; 6]);
        for repeat in packet[6..].chunks(6) {
            assert_eq!(repeat, &[0xa4, 0x83, 0xe7, 0x0a, 0x1b, 0x2c]);
        }
        assert!(magic_packet("not an address").is_none());
    }

    fn peer(id: &str, online: bool, wake: bool, monitors: Vec<Monitor>) -> Peer {
        Peer {
            device_id: id.into(),
            name: id.into(),
            os: Os::Macos,
            fingerprint: id.into(),
            online,
            connection: if online {
                Connection::Connected
            } else {
                Connection::Offline
            },
            address: Some("192.168.1.30:24800".into()),
            latency_ms: None,
            monitors: Vec::new(),
            clipboard_enabled: true,
            wake_mac: wake.then(|| "a4:83:e7:0a:1b:2c".into()),
            last_monitors: monitors,
            app_version: None,
            model: None,
        }
    }

    // Feature: pushing the cursor toward the sleeping MacBook's place on the desk wakes it, and only it.
    #[test]
    fn the_sleeping_computer_where_the_cursor_is_pushed_is_found() {
        let layout = vec![
            LayoutDevice {
                device_id: "pc".into(),
                x: 0.0,
                y: 0.0,
            },
            LayoutDevice {
                device_id: "mac".into(),
                x: 5120.0,
                y: 300.0,
            },
        ];
        let screen = Monitor {
            id: "1".into(),
            x: 0.0,
            y: 0.0,
            w: 1728.0,
            h: 1117.0,
            scale: 2.0,
            primary: true,
        };
        let peers = vec![peer("mac", false, true, vec![screen])];
        let beside = Point {
            x: 5130.0,
            y: 700.0,
        };
        assert_eq!(
            sleeping_peer_at(beside, &layout, &peers).map(|p| p.device_id.as_str()),
            Some("mac")
        );
        assert!(
            sleeping_peer_at(
                Point {
                    x: 5130.0,
                    y: 1500.0
                },
                &layout,
                &peers
            )
            .is_none(),
            "below its screen"
        );
        // Awake, or no known address: nothing to wake.
        assert!(
            sleeping_peer_at(beside, &layout, &[peer("mac", true, true, Vec::new())]).is_none()
        );
        assert!(
            sleeping_peer_at(beside, &layout, &[peer("mac", false, false, Vec::new())]).is_none()
        );
        assert_eq!(peer_ip(&peers[0]), Some(Ipv4Addr::new(192, 168, 1, 30)));
    }
}
