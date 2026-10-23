//! Wake-on-LAN: waking a sleeping machine on the local network by
//! broadcasting a "magic packet" with its network card's MAC address.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};

/// A MAC address as typed: six bytes in hex, separated by `:` or `-`,
/// grouped by dots (`aabb.ccdd.eeff`) or not at all. Fails with a one-line
/// reason.
pub fn parse_mac(text: &str) -> Result<[u8; 6], String> {
    let digits: String = text
        .trim()
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | '.'))
        .collect();
    let wrong = || format!("{} isn't a MAC address like aa:bb:cc:dd:ee:ff", text.trim());
    if digits.len() != 12 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(wrong());
    }
    let mut mac = [0u8; 6];
    for (at, byte) in mac.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digits[at * 2..at * 2 + 2], 16).map_err(|_| wrong())?;
    }
    Ok(mac)
}

/// `aa:bb:cc:dd:ee:ff`, as the vault keeps it.
pub fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Six `0xff` bytes, then the address sixteen times.
pub fn magic_packet(mac: &[u8; 6]) -> [u8; 102] {
    let mut packet = [0xffu8; 102];
    for copy in 0..16 {
        packet[6 + copy * 6..12 + copy * 6].copy_from_slice(mac);
    }
    packet
}

/// Sends the magic packet for `mac` to `to`.
pub fn send_to(mac: &[u8; 6], to: SocketAddr) -> std::io::Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    socket.send_to(&magic_packet(mac), to)?;
    Ok(())
}

/// Wakes the machine with `mac`: the magic packet, broadcast on the local
/// network to the ports cards listen on (9, then 7). Errors are a few
/// lowercase words.
pub fn wake(mac: &[u8; 6]) -> Result<(), String> {
    for port in [9, 7] {
        send_to(mac, SocketAddr::from((Ipv4Addr::BROADCAST, port)))
            .map_err(|error| crate::ssh::describe(&error))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_mac_addresses_however_written() {
        let mac = [0xaa, 0xbb, 0xcc, 0x01, 0x02, 0x03];
        for text in [
            "aa:bb:cc:01:02:03",
            "AA-BB-CC-01-02-03",
            "aabb.cc01.0203",
            " aabbcc010203 ",
        ] {
            assert_eq!(parse_mac(text), Ok(mac), "{text}");
        }
        assert_eq!(format_mac(&mac), "aa:bb:cc:01:02:03");
        assert!(parse_mac("aa:bb:cc").is_err());
        assert!(
            parse_mac("gg:bb:cc:01:02:03")
                .unwrap_err()
                .contains("MAC address")
        );
    }

    #[test]
    fn sends_the_magic_packet() {
        let mac = [1, 2, 3, 4, 5, 6];
        let packet = magic_packet(&mac);
        assert_eq!(packet[..6], [0xff; 6]);
        assert!(packet[6..].chunks(6).all(|copy| copy == mac));

        let listener = UdpSocket::bind("127.0.0.1:0").unwrap();
        send_to(&mac, listener.local_addr().unwrap()).unwrap();
        let mut got = [0u8; 200];
        let read = listener.recv(&mut got).unwrap();
        assert_eq!(&got[..read], &packet[..]);
    }
}
