//! A WebSocket client (RFC 6455) over any blocking byte stream, for a server
//! that speaks text frames. No TLS, no extensions, no compression.

use std::fmt;
use std::io::{self, Read, Write};
use std::time::Duration;

const OP_CONT: u8 = 0x0;
pub const OP_TEXT: u8 = 0x1;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

const FIN: u8 = 0x80;
const MASKED: u8 = 0x80;
const LEN_16: u8 = 126;
const LEN_64: u8 = 127;
const MAX_SHORT_LEN: usize = 125;

/// A frame or message bigger than this is refused rather than buffered.
const MAX_PAYLOAD: usize = 1 << 20;
const MAX_HEAD: usize = 16 * 1024;
const READ_CHUNK: usize = 4096;
const HANDSHAKE_WAIT: Duration = Duration::from_secs(2);
const KEY_BYTES: usize = 16;

/// A stream whose reads can be told how long to wait.
pub trait Link: Read + Write {
    fn set_read_timeout(&mut self, timeout: Option<Duration>);
}

#[derive(Debug)]
pub enum WsError {
    Io(io::Error),
    Closed,
    Handshake(String),
    Protocol(String),
}

impl fmt::Display for WsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WsError::Io(error) => write!(f, "{error}"),
            WsError::Closed => write!(f, "the server closed the connection"),
            WsError::Handshake(why) => write!(f, "the WebSocket handshake failed: {why}"),
            WsError::Protocol(why) => write!(f, "the server broke the WebSocket protocol: {why}"),
        }
    }
}

impl From<io::Error> for WsError {
    fn from(error: io::Error) -> Self {
        WsError::Io(error)
    }
}

/// Where a `ws://` address points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// Intiface Central's default port, for an address that names none.
const DEFAULT_PORT: u16 = 12345;

impl Endpoint {
    /// `ws://127.0.0.1:12345` or `ws://host:port/path`.
    pub fn parse(url: &str) -> Result<Self, String> {
        let url = url.trim();
        let Some(rest) = url.strip_prefix("ws://") else {
            return Err("the address must start with ws://".to_string());
        };
        let (authority, path) = match rest.find('/') {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !port.contains(']') => {
                let port = port
                    .parse::<u16>()
                    .map_err(|_| format!("`{port}` is not a port"))?;
                (host, port)
            }
            _ => (authority, DEFAULT_PORT),
        };
        if host.is_empty() {
            return Err("the address has no host".to_string());
        }
        Ok(Endpoint {
            host: host.to_string(),
            port,
            path: path.to_string(),
        })
    }
}

/// One decoded frame.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame {
    pub fin: bool,
    pub opcode: u8,
    pub payload: Vec<u8>,
}

/// The HTTP request that upgrades the connection.
pub fn handshake_request(endpoint: &Endpoint, key: &str) -> String {
    format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n",
        endpoint.path, endpoint.host, endpoint.port
    )
}

/// Accepts only `101 Switching Protocols`; the accept hash is not verified,
/// since nothing here depends on the answer being from a WebSocket proper.
pub fn check_response(head: &str) -> Result<(), WsError> {
    let status = head.lines().next().unwrap_or_default();
    let mut parts = status.split_whitespace();
    let version = parts.next().unwrap_or_default();
    match (version.starts_with("HTTP/1."), parts.next()) {
        (true, Some("101")) => Ok(()),
        _ => Err(WsError::Handshake(format!(
            "the server answered `{status}`"
        ))),
    }
}

/// Encodes one frame; a client frame carries a `mask`, a server frame none.
pub fn encode_frame(opcode: u8, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
    let mask_bit = if mask.is_some() { MASKED } else { 0 };
    let mut out = vec![FIN | opcode];

    match payload.len() {
        len if len <= MAX_SHORT_LEN => out.push(mask_bit | len as u8),
        len if len <= u16::MAX as usize => {
            out.push(mask_bit | LEN_16);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        len => {
            out.push(mask_bit | LEN_64);
            out.extend_from_slice(&(len as u64).to_be_bytes());
        }
    }

    match mask {
        Some(key) => {
            out.extend_from_slice(&key);
            out.extend(
                payload
                    .iter()
                    .enumerate()
                    .map(|(i, byte)| byte ^ key[i % 4]),
            );
        }
        None => out.extend_from_slice(payload),
    }
    out
}

/// Decodes the first frame in `bytes`: the frame and how many bytes it took,
/// or `None` while it is incomplete.
pub fn decode_frame(bytes: &[u8]) -> Result<Option<(Frame, usize)>, WsError> {
    if bytes.len() < 2 {
        return Ok(None);
    }
    let fin = bytes[0] & FIN != 0;
    let opcode = bytes[0] & 0x0F;
    let masked = bytes[1] & MASKED != 0;

    let (len, mut at) = match bytes[1] & !MASKED {
        LEN_16 => match bytes.get(2..4) {
            Some(raw) => (u16::from_be_bytes([raw[0], raw[1]]) as u64, 4),
            None => return Ok(None),
        },
        LEN_64 => match bytes.get(2..10) {
            Some(raw) => (u64::from_be_bytes(raw.try_into().expect("8 bytes")), 10),
            None => return Ok(None),
        },
        short => (short as u64, 2),
    };
    if len > MAX_PAYLOAD as u64 {
        return Err(WsError::Protocol("a frame is too large".to_string()));
    }
    let len = len as usize;

    let mut key = [0u8; 4];
    if masked {
        match bytes.get(at..at + 4) {
            Some(raw) => key.copy_from_slice(raw),
            None => return Ok(None),
        }
        at += 4;
    }
    let Some(raw) = bytes.get(at..at + len) else {
        return Ok(None);
    };
    let payload = match masked {
        true => raw
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ key[i % 4])
            .collect(),
        false => raw.to_vec(),
    };
    Ok(Some((
        Frame {
            fin,
            opcode,
            payload,
        },
        at + len,
    )))
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            match i <= chunk.len() {
                true => out.push(BASE64[(n >> (18 - 6 * i) & 63) as usize] as char),
                false => out.push('='),
            }
        }
    }
    out
}

pub struct WebSocket<L: Link> {
    link: L,
    buf: Vec<u8>,
    /// The fragments of a text message so far, while one is being assembled.
    message: Option<Vec<u8>>,
    rng: u64,
}

impl<L: Link> WebSocket<L> {
    /// Upgrades `link`. Masks and the key need no secrecy, only variety, so a
    /// xorshift seeded by `seed` is enough.
    pub fn connect(link: L, endpoint: &Endpoint, seed: u64) -> Result<Self, WsError> {
        let mut socket = WebSocket {
            link,
            buf: Vec::new(),
            message: None,
            rng: seed | 1,
        };
        let key: Vec<u8> = (0..KEY_BYTES).map(|_| socket.next_random() as u8).collect();
        socket
            .link
            .write_all(handshake_request(endpoint, &base64(&key)).as_bytes())?;
        socket.link.flush()?;
        socket.read_head()?;
        Ok(socket)
    }

    fn next_random(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    /// Reads the HTTP answer up to its blank line; whatever follows stays buffered.
    fn read_head(&mut self) -> Result<(), WsError> {
        loop {
            if let Some(end) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&self.buf[..end]).into_owned();
                self.buf.drain(..end + 4);
                return check_response(&head);
            }
            if self.buf.len() > MAX_HEAD {
                return Err(WsError::Handshake("the answer is too long".to_string()));
            }
            if !self.fill(HANDSHAKE_WAIT)? {
                return Err(WsError::Handshake("the server did not answer".to_string()));
            }
        }
    }

    /// Reads once; `false` when nothing came within `wait`.
    fn fill(&mut self, wait: Duration) -> Result<bool, WsError> {
        let mut chunk = [0u8; READ_CHUNK];
        self.link.set_read_timeout(Some(wait));
        match self.link.read(&mut chunk) {
            Ok(0) => Err(WsError::Closed),
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                Ok(true)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn send(&mut self, opcode: u8, payload: &[u8]) -> Result<(), WsError> {
        let mask = (self.next_random() as u32).to_be_bytes();
        self.link
            .write_all(&encode_frame(opcode, payload, Some(mask)))?;
        self.link.flush()?;
        Ok(())
    }

    pub fn send_text(&mut self, text: &str) -> Result<(), WsError> {
        self.send(OP_TEXT, text.as_bytes())
    }

    /// The next text message, waiting up to `wait` for more bytes; `None`
    /// when nothing complete arrived. Pings are answered on the way.
    pub fn next_text(&mut self, wait: Duration) -> Result<Option<String>, WsError> {
        loop {
            if let Some((frame, used)) = decode_frame(&self.buf)? {
                self.buf.drain(..used);
                if let Some(text) = self.on_frame(frame)? {
                    return Ok(Some(text));
                }
                continue;
            }
            if !self.fill(wait)? {
                return Ok(None);
            }
        }
    }

    fn on_frame(&mut self, frame: Frame) -> Result<Option<String>, WsError> {
        match frame.opcode {
            OP_PING => self.send(OP_PONG, &frame.payload).map(|_| None),
            OP_CLOSE => {
                // Echo the close so the server can finish; failing to is moot.
                let _ = self.send(OP_CLOSE, &frame.payload);
                Err(WsError::Closed)
            }
            OP_TEXT => {
                self.message = Some(Vec::new());
                self.on_fragment(frame)
            }
            OP_CONT => self.on_fragment(frame),
            _ => Ok(None),
        }
    }

    fn on_fragment(&mut self, frame: Frame) -> Result<Option<String>, WsError> {
        let Some(message) = self.message.as_mut() else {
            return Ok(None);
        };
        message.extend_from_slice(&frame.payload);
        if message.len() > MAX_PAYLOAD {
            return Err(WsError::Protocol("a message is too large".to_string()));
        }
        if !frame.fin {
            return Ok(None);
        }
        let bytes = self.message.take().unwrap_or_default();
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| WsError::Protocol("a text message is not UTF-8".to_string()))
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use std::net::TcpStream;

    /// A scripted stream: reads what it was given, then reports a timeout.
    pub struct Pipe {
        pub input: Vec<u8>,
        pub output: Vec<u8>,
    }

    impl Read for Pipe {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.input.is_empty() {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let n = buf.len().min(self.input.len());
            buf[..n].copy_from_slice(&self.input[..n]);
            self.input.drain(..n);
            Ok(n)
        }
    }

    impl Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Link for Pipe {
        fn set_read_timeout(&mut self, _: Option<Duration>) {}
    }

    impl Link for TcpStream {
        fn set_read_timeout(&mut self, timeout: Option<Duration>) {
            TcpStream::set_read_timeout(self, timeout).expect("set timeout");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Pipe;
    use super::*;

    const WAIT: Duration = Duration::from_millis(1);
    const UPGRADE: &str = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n";

    fn endpoint() -> Endpoint {
        Endpoint::parse("ws://127.0.0.1:12345").unwrap()
    }

    fn open(after_head: &[u8]) -> WebSocket<Pipe> {
        let mut input = UPGRADE.as_bytes().to_vec();
        input.extend_from_slice(after_head);
        WebSocket::connect(
            Pipe {
                input,
                output: Vec::new(),
            },
            &endpoint(),
            7,
        )
        .unwrap()
    }

    #[test]
    fn an_address_gives_host_port_and_path() {
        let found = Endpoint::parse("ws://localhost:99/x").unwrap();
        assert_eq!(
            (found.host.as_str(), found.port, found.path.as_str()),
            ("localhost", 99, "/x")
        );
        assert_eq!(Endpoint::parse("ws://box").unwrap().port, DEFAULT_PORT);
        assert!(Endpoint::parse("http://box:1").is_err());
        assert!(Endpoint::parse("ws://box:nope").is_err());
    }

    #[test]
    fn the_handshake_asks_for_an_upgrade() {
        let request = handshake_request(&endpoint(), "a2V5");
        assert!(request.starts_with("GET / HTTP/1.1\r\nHost: 127.0.0.1:12345\r\n"));
        assert!(request.contains("Upgrade: websocket\r\n"));
        assert!(request.contains("Sec-WebSocket-Key: a2V5\r\n"));
        assert!(request.contains("Sec-WebSocket-Version: 13\r\n"));
        assert!(request.ends_with("\r\n\r\n"));
    }

    #[test]
    fn only_a_101_answer_passes() {
        assert!(check_response("HTTP/1.1 101 Switching Protocols\r\nA: b").is_ok());
        assert!(check_response("HTTP/1.1 200 OK").is_err());
        assert!(check_response("").is_err());
    }

    #[test]
    fn the_key_is_base64() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
        assert_eq!(base64(&[0u8; KEY_BYTES]).len(), 24);
    }

    #[test]
    fn a_client_frame_is_masked() {
        let mask = [1, 2, 3, 4];
        let frame = encode_frame(OP_TEXT, b"hi", Some(mask));
        assert_eq!(frame, vec![0x81, 0x82, 1, 2, 3, 4, b'h' ^ 1, b'i' ^ 2]);
        let (decoded, used) = decode_frame(&frame).unwrap().unwrap();
        assert_eq!((decoded.payload, used), (b"hi".to_vec(), frame.len()));
    }

    #[test]
    fn short_medium_and_long_lengths_round_trip() {
        for len in [0, 125, 126, 65_535, 65_536] {
            let payload = vec![b'x'; len];
            let frame = encode_frame(OP_TEXT, &payload, None);
            let head = match len {
                0..=125 => 2,
                126..=65_535 => 4,
                _ => 10,
            };
            assert_eq!(frame.len(), head + len);
            let (decoded, used) = decode_frame(&frame).unwrap().unwrap();
            assert_eq!((decoded.payload.len(), used), (len, frame.len()));
        }
    }

    #[test]
    fn a_partial_frame_waits_and_a_huge_one_is_refused() {
        let frame = encode_frame(OP_TEXT, &[b'x'; 300], None);
        assert!(decode_frame(&frame[..3]).unwrap().is_none());
        assert!(decode_frame(&frame[..100]).unwrap().is_none());

        let mut huge = vec![0x81, LEN_64];
        huge.extend_from_slice(&(MAX_PAYLOAD as u64 + 1).to_be_bytes());
        assert!(decode_frame(&huge).is_err());
    }

    #[test]
    fn a_ping_is_answered_with_a_masked_pong() {
        let mut socket = open(&encode_frame(OP_PING, b"yo", None));
        assert_eq!(socket.next_text(WAIT).unwrap(), None);

        let sent = &socket.link.output;
        let request_len = handshake_request(&endpoint(), &base64(&[0; KEY_BYTES])).len();
        let (pong, _) = decode_frame(&sent[request_len..]).unwrap().unwrap();
        assert_eq!((pong.opcode, pong.payload), (OP_PONG, b"yo".to_vec()));
        assert_ne!(sent[request_len + 1] & MASKED, 0);
    }

    #[test]
    fn fragments_join_into_one_text_message() {
        let mut first = encode_frame(OP_TEXT, b"hel", None);
        first[0] &= !FIN;
        let mut bytes = first;
        bytes.extend(encode_frame(OP_CONT, b"lo", None));
        let mut socket = open(&bytes);
        assert_eq!(socket.next_text(WAIT).unwrap().as_deref(), Some("hello"));
    }

    #[test]
    fn bytes_after_the_head_and_split_frames_are_kept() {
        let frame = encode_frame(OP_TEXT, b"split", None);
        let mut socket = open(&frame[..3]);
        assert_eq!(socket.next_text(WAIT).unwrap(), None);
        socket.link.input.extend_from_slice(&frame[3..]);
        assert_eq!(socket.next_text(WAIT).unwrap().as_deref(), Some("split"));
    }

    #[test]
    fn a_close_ends_the_stream() {
        let mut socket = open(&encode_frame(OP_CLOSE, &[], None));
        assert!(matches!(socket.next_text(WAIT), Err(WsError::Closed)));
    }

    #[test]
    fn a_refused_upgrade_fails_the_connect() {
        let pipe = Pipe {
            input: b"HTTP/1.1 403 Forbidden\r\n\r\n".to_vec(),
            output: Vec::new(),
        };
        assert!(matches!(
            WebSocket::connect(pipe, &endpoint(), 1),
            Err(WsError::Handshake(_))
        ));
    }
}
