//! Length-delimited messages on inherited anonymous pipes; never exposed on a network port.

use std::io::{self, Read, Write};

use serde::{Serialize, de::DeserializeOwned};

pub(super) const MAX_MESSAGE: usize = 64 * 1024 * 1024;
pub(super) const MAX_DATA: usize = 64 * 1024 * 1024;
pub(super) const RANGE_BYTES: usize = 1024 * 1024;
const MAGIC: &[u8; 4] = b"FCB1";

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Stop serialization at the budget rather than growing an unbounded intermediate Vec.
struct Limited(Vec<u8>);
impl Write for Limited {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_MESSAGE.saturating_sub(self.0.len()) {
            return Err(invalid("backend project/message exceeds the 64 MiB budget"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn encode(message: &impl Serialize, data: &[u8]) -> io::Result<Vec<u8>> {
    if data.len() > MAX_DATA {
        return Err(invalid("backend binary response exceeds the 64 MiB budget"));
    }
    let mut json = Limited(Vec::new());
    serde_json::to_writer(&mut json, message).map_err(|e| invalid(&e.to_string()))?;
    let mut packet = Vec::with_capacity(12 + json.0.len() + data.len());
    packet.extend_from_slice(MAGIC);
    packet.extend_from_slice(&(json.0.len() as u32).to_le_bytes());
    packet.extend_from_slice(&(data.len() as u32).to_le_bytes());
    packet.extend_from_slice(&json.0);
    packet.extend_from_slice(data);
    Ok(packet)
}

pub(super) fn send(writer: &mut impl Write, message: &impl Serialize, data: &[u8]) -> io::Result<()> {
    writer.write_all(&encode(message, data)?)?;
    writer.flush()
}

pub(super) fn read<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<(T, Vec<u8>)> {
    let mut header = [0u8; 12];
    reader.read_exact(&mut header)?;
    if header.get(..4) != Some(MAGIC.as_slice()) {
        return Err(invalid("backend protocol/version mismatch"));
    }
    let json_len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    let data_len = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
    // Validate both before allocation, even for a child returning malformed sizes.
    if json_len == 0 || json_len > MAX_MESSAGE || data_len > MAX_DATA {
        return Err(invalid("backend packet length exceeds the transport budget"));
    }
    let mut json = vec![0; json_len];
    reader.read_exact(&mut json)?;
    let message = serde_json::from_slice(&json).map_err(|e| invalid(&format!("invalid backend message: {e}")))?;
    let mut data = vec![0; data_len];
    reader.read_exact(&mut data)?;
    Ok((message, data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn packets_keep_json_and_binary_separate() {
        let packet = encode(&json!({"id": 7}), b"\0\n\xff").unwrap();
        let (message, data) = read::<Value>(&mut packet.as_slice()).unwrap();
        assert_eq!(message["id"], 7);
        assert_eq!(data, b"\0\n\xff");
    }

    #[test]
    fn hostile_and_truncated_packets_return_errors_without_panicking() {
        let packet = encode(&json!({"ok":true}), b"data").unwrap();
        for length in 0..packet.len() {
            assert!(std::panic::catch_unwind(|| read::<Value>(&mut &packet[..length])).unwrap().is_err());
        }
        for offset in [0, 4, 8] {
            let mut corrupt = packet.clone();
            corrupt[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            assert!(std::panic::catch_unwind(|| read::<Value>(&mut corrupt.as_slice())).unwrap().is_err());
        }
    }
}
