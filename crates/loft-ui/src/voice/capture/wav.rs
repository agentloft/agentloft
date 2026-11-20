//! Reading the fake microphone's WAV file: 16-bit PCM, any rate and
//! channel count (the RIFF layout: a `fmt ` chunk, then `data`; other
//! chunks are skipped).

use anyhow::{Context as _, Result, bail};

#[derive(Debug)]
pub(crate) struct Wav {
    pub rate: u32,
    pub channels: usize,
    /// Interleaved frames.
    pub samples: Vec<i16>,
}

const PCM: u16 = 1;
const EXTENSIBLE: u16 = 0xFFFE;

pub(crate) fn parse(bytes: &[u8]) -> Result<Wav> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        bail!("not a WAV file");
    }
    let mut rest = &bytes[12..];
    let mut format = None;
    while rest.len() >= 8 {
        let id = &rest[..4];
        let size = u32::from_le_bytes(rest[4..8].try_into()?) as usize;
        let body = &rest[8..];
        // A writer that didn't know the length leaves the data chunk's size
        // too big (or 0xFFFFFFFF): take what is there.
        let body = &body[..size.min(body.len())];
        match id {
            b"fmt " => {
                if body.len() < 16 {
                    bail!("WAV format chunk is too short");
                }
                let u16_at = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
                let mut tag = u16_at(0);
                if tag == EXTENSIBLE && body.len() >= 26 {
                    // The sub-format GUID starts with the real format tag.
                    tag = u16_at(24);
                }
                let channels = usize::from(u16_at(2));
                let rate = u32::from_le_bytes(body[4..8].try_into()?);
                let bits = u16_at(14);
                if tag != PCM || bits != 16 {
                    bail!(
                        "only 16-bit PCM WAV files are supported (this one: format {tag}, {bits} bits)"
                    );
                }
                if channels == 0 || rate == 0 {
                    bail!("WAV file has no channels or no sample rate");
                }
                format = Some((rate, channels));
            }
            b"data" => {
                let (rate, channels) = format.context("WAV data comes before its format")?;
                let samples = body
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&b| i16::from_le_bytes(b))
                    .collect();
                return Ok(Wav {
                    rate,
                    channels,
                    samples,
                });
            }
            _ => {}
        }
        // Chunks are padded to an even length.
        let advance = (8 + size + size % 2).min(rest.len());
        rest = &rest[advance..];
    }
    bail!("WAV file has no data")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.extend((body.len() as u32).to_le_bytes());
        out.extend(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    fn fmt(tag: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(tag.to_le_bytes());
        body.extend(channels.to_le_bytes());
        body.extend(rate.to_le_bytes());
        let align = channels * bits / 8;
        body.extend((rate * u32::from(align)).to_le_bytes());
        body.extend(align.to_le_bytes());
        body.extend(bits.to_le_bytes());
        body
    }

    fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = chunks.concat();
        let mut out = b"RIFF".to_vec();
        out.extend((body.len() as u32 + 4).to_le_bytes());
        out.extend(b"WAVE");
        out.extend(body);
        out
    }

    #[test]
    fn reads_pcm_past_other_chunks() {
        let samples: [i16; 4] = [1, -2, 300, -32_768];
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let file = riff(&[
            chunk(b"JUNK", b"odd"),
            chunk(b"fmt ", &fmt(PCM, 2, 22_050, 16)),
            chunk(b"FLLR", &[0; 7]),
            chunk(b"data", &data),
        ]);
        let wav = parse(&file).unwrap();
        assert_eq!((wav.rate, wav.channels), (22_050, 2));
        assert_eq!(wav.samples, samples);
    }

    #[test]
    fn reads_extensible_pcm_and_a_data_size_left_unknown() {
        let mut format = fmt(EXTENSIBLE, 1, 16_000, 16);
        format.extend(22u16.to_le_bytes()); // cbSize
        format.extend(16u16.to_le_bytes()); // valid bits
        format.extend(4u32.to_le_bytes()); // channel mask
        format.extend(PCM.to_le_bytes()); // sub-format GUID…
        format.extend([0; 14]);
        let mut data = b"data".to_vec();
        data.extend(u32::MAX.to_le_bytes());
        data.extend(7i16.to_le_bytes());
        let wav = parse(&riff(&[chunk(b"fmt ", &format), data])).unwrap();
        assert_eq!((wav.rate, wav.channels, wav.samples), (16_000, 1, vec![7]));
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        assert!(parse(b"hello").is_err());
        let float = riff(&[
            chunk(b"fmt ", &fmt(3, 1, 16_000, 32)),
            chunk(b"data", &[0; 8]),
        ]);
        assert!(
            parse(&float)
                .unwrap_err()
                .to_string()
                .contains("16-bit PCM")
        );
        let pcm24 = riff(&[
            chunk(b"fmt ", &fmt(PCM, 1, 16_000, 24)),
            chunk(b"data", &[0; 6]),
        ]);
        assert!(parse(&pcm24).is_err());
        assert!(parse(&riff(&[chunk(b"data", &[0; 2])])).is_err());
        assert!(parse(&riff(&[chunk(b"fmt ", &fmt(PCM, 1, 16_000, 16))])).is_err());
    }
}
