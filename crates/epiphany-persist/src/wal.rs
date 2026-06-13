//! The write-ahead log: a binary, append-only record of cell writes (numeric and string).
//!
//! Each record is framed as `[len u32][payload][crc u32]` (little-endian), so a
//! write torn by a crash is detected on recovery (the length runs past the file
//! end, or the CRC32 fails) and the torn tail is discarded. The file opens with
//! an 8-byte header (magic + version). See ADR-0002.

use epiphany_core::Fixed;

/// Magic bytes prefixing every WAL file.
pub(crate) const WAL_MAGIC: [u8; 6] = *b"EPIWAL";
/// On-disk WAL format version.
pub(crate) const WAL_VERSION: u16 = 1;
/// Length of the fixed file header (magic + little-endian version).
pub(crate) const WAL_HEADER_LEN: u64 = 8;

const OP_SET_LEAF: u8 = 1;
const OP_SET_STRING: u8 = 2;

/// A decoded WAL record. Phase 1 logs cell writes (numeric and string);
/// structural changes are captured by a checkpoint (a fresh snapshot), not the
/// log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Record {
    SetLeaf { coord: Vec<u32>, value: Fixed },
    SetString { coord: Vec<u32>, value: String },
}

/// Why a WAL file could not be read.
#[derive(Debug)]
pub(crate) enum WalError {
    /// The file is shorter than the header, or the magic/version is wrong.
    BadHeader,
}

impl std::fmt::Display for WalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalError::BadHeader => write!(f, "WAL header missing or unrecognized"),
        }
    }
}

/// The result of scanning a WAL body: the intact records, and the byte length of
/// the intact framing (including the header). Bytes beyond `good_len` are a torn
/// trailing write and must be truncated before appending again.
#[derive(Debug)]
pub(crate) struct Replay {
    pub records: Vec<Record>,
    pub good_len: u64,
}

/// The 8-byte file header: magic then little-endian version.
pub(crate) fn header() -> [u8; 8] {
    let mut h = [0u8; 8];
    h[..6].copy_from_slice(&WAL_MAGIC);
    h[6..].copy_from_slice(&WAL_VERSION.to_le_bytes());
    h
}

/// Encode a record framed as `[len u32][payload][crc u32]` (little-endian).
pub(crate) fn encode(record: &Record) -> Vec<u8> {
    let mut payload = Vec::new();
    match record {
        Record::SetLeaf { coord, value } => {
            payload.push(OP_SET_LEAF);
            write_coord(&mut payload, coord);
            payload.extend_from_slice(&value.to_scaled().to_le_bytes());
        }
        Record::SetString { coord, value } => {
            payload.push(OP_SET_STRING);
            write_coord(&mut payload, coord);
            let bytes = value.as_bytes();
            payload.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            payload.extend_from_slice(bytes);
        }
    }
    let mut framed = Vec::with_capacity(payload.len() + 8);
    framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    framed.extend_from_slice(&payload);
    framed.extend_from_slice(&crc32(&payload).to_le_bytes());
    framed
}

/// Scan a WAL file (the whole file, header included), stopping at the first torn
/// or corrupt record. An append-only log only ever tears at the tail, so the
/// records before the break are the durable, acknowledged writes.
pub(crate) fn replay(bytes: &[u8]) -> Result<Replay, WalError> {
    if bytes.len() < WAL_HEADER_LEN as usize
        || bytes[..6] != WAL_MAGIC
        || u16::from_le_bytes([bytes[6], bytes[7]]) != WAL_VERSION
    {
        return Err(WalError::BadHeader);
    }

    let mut records = Vec::new();
    let mut pos = WAL_HEADER_LEN as usize;
    let mut good = WAL_HEADER_LEN;
    while pos + 4 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let frame_end = pos + 4 + len + 4;
        if frame_end > bytes.len() {
            break; // torn: payload + crc run past the file end
        }
        let payload = &bytes[pos + 4..pos + 4 + len];
        let crc = u32::from_le_bytes(bytes[pos + 4 + len..frame_end].try_into().unwrap());
        if crc32(payload) != crc {
            break; // torn or corrupt trailing record
        }
        match decode(payload) {
            Some(record) => records.push(record),
            None => break, // unknown op: treat as a corrupt tail
        }
        pos = frame_end;
        good = frame_end as u64;
    }
    Ok(Replay {
        records,
        good_len: good,
    })
}

/// Append a coordinate as `[rank u16][rank x u32]` (little-endian).
fn write_coord(payload: &mut Vec<u8>, coord: &[u32]) {
    payload.extend_from_slice(&(coord.len() as u16).to_le_bytes());
    for &idx in coord {
        payload.extend_from_slice(&idx.to_le_bytes());
    }
}

/// Read a coordinate from the front of `rest`, returning it and the bytes consumed.
fn read_coord(rest: &[u8]) -> Option<(Vec<u32>, usize)> {
    let rank = u16::from_le_bytes([*rest.first()?, *rest.get(1)?]) as usize;
    let mut pos = 2;
    let mut coord = Vec::with_capacity(rank);
    for _ in 0..rank {
        let end = pos + 4;
        coord.push(u32::from_le_bytes(rest.get(pos..end)?.try_into().unwrap()));
        pos = end;
    }
    Some((coord, pos))
}

fn decode(payload: &[u8]) -> Option<Record> {
    let (&op, rest) = payload.split_first()?;
    match op {
        OP_SET_LEAF => {
            let (coord, pos) = read_coord(rest)?;
            let scaled = i64::from_le_bytes(rest.get(pos..pos + 8)?.try_into().unwrap());
            Some(Record::SetLeaf {
                coord,
                value: Fixed::from_scaled(scaled),
            })
        }
        OP_SET_STRING => {
            let (coord, mut pos) = read_coord(rest)?;
            let len = u32::from_le_bytes(rest.get(pos..pos + 4)?.try_into().unwrap()) as usize;
            pos += 4;
            let value = std::str::from_utf8(rest.get(pos..pos + len)?)
                .ok()?
                .to_string();
            Some(Record::SetString { coord, value })
        }
        _ => None,
    }
}

/// CRC32 (IEEE 802.3, polynomial 0xEDB88320), table generated at compile time so
/// there is no runtime initialization and no external dependency.
const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static CRC32_TABLE: [u32; 256] = crc32_table();

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc = (crc >> 8) ^ CRC32_TABLE[((crc ^ u32::from(b)) & 0xFF) as usize];
    }
    crc ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trips_through_encode_replay() {
        let mut bytes = header().to_vec();
        let records = vec![
            Record::SetLeaf {
                coord: vec![0, 3, 7],
                value: Fixed::from(42),
            },
            Record::SetString {
                coord: vec![1, 0, 2],
                value: "hello".to_string(),
            },
            Record::SetLeaf {
                coord: vec![1, 0, 2],
                value: Fixed::from(-5),
            },
        ];
        for record in &records {
            bytes.extend_from_slice(&encode(record));
        }
        let replay = replay(&bytes).unwrap();
        assert_eq!(replay.records, records);
        assert_eq!(replay.good_len as usize, bytes.len());
    }

    #[test]
    fn torn_trailing_record_stops_at_last_good() {
        let mut bytes = header().to_vec();
        let good = Record::SetLeaf {
            coord: vec![2],
            value: Fixed::from(9),
        };
        bytes.extend_from_slice(&encode(&good));
        let good_len = bytes.len();
        // A half-written next record: a length prefix with a truncated body.
        bytes.extend_from_slice(&7u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 0]); // only 2 of the promised 7 payload bytes
        let replay = replay(&bytes).unwrap();
        assert_eq!(replay.records, vec![good]);
        assert_eq!(replay.good_len as usize, good_len);
    }

    #[test]
    fn corrupt_crc_is_treated_as_torn() {
        let mut bytes = header().to_vec();
        let mut framed = encode(&Record::SetLeaf {
            coord: vec![0],
            value: Fixed::from(1),
        });
        let last = framed.len() - 1;
        framed[last] ^= 0xFF; // flip a CRC byte
        bytes.extend_from_slice(&framed);
        let replay = replay(&bytes).unwrap();
        assert!(replay.records.is_empty());
        assert_eq!(replay.good_len, WAL_HEADER_LEN);
    }

    #[test]
    fn bad_header_is_rejected() {
        assert!(matches!(replay(b"nope").unwrap_err(), WalError::BadHeader));
    }
}
