// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Battery-BMS vendor frames. The vendor firmware strings show the BMS uses
// XOR-checked frames distinct from Modbus CRC16 ("parse battery type,
// failed to XorCheck"). The exact frame layout is NOT yet captured — this
// module implements the checksum and a conservative stream extractor so
// that, once a live capture confirms the layout, only the field offsets
// here need to change. Until then, BMS data is polled via plain Modbus
// (see poll.rs) and this parser is exercised by unit tests only.

/// XOR of all bytes — the vendor "XorCheck" over a frame body.
pub fn xor_check(body: &[u8]) -> u8 {
    body.iter().fold(0u8, |acc, &b| acc ^ b)
}

/// Verify a frame whose last byte is the XOR of the preceding body.
pub fn verify_frame(frame: &[u8]) -> bool {
    frame.len() >= 2 && xor_check(&frame[..frame.len() - 1]) == frame[frame.len() - 1]
}

/// A candidate XOR-framed BMS packet extracted from a byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BmsFrame {
    pub body: Vec<u8>,
}

/// Extract complete XOR-checked frames from a stream buffer. Frame
/// boundaries are currently assumed to be `<len:1><body:len><xor:1>` with
/// len covering body+xor — TODO(validate) against a live BMS capture and
/// adjust. Unknown bytes are skipped, so a wrong guess degrades to "no
/// frames", never to garbage data.
pub fn extract_frames(buf: &[u8]) -> (Vec<BmsFrame>, usize) {
    let mut frames = Vec::new();
    let mut i = 0usize;
    let mut consumed = 0usize;
    while i + 2 <= buf.len() {
        let len = buf[i] as usize;
        if len < 2 || len > 128 {
            i += 1; // implausible length: garbage byte, resync
            consumed = i;
            continue;
        }
        if i + 1 + len > buf.len() {
            break; // plausible frame, wait for more data
        }
        let frame = &buf[i + 1..i + 1 + len];
        if verify_frame(frame) {
            frames.push(BmsFrame {
                body: frame[..frame.len() - 1].to_vec(),
            });
            i += 1 + len;
            consumed = i;
        } else {
            i += 1; // resync: skip one byte
            consumed = i;
        }
    }
    (frames, consumed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xor_roundtrip() {
        let body = [0x01u8, 0x02, 0x03];
        let x = xor_check(&body);
        let mut frame = body.to_vec();
        frame.push(x);
        assert!(verify_frame(&frame));
        frame[1] ^= 0xFF;
        assert!(!verify_frame(&frame));
    }

    #[test]
    fn extract_and_resync() {
        let body = [0x10u8, 0x20, 0x30];
        let mut stream = vec![0xAA, 0xBB]; // garbage to resync past
        let mut frame = body.to_vec();
        frame.push(xor_check(&body));
        stream.push(frame.len() as u8);
        stream.extend_from_slice(&frame);
        stream.push(0x05); // truncated frame header: kept for next round
        let (frames, consumed) = extract_frames(&stream);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].body, body);
        assert_eq!(consumed, stream.len() - 1);
    }
}
