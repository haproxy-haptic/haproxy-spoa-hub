use std::collections::HashMap;
use std::time::Duration;

use spop::types::TypedData;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// A mock SPOP client that simulates HAProxy connecting to an SPOE agent.
/// Uses raw byte encoding/decoding because the spop codec only supports
/// decoding HAProxy-side frames (1-3), not agent-side frames (101-103).
pub struct SpopClient<S> {
    stream: S,
    frame_id: u64,
}

impl<S: AsyncRead + AsyncWrite + Unpin> SpopClient<S> {
    /// Perform HAPROXY-HELLO / AGENT-HELLO handshake.
    pub async fn handshake(mut stream: S) -> anyhow::Result<Self> {
        // Send HAPROXY-HELLO
        let hello = build_haproxy_hello();
        stream.write_all(&hello).await?;
        stream.flush().await?;

        // Read AGENT-HELLO response
        let frame = read_frame(&mut stream).await?;
        let frame_type = frame[0];
        assert_eq!(
            frame_type, 101,
            "expected AGENT-HELLO (101), got {frame_type}"
        );

        Ok(Self {
            stream,
            frame_id: 0,
        })
    }

    /// Send NOTIFY and return ACK response.
    pub async fn send_notify(
        &mut self,
        message_name: &str,
        args: HashMap<String, TypedData>,
    ) -> anyhow::Result<AckResponse> {
        self.frame_id += 1;
        let notify = build_notify_frame(1, self.frame_id, message_name, &args);
        self.stream.write_all(&notify).await?;
        self.stream.flush().await?;

        // Read ACK
        let frame =
            tokio::time::timeout(Duration::from_secs(5), read_frame(&mut self.stream)).await??;

        let frame_type = frame[0];
        assert_eq!(frame_type, 103, "expected ACK (103), got {frame_type}");

        // Parse ACK: skip frame_type(1) + flags(4) + stream_id(varint) + frame_id(varint)
        let mut pos = 1;
        pos += 4; // flags
        let (stream_id, bytes_read) = decode_varint(&frame[pos..]);
        pos += bytes_read;
        let (frame_id, bytes_read) = decode_varint(&frame[pos..]);
        pos += bytes_read;

        // Parse actions
        let actions = parse_actions(&frame[pos..]);

        Ok(AckResponse {
            stream_id,
            frame_id,
            actions,
        })
    }

    /// Send HAPROXY-DISCONNECT.
    pub async fn disconnect(&mut self) -> anyhow::Result<()> {
        let frame = build_haproxy_disconnect();
        self.stream.write_all(&frame).await?;
        self.stream.flush().await?;
        Ok(())
    }
}

/// Read a single SPOP frame (4-byte length prefix + frame body).
async fn read_frame<S: AsyncRead + Unpin>(stream: &mut S) -> anyhow::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut frame = vec![0u8; len];
    stream.read_exact(&mut frame).await?;
    Ok(frame)
}

/// Assemble a length-prefixed SPOP frame from its components.
fn assemble_frame(frame_type: u8, metadata: &[u8], payload: &[u8]) -> Vec<u8> {
    let frame_len = 1 + metadata.len() + payload.len();
    let mut frame = Vec::with_capacity(4 + frame_len);
    frame.extend_from_slice(&(frame_len as u32).to_be_bytes());
    frame.push(frame_type);
    frame.extend_from_slice(metadata);
    frame.extend_from_slice(payload);
    frame
}

fn build_haproxy_hello() -> Vec<u8> {
    let mut payload = Vec::new();

    // supported-versions
    encode_spop_string(&mut payload, "supported-versions");
    payload.push(8); // string type
    encode_spop_string(&mut payload, "2.0");

    // max-frame-size
    encode_spop_string(&mut payload, "max-frame-size");
    payload.push(3); // uint32 type
    encode_varint(&mut payload, 16384);

    // capabilities
    encode_spop_string(&mut payload, "capabilities");
    payload.push(8); // string type
    encode_spop_string(&mut payload, "pipelining");

    // engine-id
    encode_spop_string(&mut payload, "engine-id");
    payload.push(8);
    encode_spop_string(&mut payload, "test-engine");

    let mut metadata = Vec::new();
    metadata.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // flags: FIN
    encode_varint(&mut metadata, 0); // stream_id
    encode_varint(&mut metadata, 0); // frame_id

    assemble_frame(1, &metadata, &payload)
}

fn build_notify_frame(
    stream_id: u64,
    frame_id: u64,
    message_name: &str,
    args: &HashMap<String, TypedData>,
) -> Vec<u8> {
    let mut metadata = Vec::new();
    metadata.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // flags: FIN
    encode_varint(&mut metadata, stream_id);
    encode_varint(&mut metadata, frame_id);

    let mut payload = Vec::new();
    encode_spop_string(&mut payload, message_name);
    encode_varint(&mut payload, args.len() as u64);
    for (key, value) in args {
        encode_spop_string(&mut payload, key);
        encode_typed_data(&mut payload, value);
    }

    assemble_frame(3, &metadata, &payload)
}

fn build_haproxy_disconnect() -> Vec<u8> {
    let mut payload = Vec::new();
    encode_spop_string(&mut payload, "status-code");
    payload.push(3); // uint32
    encode_varint(&mut payload, 0);
    encode_spop_string(&mut payload, "message");
    payload.push(8); // string
    encode_spop_string(&mut payload, "test complete");

    let mut metadata = Vec::new();
    metadata.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // FIN
    encode_varint(&mut metadata, 0);
    encode_varint(&mut metadata, 0);

    assemble_frame(2, &metadata, &payload)
}

/// Parse ACK actions from the payload portion of an ACK frame.
///
/// SPOP action format:
///   SET-VAR:   <1:1byte> <NB-ARGS:1byte=3> <SCOPE:1byte> <NAME:string> <VALUE:typed-data>
///   UNSET-VAR: <2:1byte> <NB-ARGS:1byte=2> <SCOPE:1byte> <NAME:string>
fn parse_actions(data: &[u8]) -> Vec<AckAction> {
    let mut actions = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let action_type = data[pos];
        pos += 1;
        pos += 1; // skip nb-args byte

        let scope = data[pos];
        pos += 1;
        let scope_name = match scope {
            0 => "Process",
            1 => "Session",
            2 => "Transaction",
            3 => "Request",
            4 => "Response",
            _ => "Unknown",
        };

        let (name, bytes_read) = decode_spop_string(&data[pos..]);
        pos += bytes_read;

        match action_type {
            1 => {
                // set-var: also has a value
                let (value, bytes_read) = decode_typed_data(&data[pos..]);
                pos += bytes_read;
                actions.push(AckAction::SetVar {
                    scope: scope_name,
                    name,
                    value,
                });
            }
            2 => {
                // unset-var: no value
                actions.push(AckAction::UnSetVar {
                    scope: scope_name,
                    name,
                });
            }
            _ => break,
        }
    }
    actions
}

fn encode_varint(buf: &mut Vec<u8>, value: u64) {
    // Match spop crate's exact varint encoding (Peers encoding)
    if value < 240 {
        buf.push(value as u8);
    } else {
        buf.push((value | 0b1111_0000) as u8);
        let mut i = (value - 240) >> 4;
        while i >= 128 {
            buf.push((i | 0b1000_0000) as u8);
            i = (i - 128) >> 7;
        }
        buf.push(i as u8);
    }
}

fn decode_varint(data: &[u8]) -> (u64, usize) {
    // Match spop crate's exact varint decoding (Peers encoding)
    let mut pos = 0;
    let first = data[pos] as u64;
    pos += 1;
    if first < 240 {
        return (first, pos);
    }
    let mut value = first;
    let mut shift = 4u32;
    loop {
        let byte = data[pos] as u64;
        pos += 1;
        value += byte << shift;
        shift += 7;
        if byte < 128 {
            break;
        }
    }
    (value, pos)
}

fn encode_spop_string(buf: &mut Vec<u8>, s: &str) {
    encode_varint(buf, s.len() as u64);
    buf.extend_from_slice(s.as_bytes());
}

fn decode_spop_string(data: &[u8]) -> (String, usize) {
    let (len, bytes_read) = decode_varint(data);
    let s = String::from_utf8_lossy(&data[bytes_read..bytes_read + len as usize]).into_owned();
    (s, bytes_read + len as usize)
}

fn encode_typed_data(buf: &mut Vec<u8>, td: &TypedData) {
    match td {
        TypedData::Null => buf.push(0),
        TypedData::Bool(true) => buf.push(1 | (1 << 4)),
        TypedData::Bool(false) => buf.push(1),
        TypedData::Int32(v) => {
            buf.push(2);
            encode_varint(buf, *v as u64);
        }
        TypedData::UInt32(v) => {
            buf.push(3);
            encode_varint(buf, u64::from(*v));
        }
        TypedData::Int64(v) => {
            buf.push(4);
            encode_varint(buf, *v as u64);
        }
        TypedData::UInt64(v) => {
            buf.push(5);
            encode_varint(buf, *v);
        }
        TypedData::IPv4(addr) => {
            buf.push(6);
            buf.extend_from_slice(&addr.octets());
        }
        TypedData::IPv6(addr) => {
            buf.push(7);
            buf.extend_from_slice(&addr.octets());
        }
        TypedData::String(s) => {
            buf.push(8);
            encode_spop_string(buf, s);
        }
        TypedData::Binary(b) => {
            buf.push(9);
            encode_varint(buf, b.len() as u64);
            buf.extend_from_slice(b);
        }
    }
}

fn decode_typed_data(data: &[u8]) -> (TypedData, usize) {
    let type_byte = data[0];
    let data_type = type_byte & 0x0F;
    let flags = type_byte >> 4;
    let mut pos = 1;

    match data_type {
        1 => (TypedData::Bool(flags & 1 != 0), pos),
        2 => {
            let (v, bytes_read) = decode_varint(&data[pos..]);
            pos += bytes_read;
            (TypedData::Int32(v as i32), pos)
        }
        3 => {
            let (v, bytes_read) = decode_varint(&data[pos..]);
            pos += bytes_read;
            (TypedData::UInt32(v as u32), pos)
        }
        4 => {
            let (v, bytes_read) = decode_varint(&data[pos..]);
            pos += bytes_read;
            (TypedData::Int64(v as i64), pos)
        }
        5 => {
            let (v, bytes_read) = decode_varint(&data[pos..]);
            pos += bytes_read;
            (TypedData::UInt64(v), pos)
        }
        6 => {
            let octets: [u8; 4] = data[pos..pos + 4].try_into().unwrap();
            pos += 4;
            (TypedData::IPv4(std::net::Ipv4Addr::from(octets)), pos)
        }
        7 => {
            let octets: [u8; 16] = data[pos..pos + 16].try_into().unwrap();
            pos += 16;
            (TypedData::IPv6(std::net::Ipv6Addr::from(octets)), pos)
        }
        8 => {
            let (s, bytes_read) = decode_spop_string(&data[pos..]);
            pos += bytes_read;
            (TypedData::String(s), pos)
        }
        9 => {
            let (len, bytes_read) = decode_varint(&data[pos..]);
            pos += bytes_read;
            let bytes = data[pos..pos + len as usize].to_vec();
            pos += len as usize;
            (TypedData::Binary(bytes), pos)
        }
        _ => (TypedData::Null, pos),
    }
}

#[derive(Debug)]
pub struct AckResponse {
    pub stream_id: u64,
    pub frame_id: u64,
    pub actions: Vec<AckAction>,
}

impl AckResponse {
    pub fn find_var(&self, name: &str) -> Option<&TypedData> {
        self.actions.iter().find_map(|a| match a {
            AckAction::SetVar { name: n, value, .. } if n.contains(name) => Some(value),
            _ => None,
        })
    }
}

#[derive(Debug)]
pub enum AckAction {
    SetVar {
        scope: &'static str,
        name: String,
        value: TypedData,
    },
    UnSetVar {
        scope: &'static str,
        name: String,
    },
}
