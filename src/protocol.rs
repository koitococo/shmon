use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::io;
use tokio_util::codec::{Decoder, Encoder};

const TYPE_DATA: u8 = 0;
const TYPE_RESIZE: u8 = 1;
const TYPE_SIGNAL: u8 = 2;
const TYPE_EXIT: u8 = 3;
const HEADER_LEN: usize = 5;
const MAX_PAYLOAD: usize = 1024 * 1024; // 1MB

#[derive(Debug, Clone)]
pub enum Packet {
    Data(Bytes),
    Resize(u16, u16),
    Signal(i32),
    Exit(ExitStatus), // Shell exit status
}

/// Shell exit status information
#[derive(Debug, Clone)]
pub struct ExitStatus {
    /// Exit code if process exited normally
    pub code: Option<i32>,
    /// Signal number if process was killed by signal
    pub signal: Option<i32>,
}

impl ExitStatus {
    /// Create an ExitStatus from a normal exit code
    pub fn from_code(code: i32) -> Self {
        Self {
            code: Some(code),
            signal: None,
        }
    }

    /// Create an ExitStatus from a signal
    pub fn from_signal(signal: i32) -> Self {
        Self {
            code: None,
            signal: Some(signal),
        }
    }

    /// Format the exit status as a human-readable message
    pub fn format_message(&self) -> String {
        match (self.code, self.signal) {
            (Some(code), None) => format!("[shell exited with code {}]\r\n", code),
            (None, Some(sig)) => format!("[shell killed by signal {}]\r\n", sig),
            _ => "[shell exited]\r\n".to_string(),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct PacketCodec;

impl Encoder<Packet> for PacketCodec {
    type Error = io::Error;

    fn encode(&mut self, item: Packet, dst: &mut BytesMut) -> Result<(), Self::Error> {
        match item {
            Packet::Data(data) => {
                let len = data.len();
                dst.reserve(HEADER_LEN + len);
                dst.put_u8(TYPE_DATA);
                dst.put_u32(len as u32);
                dst.extend_from_slice(&data);
            }
            Packet::Resize(cols, rows) => {
                dst.reserve(HEADER_LEN + 4);
                dst.put_u8(TYPE_RESIZE);
                dst.put_u32(4);
                dst.put_u16(cols);
                dst.put_u16(rows);
            }
            Packet::Signal(signal) => {
                dst.reserve(HEADER_LEN + 4);
                dst.put_u8(TYPE_SIGNAL);
                dst.put_u32(4);
                dst.put_i32(signal);
            }
            Packet::Exit(status) => {
                dst.reserve(HEADER_LEN + 8);
                dst.put_u8(TYPE_EXIT);
                dst.put_u32(8);
                dst.put_i32(status.code.unwrap_or(-1));
                dst.put_i32(status.signal.unwrap_or(0));
            }
        }
        Ok(())
    }
}

impl Decoder for PacketCodec {
    type Item = Packet;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < HEADER_LEN {
            return Ok(None);
        }
        let mut header = &src[..HEADER_LEN];
        let packet_type = header.get_u8();
        let len = header.get_u32() as usize;
        if len > MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("payload too large: {} bytes", len),
            ));
        }
        if src.len() < HEADER_LEN + len {
            return Ok(None);
        }
        src.advance(HEADER_LEN);
        let payload = src.split_to(len).freeze();
        let packet = match packet_type {
            TYPE_DATA => Packet::Data(payload),
            TYPE_RESIZE => {
                if payload.len() != 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid resize payload",
                    ));
                }
                let mut buf = payload.as_ref();
                let cols = buf.get_u16();
                let rows = buf.get_u16();
                Packet::Resize(cols, rows)
            }
            TYPE_SIGNAL => {
                if payload.len() != 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid signal payload",
                    ));
                }
                let mut buf = payload.as_ref();
                let signal = buf.get_i32();
                Packet::Signal(signal)
            }
            TYPE_EXIT => {
                if payload.len() != 8 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid exit payload",
                    ));
                }
                let mut buf = payload.as_ref();
                let code = buf.get_i32();
                let signal = buf.get_i32();
                let status = if signal > 0 {
                    ExitStatus::from_signal(signal)
                } else if code >= 0 {
                    ExitStatus::from_code(code)
                } else {
                    ExitStatus {
                        code: None,
                        signal: None,
                    }
                };
                Packet::Exit(status)
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unknown packet type",
                ));
            }
        };
        Ok(Some(packet))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    #[test]
    fn rejects_oversized_payload() {
        let mut codec = PacketCodec;
        let mut buf = BytesMut::new();
        buf.put_u8(TYPE_DATA);
        buf.put_u32((MAX_PAYLOAD + 1) as u32);
        buf.put_bytes(0, 1024);
        let result = codec.decode(&mut buf);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn encodes_decodes_data() {
        let mut codec = PacketCodec;
        let mut buf = BytesMut::new();
        let data = Bytes::from_static(b"hello world");
        codec.encode(Packet::Data(data.clone()), &mut buf).unwrap();
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        match decoded {
            Packet::Data(d) => assert_eq!(d, data),
            _ => panic!("expected Data"),
        }
    }

    #[test]
    fn encodes_decodes_resize() {
        let mut codec = PacketCodec;
        let mut buf = BytesMut::new();
        codec.encode(Packet::Resize(80, 24), &mut buf).unwrap();
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        match decoded {
            Packet::Resize(cols, rows) => {
                assert_eq!(cols, 80);
                assert_eq!(rows, 24);
            }
            _ => panic!("expected Resize"),
        }
    }

    #[test]
    fn encodes_decodes_signal() {
        let mut codec = PacketCodec;
        let mut buf = BytesMut::new();
        codec.encode(Packet::Signal(15), &mut buf).unwrap();
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        match decoded {
            Packet::Signal(sig) => assert_eq!(sig, 15),
            _ => panic!("expected Signal"),
        }
    }

    #[test]
    fn encodes_decodes_exit_code() {
        let mut codec = PacketCodec;
        let mut buf = BytesMut::new();
        let status = ExitStatus::from_code(42);
        codec.encode(Packet::Exit(status), &mut buf).unwrap();
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        match decoded {
            Packet::Exit(s) => {
                assert_eq!(s.code, Some(42));
                assert_eq!(s.signal, None);
            }
            _ => panic!("expected Exit"),
        }
    }

    #[test]
    fn encodes_decodes_exit_signal() {
        let mut codec = PacketCodec;
        let mut buf = BytesMut::new();
        let status = ExitStatus::from_signal(9);
        codec.encode(Packet::Exit(status), &mut buf).unwrap();
        let decoded = codec.decode(&mut buf).unwrap().unwrap();
        match decoded {
            Packet::Exit(s) => {
                assert_eq!(s.code, None);
                assert_eq!(s.signal, Some(9));
            }
            _ => panic!("expected Exit"),
        }
    }

    #[test]
    fn rejects_unknown_packet_type() {
        let mut codec = PacketCodec;
        let mut buf = BytesMut::new();
        buf.put_u8(99); // unknown type
        buf.put_u32(0);
        let result = codec.decode(&mut buf);
        assert!(result.is_err());
    }
}
