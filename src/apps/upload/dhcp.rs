//! Minimal single-client DHCP server for upload-mode SoftAP.

use embassy_net::udp::UdpSocket;
use embassy_net::{IpEndpoint, Ipv4Address};

pub const SERVER_IP: [u8; 4] = [192, 168, 4, 1];
pub const CLIENT_IP: [u8; 4] = [192, 168, 4, 2];

const SERVER_PORT: u16 = 67;
const CLIENT_PORT: u16 = 68;
const BOOTP_FIXED_LEN: usize = 236;
const OPTIONS_START: usize = 240;
const MAGIC_COOKIE: [u8; 4] = [99, 130, 83, 99];
const RESPONSE_LEN: usize = 300;
const LEASE_SECONDS: u32 = 3600;

const DHCP_DISCOVER: u8 = 1;
const DHCP_OFFER: u8 = 2;
const DHCP_REQUEST: u8 = 3;
const DHCP_ACK: u8 = 5;
const DHCP_NAK: u8 = 6;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Reply {
    Offer,
    Ack,
    Nak,
}

#[derive(Clone, Copy)]
struct Request {
    xid: [u8; 4],
    flags: [u8; 2],
    ciaddr: [u8; 4],
    chaddr: [u8; 16],
    message_type: u8,
    requested_ip: Option<[u8; 4]>,
    server_id: Option<[u8; 4]>,
}

/// Bind the DHCP server socket to the standard server port.
pub fn bind(socket: &mut UdpSocket<'_>) -> bool {
    socket.bind(SERVER_PORT).is_ok()
}

/// What handling one datagram told us about the client.
///
/// The ACK is the first moment the client can actually reach the
/// server, which is when the screen stops advertising the join
/// credential and starts advertising the address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Served {
    /// Nothing the caller needs to act on.
    Nothing,
    /// A client took the lease.
    Bound,
}

/// Handle one DHCP datagram. Invalid and irrelevant packets are ignored.
pub async fn handle_one(socket: &mut UdpSocket<'_>) -> Served {
    let mut request_buf = [0u8; 576];
    let Ok((len, _)) = socket.recv_from(&mut request_buf).await else {
        return Served::Nothing;
    };
    let Some(request) = parse_request(&request_buf[..len]) else {
        return Served::Nothing;
    };
    let Some(reply) = classify(&request) else {
        return Served::Nothing;
    };

    let mut response = [0u8; RESPONSE_LEN];
    build_response(&request, reply, &mut response);
    let destination = IpEndpoint::new(Ipv4Address::new(255, 255, 255, 255).into(), CLIENT_PORT);
    if socket.send_to(&response, destination).await.is_err() {
        return Served::Nothing;
    }

    match reply {
        Reply::Ack => Served::Bound,
        Reply::Offer | Reply::Nak => Served::Nothing,
    }
}

fn parse_request(packet: &[u8]) -> Option<Request> {
    if packet.len() < OPTIONS_START
        || packet[0] != 1
        || packet[1] != 1
        || packet[2] < 6
        || packet[BOOTP_FIXED_LEN..OPTIONS_START] != MAGIC_COOKIE
    {
        return None;
    }

    let mut request = Request {
        xid: packet[4..8].try_into().ok()?,
        flags: packet[10..12].try_into().ok()?,
        ciaddr: packet[12..16].try_into().ok()?,
        chaddr: packet[28..44].try_into().ok()?,
        message_type: 0,
        requested_ip: None,
        server_id: None,
    };

    let mut pos = OPTIONS_START;
    while pos < packet.len() {
        let code = packet[pos];
        pos += 1;
        match code {
            0 => continue,
            255 => break,
            _ => {
                let len = *packet.get(pos)? as usize;
                pos += 1;
                let value = packet.get(pos..pos.checked_add(len)?)?;
                match (code, value) {
                    (53, [kind]) => request.message_type = *kind,
                    (50, [a, b, c, d]) => request.requested_ip = Some([*a, *b, *c, *d]),
                    (54, [a, b, c, d]) => request.server_id = Some([*a, *b, *c, *d]),
                    _ => {}
                }
                pos += len;
            }
        }
    }

    (request.message_type != 0).then_some(request)
}

fn classify(request: &Request) -> Option<Reply> {
    match request.message_type {
        DHCP_DISCOVER => Some(Reply::Offer),
        DHCP_REQUEST => {
            // A client selecting another DHCP server is not ours to NAK.
            if request.server_id.is_some_and(|id| id != SERVER_IP) {
                return None;
            }
            let requested = request.requested_ip.unwrap_or(request.ciaddr);
            Some(if requested == CLIENT_IP {
                Reply::Ack
            } else {
                Reply::Nak
            })
        }
        _ => None,
    }
}

fn build_response(request: &Request, reply: Reply, out: &mut [u8; RESPONSE_LEN]) {
    out.fill(0);
    out[0] = 2; // BOOTREPLY
    out[1] = 1; // Ethernet
    out[2] = 6;
    out[4..8].copy_from_slice(&request.xid);
    out[10..12].copy_from_slice(&request.flags);
    if reply != Reply::Nak {
        out[16..20].copy_from_slice(&CLIENT_IP);
    }
    out[20..24].copy_from_slice(&SERVER_IP);
    out[28..44].copy_from_slice(&request.chaddr);
    out[BOOTP_FIXED_LEN..OPTIONS_START].copy_from_slice(&MAGIC_COOKIE);

    let mut pos = OPTIONS_START;
    let message_type = match reply {
        Reply::Offer => DHCP_OFFER,
        Reply::Ack => DHCP_ACK,
        Reply::Nak => DHCP_NAK,
    };
    put_option(out, &mut pos, 53, &[message_type]);
    put_option(out, &mut pos, 54, &SERVER_IP);
    if reply != Reply::Nak {
        put_option(out, &mut pos, 51, &LEASE_SECONDS.to_be_bytes());
        put_option(out, &mut pos, 1, &[255, 255, 255, 0]);
        put_option(out, &mut pos, 3, &SERVER_IP);
    }
    out[pos] = 255;
}

fn put_option(out: &mut [u8; RESPONSE_LEN], pos: &mut usize, code: u8, value: &[u8]) {
    out[*pos] = code;
    out[*pos + 1] = value.len() as u8;
    out[*pos + 2..*pos + 2 + value.len()].copy_from_slice(value);
    *pos += value.len() + 2;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(kind: u8) -> [u8; 300] {
        let mut packet = [0u8; 300];
        packet[0] = 1;
        packet[1] = 1;
        packet[2] = 6;
        packet[4..8].copy_from_slice(&[1, 2, 3, 4]);
        packet[28..34].copy_from_slice(&[10, 11, 12, 13, 14, 15]);
        packet[236..240].copy_from_slice(&MAGIC_COOKIE);
        packet[240..244].copy_from_slice(&[53, 1, kind, 255]);
        packet
    }

    #[test]
    fn discover_builds_offer_and_echoes_identity() {
        let request = parse_request(&packet(DHCP_DISCOVER)).unwrap();
        assert_eq!(classify(&request), Some(Reply::Offer));
        let mut response = [0u8; RESPONSE_LEN];
        build_response(&request, Reply::Offer, &mut response);
        assert_eq!(&response[4..8], &[1, 2, 3, 4]);
        assert_eq!(&response[16..20], &CLIENT_IP);
        assert_eq!(&response[28..34], &[10, 11, 12, 13, 14, 15]);
        assert_eq!(&response[240..243], &[53, 1, DHCP_OFFER]);
    }

    #[test]
    fn requested_lease_is_acked() {
        let mut packet = packet(DHCP_REQUEST);
        packet[243..250].copy_from_slice(&[50, 4, 192, 168, 4, 2, 255]);
        let request = parse_request(&packet).unwrap();
        assert_eq!(classify(&request), Some(Reply::Ack));
    }

    #[test]
    fn renewal_from_lease_address_is_acked() {
        let mut packet = packet(DHCP_REQUEST);
        packet[12..16].copy_from_slice(&CLIENT_IP);
        let request = parse_request(&packet).unwrap();
        assert_eq!(classify(&request), Some(Reply::Ack));
    }

    #[test]
    fn wrong_address_is_naked() {
        let request = parse_request(&packet(DHCP_REQUEST)).unwrap();
        assert_eq!(classify(&request), Some(Reply::Nak));
    }

    #[test]
    fn request_for_another_server_is_ignored() {
        let mut packet = packet(DHCP_REQUEST);
        packet[243..250].copy_from_slice(&[54, 4, 10, 0, 0, 1, 255]);
        let request = parse_request(&packet).unwrap();
        assert_eq!(classify(&request), None);
    }

    #[test]
    fn truncated_and_bad_cookie_packets_are_ignored() {
        assert!(parse_request(&[0; 20]).is_none());
        assert!(parse_request(&packet(DHCP_DISCOVER)[..250]).is_some());
        let mut bad = packet(DHCP_DISCOVER);
        bad[236] = 0;
        assert!(parse_request(&bad).is_none());
    }
}
