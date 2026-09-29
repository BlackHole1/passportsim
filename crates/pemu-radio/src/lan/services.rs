//! The scripted services of the virtual LAN: DNS and HTTP, deterministic, class C.
//!
//! The script is what `probes/probe_wifi_http` asks for: `GET /probe` on port 80 of the DHCP
//! gateway. HTTP answers carry `Content-Length`, so a client that keeps the connection open knows
//! where the body ends. DNS is authoritative for [`ZONE`] and does not recurse (`AA` set, `RA`
//! clear); the encoder follows RFC 1035 section 4.1 because `smoltcp::wire` builds queries, not
//! answers.

use super::gateway::GATEWAY_IP;

pub const HTTP_PORT: u16 = 80;
pub const DNS_PORT: u16 = 53;
/// `probe_wifi_http`'s `CONFIG_PROBE_HTTP_PATH` default
/// (`probes/probe_wifi_http/main/Kconfig.projbuild`).
pub const PROBE_PATH: &str = "/probe";
/// Class C. The probe prints its SHA-256.
pub const PROBE_BODY: &[u8] = b"passportsim virtual LAN: scripted HTTP service\n";
pub const NOT_FOUND_BODY: &[u8] = b"not found\n";
/// A longer request head is answered `431`.
pub const MAX_HEAD: usize = 4096;
/// Seconds (class C).
pub const ZONE_TTL: u32 = 300;
/// Every name the server is authoritative for, with its address. `host.emu.internal` is the
/// gateway too: its allowlisted ports reach the host's loopback through an attached bridge, and
/// any other port on it is the gateway's own.
pub const ZONE: &[(&str, [u8; 4])] = &[
    ("gateway.passportsim.lan", GATEWAY_IP),
    (super::bridge::HOST_NAME, GATEWAY_IP),
];

/// `None` while the head is not yet complete. `GET` and `HEAD` of [`PROBE_PATH`] get
/// [`PROBE_BODY`], another path `404`, another method `501`.
pub fn http_answer(request: &[u8]) -> Option<Vec<u8>> {
    let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
        if request.len() > MAX_HEAD {
            return Some(response(431, "Request Header Fields Too Large", b"", true));
        }
        return None;
    };
    let head = String::from_utf8_lossy(&request[..end]);
    let line = head.lines().next().unwrap_or("");
    let mut parts = line.split(' ');
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let path = target.split('?').next().unwrap_or("");
    Some(match method {
        "GET" | "HEAD" => {
            let body = method == "GET";
            if path == PROBE_PATH {
                response(200, "OK", PROBE_BODY, body)
            } else {
                response(404, "Not Found", NOT_FOUND_BODY, body)
            }
        }
        _ => response(501, "Not Implemented", b"", true),
    })
}

fn response(status: u16, reason: &str, body: &[u8], with_body: bool) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    if with_body {
        out.extend_from_slice(body);
    }
    out
}

/// `RCODE` 3, name error (RFC 1035 section 4.1.1).
const NXDOMAIN: u8 = 3;
/// `TYPE` A and `CLASS` IN.
const TYPE_A: u16 = 1;
const CLASS_IN: u16 = 1;

/// `None` for a message that is not a standard query of exactly one question, which a server
/// drops. A non-`A` question for a zone name gets an empty `NOERROR`, a name outside it
/// `NXDOMAIN`.
pub fn dns_answer(query: &[u8]) -> Option<Vec<u8>> {
    if query.len() < 12 {
        return None;
    }
    let flags = u16::from_be_bytes([query[2], query[3]]);
    let qr = flags >> 15;
    let opcode = (flags >> 11) & 0xF;
    let qdcount = u16::from_be_bytes([query[4], query[5]]);
    if qr != 0 || opcode != 0 || qdcount != 1 {
        return None;
    }
    // The question: labels up to the root, then QTYPE and QCLASS. A query carries no pointer.
    let mut at = 12;
    let mut labels: Vec<String> = Vec::new();
    loop {
        let len = usize::from(*query.get(at)?);
        at += 1;
        if len == 0 {
            break;
        }
        if len > 63 {
            return None;
        }
        labels.push(String::from_utf8_lossy(query.get(at..at + len)?).to_ascii_lowercase());
        at += len;
    }
    let qtype = u16::from_be_bytes([*query.get(at)?, *query.get(at + 1)?]);
    let qclass = u16::from_be_bytes([*query.get(at + 2)?, *query.get(at + 3)?]);
    let question_end = at + 4;
    let name = labels.join(".");
    let found = ZONE.iter().find(|(n, _)| *n == name).map(|(_, ip)| *ip);
    let rd = flags & 0x0100;
    let (rcode, answer) = match found {
        None => (NXDOMAIN, None),
        Some(ip) if qtype == TYPE_A && qclass == CLASS_IN => (0, Some(ip)),
        Some(_) => (0, None),
    };
    let mut out = Vec::with_capacity(question_end + 16);
    out.extend_from_slice(&query[0..2]);
    // QR, opcode 0, AA, RD as asked, RA clear, the rcode.
    let flags = 0x8000 | 0x0400 | rd | u16::from(rcode);
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&u16::from(answer.is_some()).to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&query[12..question_end]);
    if let Some(ip) = answer {
        // The name as a pointer to the question's (RFC 1035 section 4.1.4), then TYPE, CLASS, TTL,
        // RDLENGTH and the address.
        out.extend_from_slice(&0xC00Cu16.to_be_bytes());
        out.extend_from_slice(&TYPE_A.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        out.extend_from_slice(&ZONE_TTL.to_be_bytes());
        out.extend_from_slice(&4u16.to_be_bytes());
        out.extend_from_slice(&ip);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_get_of_the_probe_path_is_200_with_the_scripted_body() {
        assert_eq!(http_answer(b"GET /probe HTTP/1.1\r\nHost: 10.23.0.1"), None);
        let answer = http_answer(
            b"GET /probe HTTP/1.1\r\nHost: 10.23.0.1\r\nUser-Agent: ESP32 HTTP Client/1.0\r\n\r\n",
        )
        .expect("a whole head");
        let text = String::from_utf8(answer).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(text.contains(&format!("Content-Length: {}\r\n", PROBE_BODY.len())));
        assert!(text.ends_with(std::str::from_utf8(PROBE_BODY).unwrap()));
    }

    #[test]
    fn other_paths_and_methods_are_answered_by_status() {
        let not_found = http_answer(b"GET /x HTTP/1.1\r\n\r\n").unwrap();
        assert!(not_found.starts_with(b"HTTP/1.1 404 Not Found\r\n"));
        let head = http_answer(b"HEAD /probe HTTP/1.1\r\n\r\n").unwrap();
        assert!(head.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(head.ends_with(b"\r\n\r\n"), "no body for HEAD");
        let post = http_answer(b"POST /probe HTTP/1.1\r\n\r\n").unwrap();
        assert!(post.starts_with(b"HTTP/1.1 501 "));
        assert!(
            http_answer(&vec![b'a'; MAX_HEAD + 1])
                .unwrap()
                .starts_with(b"HTTP/1.1 431 ")
        );
    }

    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&1u16.to_be_bytes());
        q
    }

    #[test]
    fn a_zone_name_resolves_and_any_other_is_nxdomain() {
        let q = query("Gateway.passportsim.lan", 1);
        let a = dns_answer(&q).expect("an answer");
        assert_eq!(&a[0..2], &[0x12, 0x34], "the query's id");
        assert_eq!(
            u16::from_be_bytes([a[2], a[3]]),
            0x8500,
            "QR, AA, RD, NOERROR"
        );
        assert_eq!(u16::from_be_bytes([a[6], a[7]]), 1, "one answer");
        assert_eq!(&a[a.len() - 4..], &GATEWAY_IP);
        let nx = dns_answer(&query("example.com", 1)).unwrap();
        assert_eq!(nx[3] & 0xF, 3);
        assert_eq!(u16::from_be_bytes([nx[6], nx[7]]), 0);
        let aaaa = dns_answer(&query("gateway.passportsim.lan", 28)).unwrap();
        assert_eq!(aaaa[3] & 0xF, 0);
        assert_eq!(u16::from_be_bytes([aaaa[6], aaaa[7]]), 0);
        let mut response = query("x", 1);
        response[2] |= 0x80;
        assert_eq!(dns_answer(&response), None, "a response is not answered");
    }
}
