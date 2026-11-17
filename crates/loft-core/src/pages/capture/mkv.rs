//! The Matroska file a recording writes: MJPEG frames, as they come, in
//! the container the Matroska specification describes over EBML (RFC
//! 8794).
//!
//! Layout: the EBML header, then one Segment holding Info (millisecond
//! timestamps, the duration), Tracks (one video track, `V_MJPEG`) and
//! Clusters, each frame a BlockGroup with its own BlockDuration so the last
//! frame's length counts. While recording, the Segment's size is
//! "unknown", so a file cut off part-way still plays; finishing fills in
//! the size and the duration.

use std::io::{Seek, SeekFrom, Write};

const EBML: u32 = 0x1A45_DFA3;
const EBML_VERSION: u32 = 0x4286;
const EBML_READ_VERSION: u32 = 0x42F7;
const EBML_MAX_ID_LENGTH: u32 = 0x42F2;
const EBML_MAX_SIZE_LENGTH: u32 = 0x42F3;
const DOC_TYPE: u32 = 0x4282;
const DOC_TYPE_VERSION: u32 = 0x4287;
const DOC_TYPE_READ_VERSION: u32 = 0x4285;
const SEGMENT: u32 = 0x1853_8067;
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
const DURATION: u32 = 0x4489;
const MUXING_APP: u32 = 0x4D80;
const WRITING_APP: u32 = 0x5741;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_UID: u32 = 0x73C5;
const TRACK_TYPE: u32 = 0x83;
const FLAG_LACING: u32 = 0x9C;
const CODEC_ID: u32 = 0x86;
const VIDEO: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const CLUSTER: u32 = 0x1F43_B675;
const TIMESTAMP: u32 = 0xE7;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;
const BLOCK_DURATION: u32 = 0x9B;

/// A cluster is written once it holds this much, or spans this long.
const CLUSTER_BYTES: usize = 8 * 1024 * 1024;
const CLUSTER_SPAN_MS: u64 = 5_000;

const APP: &str = "agentloft";

/// An element ID as it is written: its bytes, length marker included.
fn put_id(out: &mut Vec<u8>, id: u32) {
    let bytes = id.to_be_bytes();
    let skip = bytes.iter().take_while(|b| **b == 0).count().min(3);
    out.extend_from_slice(&bytes[skip..]);
}

/// A size in its shortest form (all ones at a length means "unknown", so
/// a value that would be all ones takes one more byte).
pub(super) fn size(value: u64) -> Vec<u8> {
    let length = (1..=8u32)
        .find(|n| value < (1u64 << (7 * n)) - 1)
        .unwrap_or(8);
    let marked = value | (1u64 << (7 * length));
    marked.to_be_bytes()[(8 - length as usize)..].to_vec()
}

/// A size written in eight bytes, to be patched later.
fn size8(value: u64) -> [u8; 8] {
    (value | (1u64 << 56)).to_be_bytes()
}

/// The "unknown" size, in eight bytes.
const UNKNOWN_SIZE: [u8; 8] = [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

fn element(out: &mut Vec<u8>, id: u32, body: &[u8]) {
    put_id(out, id);
    out.extend(size(body.len() as u64));
    out.extend_from_slice(body);
}

fn uint(out: &mut Vec<u8>, id: u32, value: u64) {
    let bytes = value.to_be_bytes();
    let skip = bytes.iter().take_while(|b| **b == 0).count().min(7);
    element(out, id, &bytes[skip..]);
}

fn text(out: &mut Vec<u8>, id: u32, value: &str) {
    element(out, id, value.as_bytes());
}

fn master(out: &mut Vec<u8>, id: u32, fill: impl FnOnce(&mut Vec<u8>)) {
    let mut body = Vec::new();
    fill(&mut body);
    element(out, id, &body);
}

/// Writes frames to a Matroska file as they come.
pub(super) struct Writer<W: Write + Seek> {
    out: W,
    /// Where the Segment's size is, and where its content begins.
    segment_size_at: u64,
    segment_start: u64,
    /// Where the Info's Duration's eight bytes are.
    duration_at: u64,
    /// The cluster being filled: its timestamp and its blocks.
    cluster: Option<(u64, Vec<u8>)>,
    /// The end of the last frame, in milliseconds.
    end_ms: u64,
    frames: u64,
}

impl<W: Write + Seek> Writer<W> {
    /// Begin a file of `width`×`height` MJPEG frames.
    pub(super) fn new(mut out: W, width: u32, height: u32) -> std::io::Result<Self> {
        let mut head = Vec::new();
        master(&mut head, EBML, |h| {
            uint(h, EBML_VERSION, 1);
            uint(h, EBML_READ_VERSION, 1);
            uint(h, EBML_MAX_ID_LENGTH, 4);
            uint(h, EBML_MAX_SIZE_LENGTH, 8);
            text(h, DOC_TYPE, "matroska");
            uint(h, DOC_TYPE_VERSION, 4);
            uint(h, DOC_TYPE_READ_VERSION, 2);
        });
        let start = out.stream_position()?;
        put_id(&mut head, SEGMENT);
        let segment_size_at = start + head.len() as u64;
        head.extend(UNKNOWN_SIZE);
        let segment_start = start + head.len() as u64;
        // Info, with room for the duration (filled in at the end).
        let mut info = Vec::new();
        uint(&mut info, TIMESTAMP_SCALE, 1_000_000);
        put_id(&mut info, DURATION);
        info.extend(size(8));
        let duration_in_info = info.len();
        info.extend(0f64.to_be_bytes());
        text(&mut info, MUXING_APP, APP);
        text(&mut info, WRITING_APP, APP);
        put_id(&mut head, INFO);
        head.extend(size(info.len() as u64));
        let duration_at = start + (head.len() + duration_in_info) as u64;
        head.extend(info);
        master(&mut head, TRACKS, |t| {
            master(t, TRACK_ENTRY, |e| {
                uint(e, TRACK_NUMBER, 1);
                uint(e, TRACK_UID, track_uid());
                uint(e, TRACK_TYPE, 1);
                uint(e, FLAG_LACING, 0);
                text(e, CODEC_ID, "V_MJPEG");
                master(e, VIDEO, |v| {
                    uint(v, PIXEL_WIDTH, u64::from(width));
                    uint(v, PIXEL_HEIGHT, u64::from(height));
                });
            });
        });
        out.write_all(&head)?;
        Ok(Self {
            out,
            segment_size_at,
            segment_start,
            duration_at,
            cluster: None,
            end_ms: 0,
            frames: 0,
        })
    }

    /// One frame: a JPEG shown from `at_ms` for `duration_ms`.
    pub(super) fn frame(
        &mut self,
        jpeg: &[u8],
        at_ms: u64,
        duration_ms: u64,
    ) -> std::io::Result<()> {
        let fits = |(start, blocks): &(u64, Vec<u8>)| {
            at_ms >= *start
                && at_ms - start < CLUSTER_SPAN_MS
                && blocks.len() + jpeg.len() < CLUSTER_BYTES
        };
        if self.cluster.as_ref().is_some_and(|c| !fits(c)) {
            self.flush_cluster()?;
        }
        let (start, blocks) = self.cluster.get_or_insert_with(|| (at_ms, Vec::new()));
        let relative = (at_ms - *start) as i16;
        master(blocks, BLOCK_GROUP, |group| {
            put_id(group, BLOCK);
            group.extend(size(4 + jpeg.len() as u64));
            group.push(0x81); // track 1
            group.extend(relative.to_be_bytes());
            group.push(0); // no flags, no lacing
            group.extend_from_slice(jpeg);
            uint(group, BLOCK_DURATION, duration_ms);
        });
        self.end_ms = self.end_ms.max(at_ms + duration_ms);
        self.frames += 1;
        Ok(())
    }

    /// Frames written so far.
    pub(super) fn frames(&self) -> u64 {
        self.frames
    }

    fn flush_cluster(&mut self) -> std::io::Result<()> {
        let Some((start, blocks)) = self.cluster.take() else {
            return Ok(());
        };
        let mut body = Vec::with_capacity(blocks.len() + 16);
        uint(&mut body, TIMESTAMP, start);
        body.extend(blocks);
        let mut cluster = Vec::with_capacity(body.len() + 12);
        element(&mut cluster, CLUSTER, &body);
        self.out.write_all(&cluster)
    }

    /// Write what is left, then the Segment's size and the duration.
    pub(super) fn finish(mut self) -> std::io::Result<W> {
        self.flush_cluster()?;
        let end = self.out.stream_position()?;
        self.out.seek(SeekFrom::Start(self.segment_size_at))?;
        self.out.write_all(&size8(end - self.segment_start))?;
        self.out.seek(SeekFrom::Start(self.duration_at))?;
        self.out.write_all(&(self.end_ms as f64).to_be_bytes())?;
        self.out.seek(SeekFrom::Start(end))?;
        self.out.flush()?;
        Ok(self.out)
    }
}

/// A track UID: any nonzero number, different between files.
fn track_uid() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    (nanos ^ (u64::from(std::process::id()) << 32)) | 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn sizes_take_the_shortest_form() {
        assert_eq!(size(0), [0x80]);
        assert_eq!(size(126), [0xFE]);
        assert_eq!(size(127), [0x40, 0x7F]);
        assert_eq!(size(300), [0x41, 0x2C]);
        assert_eq!(size(16_382), [0x7F, 0xFE]);
        assert_eq!(size(16_383), [0x20, 0x3F, 0xFF]);
    }

    /// One element read back: its ID, where its body is, and its size.
    struct Read {
        id: u32,
        body: std::ops::Range<usize>,
    }

    fn vint(bytes: &[u8], at: usize, keep_marker: bool) -> (u64, usize) {
        let first = bytes[at];
        let length = first.leading_zeros() as usize + 1;
        assert!(length <= 8, "a bad length at {at}");
        let mut value = if keep_marker {
            u64::from(first)
        } else {
            u64::from(first) & (0xFF >> length)
        };
        for b in &bytes[at + 1..at + length] {
            value = (value << 8) | u64::from(*b);
        }
        (value, length)
    }

    fn read(bytes: &[u8], at: usize) -> Read {
        let (id, id_len) = vint(bytes, at, true);
        let (size, size_len) = vint(bytes, at + id_len, false);
        let start = at + id_len + size_len;
        Read {
            id: id as u32,
            body: start..start + size as usize,
        }
    }

    fn children(bytes: &[u8], range: std::ops::Range<usize>) -> Vec<Read> {
        let mut out = Vec::new();
        let mut at = range.start;
        while at < range.end {
            let child = read(bytes, at);
            assert!(child.body.end <= range.end, "a child runs past its parent");
            at = child.body.end;
            out.push(child);
        }
        assert_eq!(at, range.end);
        out
    }

    fn find(bytes: &[u8], range: std::ops::Range<usize>, id: u32) -> Vec<Read> {
        children(bytes, range)
            .into_iter()
            .filter(|c| c.id == id)
            .collect()
    }

    fn number(bytes: &[u8], body: &std::ops::Range<usize>) -> u64 {
        bytes[body.clone()]
            .iter()
            .fold(0, |n, b| (n << 8) | u64::from(*b))
    }

    #[test]
    fn the_file_is_well_formed() {
        let frames: [(&[u8], u64, u64); 2] =
            [(b"\xFF\xD8first", 0, 33), (b"\xFF\xD8second", 33, 300)];
        let mut writer = Writer::new(Cursor::new(Vec::new()), 64, 48).unwrap();
        for (jpeg, at, duration) in frames {
            writer.frame(jpeg, at, duration).unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();
        assert_eq!(&bytes[..4], &[0x1A, 0x45, 0xDF, 0xA3]);

        let top = children(&bytes, 0..bytes.len());
        assert_eq!(top.len(), 2, "the EBML header, then one Segment");
        assert_eq!(top[0].id, EBML);
        let doc_type = &find(&bytes, top[0].body.clone(), DOC_TYPE)[0];
        assert_eq!(&bytes[doc_type.body.clone()], b"matroska");
        let segment = &top[1];
        assert_eq!(segment.id, SEGMENT);
        assert_eq!(segment.body.end, bytes.len(), "its size reaches the end");

        let info = &find(&bytes, segment.body.clone(), INFO)[0];
        let duration = &find(&bytes, info.body.clone(), DURATION)[0];
        let duration = f64::from_be_bytes(bytes[duration.body.clone()].try_into().unwrap());
        assert_eq!(duration, 333.0);
        let scale = &find(&bytes, info.body.clone(), TIMESTAMP_SCALE)[0];
        assert_eq!(number(&bytes, &scale.body), 1_000_000);

        let mut found = Vec::new();
        for cluster in find(&bytes, segment.body.clone(), CLUSTER) {
            let start = number(
                &bytes,
                &find(&bytes, cluster.body.clone(), TIMESTAMP)[0].body,
            );
            for group in find(&bytes, cluster.body.clone(), BLOCK_GROUP) {
                let block = &find(&bytes, group.body.clone(), BLOCK)[0];
                let data = &bytes[block.body.clone()];
                assert_eq!(data[0], 0x81, "track 1");
                let relative = i16::from_be_bytes([data[1], data[2]]);
                let duration = number(
                    &bytes,
                    &find(&bytes, group.body.clone(), BLOCK_DURATION)[0].body,
                );
                found.push((
                    data[4..].to_vec(),
                    (start as i64 + i64::from(relative)) as u64,
                    duration,
                ));
            }
        }
        let expected: Vec<_> = frames
            .iter()
            .map(|(jpeg, at, duration)| (jpeg.to_vec(), *at, *duration))
            .collect();
        assert_eq!(found, expected);
    }

    fn jpeg(width: u32, height: u32, shade: u8) -> Vec<u8> {
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85)
            .encode_image(&image::RgbImage::from_pixel(
                width,
                height,
                image::Rgb([shade, 90, 200]),
            ))
            .unwrap();
        out
    }

    #[test]
    fn ffprobe_reads_it() {
        let Ok(probe) = std::process::Command::new("ffprobe")
            .arg("-version")
            .output()
        else {
            eprintln!("no ffprobe here: skipped");
            return;
        };
        assert!(probe.status.success());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("three.mkv");
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = Writer::new(file, 64, 48).unwrap();
        for n in 0..3u64 {
            writer
                .frame(&jpeg(64, 48, (n * 80) as u8), n * 500, 500)
                .unwrap();
        }
        writer.finish().unwrap().sync_all().unwrap();
        let out = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-count_frames", "-show_entries"])
            .args(["stream=codec_name,width,height,nb_read_frames:format=duration"])
            .args(["-of", "compact=nokey=0"])
            .arg(&path)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("codec_name=mjpeg|width=64|height=48|nb_read_frames=3"),
            "{text}"
        );
        assert!(text.contains("duration=1.5"), "{text}");
        assert!(
            out.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
