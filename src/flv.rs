//! Minimal FLV writer. FLV tags are also what RTMP carries, so the same code
//! will later feed Twitch/YouTube instead of a file.

use std::io::{self, Write};

pub struct FlvWriter<W: Write> {
    out: W,
    sps: Vec<u8>,
    pps: Vec<u8>,
    sent_config: bool,
}

impl<W: Write> FlvWriter<W> {
    pub fn new(mut out: W, has_audio: bool) -> io::Result<Self> {
        // "FLV", version 1, flags (4 = audio, 1 = video), header size 9, then PreviousTagSize0.
        let flags = if has_audio { 0x05 } else { 0x01 };
        out.write_all(&[b'F', b'L', b'V', 1, flags, 0, 0, 0, 9, 0, 0, 0, 0])?;
        Ok(Self { out, sps: Vec::new(), pps: Vec::new(), sent_config: false })
    }

    /// The AAC decoder config (AudioSpecificConfig); must precede any audio frame.
    pub fn write_audio_config(&mut self, config: &[u8]) -> io::Result<()> {
        self.audio_tag(0, 0, config)
    }

    /// Writes one raw AAC frame.
    pub fn write_audio(&mut self, frame: &[u8], ms: u32) -> io::Result<()> {
        self.audio_tag(ms, 1, frame)
    }

    /// Remembers SPS/PPS given out of band (Annex-B), for encoders that don't repeat them in-stream.
    pub fn set_parameter_sets(&mut self, annexb: &[u8]) {
        for nal in nal_units(annexb) {
            match nal[0] & 0x1f {
                7 => self.sps = nal.to_vec(),
                8 => self.pps = nal.to_vec(),
                _ => {}
            }
        }
    }

    /// Writes one encoded H.264 frame (Annex-B) as an FLV video tag.
    pub fn write_video(&mut self, annexb: &[u8], ms: u32, keyframe: bool) -> io::Result<()> {
        let mut keyframe = keyframe;
        let mut avcc = Vec::with_capacity(annexb.len() + 16);
        for nal in nal_units(annexb) {
            match nal[0] & 0x1f {
                7 if self.sps != nal => (self.sps, self.sent_config) = (nal.to_vec(), false),
                8 if self.pps != nal => (self.pps, self.sent_config) = (nal.to_vec(), false),
                7 | 8 | 9 => {} // unchanged parameter sets, access unit delimiters
                kind => {
                    keyframe |= kind == 5;
                    avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    avcc.extend_from_slice(nal);
                }
            }
        }
        if !self.sent_config {
            // Nothing is decodable before the decoder config, and that starts at a keyframe.
            if self.sps.is_empty() || self.pps.is_empty() || !keyframe {
                return Ok(());
            }
            let config = self.decoder_config();
            self.video_tag(ms, true, 0, &config)?;
            self.sent_config = true;
        }
        if avcc.is_empty() {
            return Ok(());
        }
        self.video_tag(ms, keyframe, 1, &avcc)
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.out.flush()?;
        Ok(self.out)
    }

    /// AVCDecoderConfigurationRecord (ISO 14496-15).
    fn decoder_config(&self) -> Vec<u8> {
        let mut config = vec![1, self.sps[1], self.sps[2], self.sps[3], 0xFF, 0xE1];
        config.extend_from_slice(&(self.sps.len() as u16).to_be_bytes());
        config.extend_from_slice(&self.sps);
        config.push(1);
        config.extend_from_slice(&(self.pps.len() as u16).to_be_bytes());
        config.extend_from_slice(&self.pps);
        config
    }

    fn video_tag(&mut self, ms: u32, keyframe: bool, packet_type: u8, payload: &[u8]) -> io::Result<()> {
        let frame_and_codec = if keyframe { 0x17 } else { 0x27 }; // frame type << 4 | 7 (AVC)
        let mut body = Vec::with_capacity(payload.len() + 5);
        // Composition time is 0 because we encode without B-frames.
        body.extend_from_slice(&[frame_and_codec, packet_type, 0, 0, 0]);
        body.extend_from_slice(payload);
        self.tag(9, ms, &body)
    }

    fn audio_tag(&mut self, ms: u32, packet_type: u8, payload: &[u8]) -> io::Result<()> {
        // 0xAF = AAC, 44 kHz flag (always set for AAC), 16-bit, stereo.
        let mut body = Vec::with_capacity(payload.len() + 2);
        body.extend_from_slice(&[0xAF, packet_type]);
        body.extend_from_slice(payload);
        self.tag(8, ms, &body)
    }

    fn tag(&mut self, kind: u8, ms: u32, body: &[u8]) -> io::Result<()> {
        let size = body.len() as u32;
        let mut header = [0u8; 11];
        header[0] = kind;
        header[1..4].copy_from_slice(&size.to_be_bytes()[1..]);
        header[4..7].copy_from_slice(&ms.to_be_bytes()[1..]);
        header[7] = (ms >> 24) as u8; // timestamp extension; stream ID stays 0
        self.out.write_all(&header)?;
        self.out.write_all(body)?;
        self.out.write_all(&(size + 11).to_be_bytes())
    }
}

/// Splits an Annex-B stream on 00 00 01 / 00 00 00 01 start codes.
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut start = None;
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            if let Some(s) = start {
                units.push(trim_trailing_zeros(&data[s..i]));
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        units.push(&data[s..]);
    }
    units.retain(|unit| !unit.is_empty());
    units
}

fn trim_trailing_zeros(mut nal: &[u8]) -> &[u8] {
    while let [rest @ .., 0] = nal {
        nal = rest;
    }
    nal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let stream = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5];
        assert_eq!(nal_units(&stream), vec![&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4, 5]]);
    }

    #[test]
    fn waits_for_keyframe_then_writes_config_and_frame() {
        let mut flv = FlvWriter::new(Vec::new(), false).unwrap();
        let header_len = 13;
        // A P-frame before any keyframe is dropped.
        flv.write_video(&[0, 0, 0, 1, 0x41, 9], 0, false).unwrap();
        assert_eq!(flv.out.len(), header_len);

        let keyframe = [0, 0, 0, 1, 0x67, 0x64, 0, 0x28, 0, 0, 0, 1, 0x68, 0xEE, 0, 0, 0, 1, 0x65, 0xAA];
        flv.write_video(&keyframe, 33, true).unwrap();
        let out = flv.finish().unwrap();

        // First tag: sequence header (AVCPacketType 0).
        assert_eq!(out[header_len], 9);
        assert_eq!(&out[header_len + 11..header_len + 13], &[0x17, 0]);
        // Second tag: the IDR frame as a 4-byte length-prefixed NAL.
        let config_len = u32::from_be_bytes([0, out[header_len + 1], out[header_len + 2], out[header_len + 3]]) as usize;
        let second = header_len + 11 + config_len + 4;
        assert_eq!(&out[second + 11..second + 13], &[0x17, 1]);
        assert_eq!(&out[second + 16..second + 22], &[0, 0, 0, 2, 0x65, 0xAA]);
    }

    #[test]
    fn writes_audio_tags() {
        let mut flv = FlvWriter::new(Vec::new(), true).unwrap();
        flv.write_audio_config(&[0x11, 0x90]).unwrap();
        flv.write_audio(&[1, 2, 3], 0x01_02_03_04).unwrap();
        let out = flv.finish().unwrap();
        assert_eq!(out[4], 0x05); // has audio and video

        let first = 13;
        assert_eq!(&out[first..first + 4], &[8, 0, 0, 4]); // audio tag, 4-byte body
        assert_eq!(&out[first + 11..first + 15], &[0xAF, 0, 0x11, 0x90]);

        let second = first + 11 + 4 + 4;
        assert_eq!(&out[second + 4..second + 8], &[0x02, 0x03, 0x04, 0x01]); // ms with extension byte
        assert_eq!(&out[second + 11..second + 16], &[0xAF, 1, 1, 2, 3]);
    }
}
