//! Opus directly converts the two provider sample rates. RTP remains 48 kHz.
use super::protocol::Result;
use opus::Application;
use opus::Channels;
use opus::Decoder;
use opus::Encoder;
use rtc::shared::marshal::Unmarshal;
use std::collections::VecDeque;
const MAX_SAMPLES: usize = 24000 * 8;
pub(super) struct Media {
    decoder: Decoder,
    encoder: Encoder,
    output: VecDeque<i16>,
}
impl Media {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            decoder: Decoder::new(16000, Channels::Mono).map_err(|_| "Opus decoder failed")?,
            encoder: Encoder::new(24000, Channels::Mono, Application::Voip)
                .map_err(|_| "Opus encoder failed")?,
            output: VecDeque::new(),
        })
    }
    pub(super) fn capture(&mut self, rtp: &[u8]) -> Result<Vec<u8>> {
        let packet =
            rtc::rtp::packet::Packet::unmarshal(&mut &rtp[..]).map_err(|_| "invalid RTP")?;
        let mut samples = [0_i16; 1920];
        let count = self
            .decoder
            .decode(&packet.payload, &mut samples, false)
            .map_err(|_| "invalid Opus")?;
        Ok(samples[..count]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect())
    }
    pub(super) fn append(&mut self, pcm: Vec<u8>) -> Result<()> {
        if !pcm.len().is_multiple_of(2) || self.output.len() + pcm.len() / 2 > MAX_SAMPLES {
            return Err("output audio overflow");
        }
        self.output.extend(
            pcm.chunks_exact(2)
                .map(|v| i16::from_le_bytes([v[0], v[1]])),
        );
        Ok(())
    }
    pub(super) fn clear(&mut self) {
        self.output.clear();
    }
    pub(super) fn packet(&mut self) -> Result<Vec<u8>> {
        let mut frame = [0_i16; 480];
        for v in &mut frame {
            *v = self.output.pop_front().unwrap_or(0);
        }
        let mut output = vec![0; 4000];
        let len = self
            .encoder
            .encode(&frame, &mut output)
            .map_err(|_| "Opus output failed")?;
        output.truncate(len);
        Ok(output)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rate_duration_queue_and_interruption() {
        let mut m = Media::new().unwrap();
        // Explicit PCM16: 480 samples at24k becomes320 at16k through Opus.
        let pcm = (0..480)
            .flat_map(|i| (((i as f32 * 0.15).sin() * 4000.0) as i16).to_le_bytes())
            .collect();
        m.append(pcm).unwrap();
        let encoded = m.packet().unwrap();
        let mut decoded = [0_i16; 1920];
        assert_eq!(
            m.decoder.decode(&encoded, &mut decoded, false).unwrap(),
            320
        );
        m.append(vec![0; MAX_SAMPLES * 2]).unwrap();
        assert!(m.append(vec![0; 2]).is_err());
        m.clear();
        assert!(m.output.is_empty());
        assert!(m.append(vec![1]).is_err());
    }
}
