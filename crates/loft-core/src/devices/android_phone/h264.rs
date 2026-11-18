//! Cutting `screenrecord`'s H.264 (an Annex B byte stream: NAL units after
//! `00 00 01` start codes) into what a decoder takes: the parameter sets,
//! and one access unit (a whole picture's slices) at a time.
//!
//! A unit's end shows only when the next start code arrives, which for the
//! last picture before the screen goes still would be never: the reader
//! calls [`Splitter::flush`] once the stream has paused for a moment
//! (`screenrecord` writes each encoded picture in one go). Should a picture
//! still come in two parts with a pause between, the second part shows it:
//! [`Splitter::take_broken`] says so, and the caller starts afresh from a
//! new keyframe.

/// What the decoder gets, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Unit {
    /// New sequence and picture parameter sets (before the first picture,
    /// and again whenever they change).
    Params { sps: Vec<u8>, pps: Vec<u8> },
    /// One picture: its slice NAL units (without start codes); `key` for an
    /// IDR picture, which needs nothing before it.
    Frame { nals: Vec<Vec<u8>>, key: bool },
}

#[derive(Debug, Default)]
pub struct Splitter {
    buf: Vec<u8>,
    /// Where the NAL unit being read starts in `buf`.
    start: Option<usize>,
    /// Scanning for start codes goes on from here.
    scanned: usize,
    /// The slices of the picture being gathered.
    au: Vec<Vec<u8>>,
    key: bool,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    /// The parameter sets last given out.
    said: Option<(Vec<u8>, Vec<u8>)>,
    /// The last flush ended a NAL unit that more bytes may yet continue.
    flushed_tail: bool,
    /// A picture was given out before all of it had come.
    broken: bool,
}

/// Whether `data`, coming after a pause, continues the NAL unit before it
/// (`Some(true)`), starts a new one (`Some(false)`) or can't tell yet (only
/// zeros: the beginning of a start code).
pub fn continues_unit(data: &[u8]) -> Option<bool> {
    // A start code: zeros, then 1 (at least two zeros before it).
    data.iter()
        .position(|b| *b != 0)
        .map(|i| !(i >= 2 && data[i] == 1))
}

fn trim_zeros(mut nal: &[u8]) -> &[u8] {
    while let [rest @ .., 0] = nal {
        nal = rest;
    }
    nal
}

impl Splitter {
    /// Bytes of the stream; returns the units now complete.
    pub fn push(&mut self, data: &[u8]) -> Vec<Unit> {
        let mut out = Vec::new();
        if self.flushed_tail {
            let mut seen = self.buf.clone();
            seen.extend_from_slice(data);
            if let Some(continues) = continues_unit(&seen) {
                self.flushed_tail = false;
                self.broken |= continues;
            }
        }
        self.buf.extend_from_slice(data);
        let mut i = self.scanned;
        while i + 3 <= self.buf.len() {
            if self.buf[i] == 0 && self.buf[i + 1] == 0 && self.buf[i + 2] == 1 {
                if let Some(s) = self.start {
                    let nal = trim_zeros(&self.buf[s..i]).to_vec();
                    self.nal(nal, &mut out);
                }
                self.start = Some(i + 3);
                i += 3;
            } else {
                i += 1;
            }
        }
        self.scanned = i;
        // Keep only the unit being read (or the last bytes, which may begin
        // a start code).
        let cut = self.start.unwrap_or(self.scanned).min(self.scanned);
        if cut > 0 {
            self.buf.drain(..cut);
            self.start = self.start.map(|s| s - cut);
            self.scanned -= cut;
        }
        out
    }

    /// What is waiting for the flush belongs to a keyframe (large, so
    /// likelier to come in parts).
    pub fn pending_key(&self) -> bool {
        self.key
            || self
                .start
                .and_then(|s| self.buf.get(s))
                .is_some_and(|h| matches!(h & 0x1f, 5 | 7 | 8))
    }

    /// A picture was given out incomplete since this was last asked.
    pub fn take_broken(&mut self) -> bool {
        std::mem::take(&mut self.broken)
    }

    /// The stream paused: what was read last is complete.
    pub fn flush(&mut self) -> Vec<Unit> {
        let mut out = Vec::new();
        if let Some(s) = self.start.take() {
            let nal = trim_zeros(&self.buf[s..]).to_vec();
            self.flushed_tail = !nal.is_empty();
            self.nal(nal, &mut out);
        }
        self.buf.clear();
        self.scanned = 0;
        self.end_picture(&mut out);
        out
    }

    fn end_picture(&mut self, out: &mut Vec<Unit>) {
        if !self.au.is_empty() {
            out.push(Unit::Frame {
                nals: std::mem::take(&mut self.au),
                key: std::mem::take(&mut self.key),
            });
        }
    }

    fn nal(&mut self, nal: Vec<u8>, out: &mut Vec<Unit>) {
        let Some(&header) = nal.first() else { return };
        match header & 0x1f {
            // A slice: the first of a picture (first_mb_in_slice = 0, a
            // single 1 bit) ends the picture before it.
            kind @ (1 | 5) => {
                let first = nal.get(1).is_some_and(|b| b & 0x80 != 0);
                if first {
                    self.end_picture(out);
                }
                self.key |= kind == 5;
                self.au.push(nal);
            }
            7 => {
                self.end_picture(out);
                self.sps = Some(nal);
            }
            8 => {
                self.end_picture(out);
                self.pps = Some(nal);
                if let (Some(sps), Some(pps)) = (&self.sps, &self.pps)
                    && self.said.as_ref() != Some(&(sps.clone(), pps.clone()))
                {
                    self.said = Some((sps.clone(), pps.clone()));
                    out.push(Unit::Params {
                        sps: sps.clone(),
                        pps: pps.clone(),
                    });
                }
            }
            // An access unit delimiter or SEI starts the next picture.
            6 | 9 => self.end_picture(out),
            _ => {}
        }
    }
}
