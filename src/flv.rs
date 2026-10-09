//! FLV tags: the container for recordings, and exactly what RTMP carries too,
//! so one muxer feeds both the file and the live stream.

use std::io::{self, Write};

pub const AUDIO: u8 = 8;
pub const VIDEO: u8 = 9;

/// One FLV tag, ready to become a file record or an RTMP message.
pub struct Tag<'a> {
    pub kind: u8,
    pub ms: u32,
    pub body: &'a [u8],
    /// A video keyframe: where a viewer can start decoding.
    pub keyframe: bool,
    /// Decoder config (AVC sequence header or AAC AudioSpecificConfig), needed before any frame.
    pub config: bool,
}

pub trait Sink {
    fn write(&mut self, tag: &Tag) -> io::Result<()>;
    fn finish(&mut self) -> io::Result<()>;
}

/// Turns encoder output into FLV tags and hands each one to every sink.
pub struct Muxer {
    sinks: Vec<Box<dyn Sink>>,
    sps: Vec<u8>,
    pps: Vec<u8>,
    sent_config: bool,
}

impl Muxer {
    pub fn new(sinks: Vec<Box<dyn Sink>>) -> Self {
        Self { sinks, sps: Vec::new(), pps: Vec::new(), sent_config: false }
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

    /// Takes one encoded H.264 frame (Annex-B).
    pub fn video(&mut self, annexb: &[u8], ms: u32, keyframe: bool) -> io::Result<()> {
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
            let config = video_body(true, 0, &self.decoder_config());
            self.emit(&Tag { kind: VIDEO, ms, body: &config, keyframe: true, config: true })?;
            self.sent_config = true;
        }
        if avcc.is_empty() {
            return Ok(());
        }
        let body = video_body(keyframe, 1, &avcc);
        self.emit(&Tag { kind: VIDEO, ms, body: &body, keyframe, config: false })
    }

    /// The AAC decoder config (AudioSpecificConfig); must precede any audio frame.
    pub fn audio_config(&mut self, config: &[u8]) -> io::Result<()> {
        let body = audio_body(0, config);
        self.emit(&Tag { kind: AUDIO, ms: 0, body: &body, keyframe: false, config: true })
    }

    /// Takes one raw AAC frame.
    pub fn audio(&mut self, frame: &[u8], ms: u32) -> io::Result<()> {
        let body = audio_body(1, frame);
        self.emit(&Tag { kind: AUDIO, ms, body: &body, keyframe: false, config: false })
    }

    pub fn finish(&mut self) -> io::Result<()> {
        for sink in &mut self.sinks {
            sink.finish()?;
        }
        Ok(())
    }

    fn emit(&mut self, tag: &Tag) -> io::Result<()> {
        for sink in &mut self.sinks {
            sink.write(tag)?;
        }
        Ok(())
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
}

fn video_body(keyframe: bool, packet_type: u8, payload: &[u8]) -> Vec<u8> {
    let frame_and_codec = if keyframe { 0x17 } else { 0x27 }; // frame type << 4 | 7 (AVC)
    let mut body = Vec::with_capacity(payload.len() + 5);
    // Composition time is 0 because we encode without B-frames.
    body.extend_from_slice(&[frame_and_codec, packet_type, 0, 0, 0]);
    body.extend_from_slice(payload);
    body
}

fn audio_body(packet_type: u8, payload: &[u8]) -> Vec<u8> {
    // 0xAF = AAC, 44 kHz flag (always set for AAC), 16-bit, stereo.
    let mut body = Vec::with_capacity(payload.len() + 2);
    body.extend_from_slice(&[0xAF, packet_type]);
    body.extend_from_slice(payload);
    body
}

/// Writes tags to an .flv file.
pub struct FlvFile<W: Write> {
    out: W,
}

impl<W: Write> FlvFile<W> {
    pub fn new(mut out: W, has_audio: bool) -> io::Result<Self> {
        // "FLV", version 1, flags (4 = audio, 1 = video), header size 9, then PreviousTagSize0.
        let flags = if has_audio { 0x05 } else { 0x01 };
        out.write_all(&[b'F', b'L', b'V', 1, flags, 0, 0, 0, 9, 0, 0, 0, 0])?;
        Ok(Self { out })
    }
}

impl<W: Write> Sink for FlvFile<W> {
    fn write(&mut self, tag: &Tag) -> io::Result<()> {
        let size = tag.body.len() as u32;
        let mut header = [0u8; 11];
        header[0] = tag.kind;
        header[1..4].copy_from_slice(&size.to_be_bytes()[1..]);
        header[4..7].copy_from_slice(&tag.ms.to_be_bytes()[1..]);
        header[7] = (tag.ms >> 24) as u8; // timestamp extension; stream ID stays 0
        self.out.write_all(&header)?;
        self.out.write_all(tag.body)?;
        self.out.write_all(&(size + 11).to_be_bytes())
    }

    fn finish(&mut self) -> io::Result<()> {
        self.out.flush()
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
    use std::cell::RefCell;
    use std::rc::Rc;

    /// (kind, ms, body, keyframe, config)
    type Seen = Rc<RefCell<Vec<(u8, u32, Vec<u8>, bool, bool)>>>;

    struct Collect(Seen);

    impl Sink for Collect {
        fn write(&mut self, tag: &Tag) -> io::Result<()> {
            self.0.borrow_mut().push((tag.kind, tag.ms, tag.body.to_vec(), tag.keyframe, tag.config));
            Ok(())
        }
        fn finish(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn muxer() -> (Muxer, Seen) {
        let seen = Seen::default();
        (Muxer::new(vec![Box::new(Collect(seen.clone()))]), seen)
    }

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let stream = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5];
        assert_eq!(nal_units(&stream), vec![&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4, 5]]);
    }

    #[test]
    fn waits_for_keyframe_then_sends_config_and_frame() {
        let (mut muxer, seen) = muxer();
        muxer.video(&[0, 0, 0, 1, 0x41, 9], 0, false).unwrap(); // P-frame before any keyframe: dropped
        assert!(seen.borrow().is_empty());

        let keyframe = [0, 0, 0, 1, 0x67, 0x64, 0, 0x28, 0, 0, 0, 1, 0x68, 0xEE, 0, 0, 0, 1, 0x65, 0xAA];
        muxer.video(&keyframe, 33, true).unwrap();
        let seen = seen.borrow();
        assert_eq!(seen.len(), 2);
        let (kind, _, config, _, is_config) = &seen[0];
        assert_eq!((*kind, &config[..2], *is_config), (VIDEO, &[0x17, 0][..], true));
        let (_, ms, frame, keyframe, is_config) = &seen[1];
        assert_eq!((*ms, *keyframe, *is_config), (33, true, false));
        assert_eq!(&frame[..], &[0x17, 1, 0, 0, 0, 0, 0, 0, 2, 0x65, 0xAA]);
    }

    #[test]
    fn audio_bodies() {
        let (mut muxer, seen) = muxer();
        muxer.audio_config(&[0x11, 0x90]).unwrap();
        muxer.audio(&[1, 2, 3], 21).unwrap();
        let seen = seen.borrow();
        assert_eq!(seen[0].2, vec![0xAF, 0, 0x11, 0x90]);
        assert!(seen[0].4);
        assert_eq!((seen[1].1, &seen[1].2[..]), (21, &[0xAF, 1, 1, 2, 3][..]));
    }

    #[test]
    fn file_tag_layout() {
        let mut file = FlvFile::new(Vec::new(), true).unwrap();
        file.write(&Tag { kind: AUDIO, ms: 0x01_02_03_04, body: &[7, 8], keyframe: false, config: false }).unwrap();
        let out = file.out;
        assert_eq!(out[4], 0x05); // has audio and video
        let tag = &out[13..];
        assert_eq!(&tag[..4], &[8, 0, 0, 2]); // audio, 2-byte body
        assert_eq!(&tag[4..8], &[0x02, 0x03, 0x04, 0x01]); // ms with extension byte
        assert_eq!(&tag[11..13], &[7, 8]);
        assert_eq!(&tag[13..17], &13u32.to_be_bytes()); // previous tag size
    }
}
