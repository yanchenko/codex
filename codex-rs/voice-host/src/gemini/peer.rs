//! Local-only answerer using the exact RTC and codec stack of the native helper.
use futures::future::BoxFuture;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::time::timeout;
use webrtc::data_channel::DataChannel;
use webrtc::peer_connection::PeerConnection;
use webrtc::peer_connection::PeerConnectionBuilder;
use webrtc::peer_connection::PeerConnectionEventHandler;
use webrtc::peer_connection::RTCIceGatheringState;
use webrtc::peer_connection::RTCPeerConnectionState;
use webrtc::peer_connection::RTCSessionDescription;
use webrtc::peer_connection::SettingEngine;

struct Events {
    gathered: Arc<Notify>,
    channel: mpsc::Sender<Arc<dyn DataChannel>>,
    seen: AtomicBool,
    failed: Arc<AtomicBool>,
}
impl PeerConnectionEventHandler for Events {
    fn on_data_channel<'a, 'b>(&'a self, channel: Arc<dyn DataChannel>) -> BoxFuture<'b, ()>
    where
        'a: 'b,
        Self: 'b,
    {
        Box::pin(async move {
            if self.seen.swap(true, Ordering::SeqCst)
                || channel.label().await.ok().as_deref() != Some("oai-events")
                || channel.ordered().await.ok() != Some(true)
                || self.channel.try_send(channel.clone()).is_err()
            {
                self.failed.store(true, Ordering::SeqCst);
                let _ = channel.close().await;
            }
        })
    }
    fn on_connection_state_change<'a, 'b>(
        &'a self,
        state: RTCPeerConnectionState,
    ) -> BoxFuture<'b, ()>
    where
        'a: 'b,
        Self: 'b,
    {
        Box::pin(async move {
            if matches!(
                state,
                RTCPeerConnectionState::Failed
                    | RTCPeerConnectionState::Disconnected
                    | RTCPeerConnectionState::Closed
            ) {
                self.failed.store(true, Ordering::SeqCst);
            }
        })
    }
    fn on_ice_gathering_state_change<'a, 'b>(
        &'a self,
        state: RTCIceGatheringState,
    ) -> BoxFuture<'b, ()>
    where
        'a: 'b,
        Self: 'b,
    {
        Box::pin(async move {
            if state == RTCIceGatheringState::Complete {
                self.gathered.notify_one();
            }
        })
    }
}

pub(super) fn local_addresses() -> Result<HashSet<std::net::IpAddr>, &'static str> {
    let mut ips = HashSet::from([
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
    ]);
    for interface in
        rtc::shared::ifaces::ifaces().map_err(|_| "local interface discovery failed")?
    {
        if let Some(address) = interface.addr
            && !address.ip().is_unspecified()
        {
            ips.insert(address.ip());
        }
    }
    Ok(ips)
}

pub(super) fn validate_offer(sdp: &str) -> Result<(), &'static str> {
    normalized_offer(sdp).map(|_| ())
}
fn normalized_offer(sdp: &str) -> Result<String, &'static str> {
    if sdp.is_empty() || sdp.len() > 64 * 1024 {
        return Err("invalid offer size");
    }
    let ips = local_addresses()?;
    let mut count = 0;
    let mut accepted = 0;
    let mut media = Vec::new();
    let mut lines = Vec::new();
    for line in sdp.lines() {
        if line.starts_with("m=") {
            media.push(line.split_whitespace().next().unwrap_or(""));
        }
        if line.starts_with("a=setup:") && line != "a=setup:actpass" {
            return Err("actpass offer required");
        }
        if let Some(candidate) = line.strip_prefix("a=candidate:") {
            count += 1;
            let fields = candidate.split_whitespace().collect::<Vec<_>>();
            if count > 32 || fields.len() < 8 || fields[6] != "typ" || fields[7] != "host" {
                return Err("invalid local candidate");
            }
            let ip = fields[4]
                .parse::<std::net::IpAddr>()
                .map_err(|_| "numeric candidate required")?;
            if !ips.contains(&ip) {
                return Err("candidate is not assigned locally");
            }
            if fields[5]
                .parse::<u16>()
                .map_err(|_| "invalid candidate port")?
                == 0
            {
                return Err("invalid candidate port");
            }
            if !ip.is_ipv4() || !fields[2].eq_ignore_ascii_case("udp") || fields[1] != "1" {
                continue;
            }
            accepted += 1;
        }
        if line.starts_with("a=ice-options:") {
            continue;
        }
        lines.push(line);
    }
    media.sort_unstable();
    if media != ["m=application", "m=audio"] || accepted == 0 {
        return Err("local audio and datachannel required");
    }
    Ok(lines.join("\r\n") + "\r\n")
}

pub(super) struct Peer {
    pub(super) connection: Arc<dyn PeerConnection>,
    pub(super) failed: Arc<AtomicBool>,
    pub(super) audio: crate::audio_track::AudioTrack,
    pub(super) incoming: crate::incoming::Incoming,
    pub(super) channels: mpsc::Receiver<Arc<dyn DataChannel>>,
    gathered: Arc<Notify>,
}
impl Peer {
    pub(super) async fn new() -> Result<Self, &'static str> {
        let gathered = Arc::new(Notify::new());
        let failed = Arc::new(AtomicBool::new(false));
        let (sender, channels) = mpsc::channel(1);
        let (media, audio) = crate::audio_track::AudioTrack::new()?;
        let (incoming, ingress) = crate::incoming::Incoming::new();
        incoming.set_suppressed(false)?;
        let mut settings = SettingEngine::default();
        settings.set_lite(true);
        settings.set_multicast_dns_mode(rtc::ice::mdns::MulticastDnsMode::Disabled);
        let connection = PeerConnectionBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(rtc::interceptor::Registry::from(ingress))
            .with_setting_engine(settings)
            .with_runtime(Arc::new(super::network::LocalRuntime))
            .with_handler(Arc::new(Events {
                gathered: gathered.clone(),
                channel: sender,
                seen: AtomicBool::new(false),
                failed: failed.clone(),
            }))
            .with_udp_addrs(vec!["127.0.0.1:0"])
            .build()
            .await
            .map_err(|_| "RTC startup failed")?;
        let peer = Self {
            connection: Arc::new(connection),
            audio,
            incoming,
            channels,
            gathered,
            failed,
        };
        peer.connection
            .add_track(peer.audio.track.clone())
            .await
            .map_err(|_| "RTC audio setup failed")?;
        Ok(peer)
    }
    pub(super) async fn answer(&self, offer: String) -> Result<String, &'static str> {
        let offer = normalized_offer(&offer)?;
        self.connection
            .set_remote_description(RTCSessionDescription::offer(offer).map_err(|_| "invalid SDP")?)
            .await
            .map_err(|_| "invalid offer")?;
        let answer = self
            .connection
            .create_answer(None)
            .await
            .map_err(|_| "answer failed")?;
        self.connection
            .set_local_description(answer)
            .await
            .map_err(|_| "answer failed")?;
        self.gathered.notified().await;
        Ok(self
            .connection
            .local_description()
            .await
            .ok_or("missing answer")?
            .sdp)
    }
    pub(super) async fn close(&self) {
        let _ = timeout(Duration::from_secs(2), self.connection.close()).await;
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        let peer = self.connection.clone();
        tokio::spawn(async move {
            let _ = timeout(Duration::from_secs(2), peer.close()).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn native_helper_transport_opus_and_datachannel_with_loopback_answerer() {
        timeout(Duration::from_secs(25), async {
            let gathered = Arc::new(Notify::new());
            let failed = Arc::new(AtomicBool::new(false));
            let (sender, mut channels) = mpsc::channel(1);
            let (media, mut audio) = crate::audio_track::AudioTrack::new().unwrap();
            let (mut incoming, ingress) = crate::incoming::Incoming::new();
            incoming.set_suppressed(false).unwrap();
            let mut settings = SettingEngine::default();
            settings.set_lite(true);
            settings.set_multicast_dns_mode(rtc::ice::mdns::MulticastDnsMode::Disabled);
            let peer = PeerConnectionBuilder::new()
                .with_media_engine(media)
                .with_interceptor_registry(rtc::interceptor::Registry::from(ingress))
                .with_setting_engine(settings)
                .with_handler(Arc::new(Events {
                    gathered: gathered.clone(),
                    channel: sender,
                    seen: AtomicBool::new(false),
                    failed: failed.clone(),
                }))
                .with_runtime(Arc::new(super::super::network::LocalRuntime))
                .with_udp_addrs(vec!["127.0.0.1:0"])
                .build()
                .await
                .unwrap();
            peer.add_track(audio.track.clone()).await.unwrap();
            let mut native = crate::transport::Transport::new().await.unwrap();
            let offer = native.offer().await.unwrap();
            validate_offer(&offer).unwrap();
            peer.set_remote_description(RTCSessionDescription::offer(offer).unwrap())
                .await
                .unwrap();
            peer.set_local_description(peer.create_answer(None).await.unwrap())
                .await
                .unwrap();
            gathered.notified().await;
            let answer = peer.local_description().await.unwrap().sdp;
            assert!(answer.contains("127.0.0.1"));
            assert_eq!(
                native.apply_answer(answer).await.unwrap(),
                crate::transport::AnswerOutcome::Ready
            );
            let channel = channels.recv().await.unwrap();
            assert_eq!(channel.label().await.unwrap(), "oai-events");
            native.incoming.set_suppressed(false).unwrap();
            native
                .audio
                .send(crate::audio_track::EncodedAudio {
                    data: vec![0xf8, 0xff, 0xfe],
                    at: std::time::Instant::now(),
                })
                .await
                .unwrap();
            audio
                .send(crate::audio_track::EncodedAudio {
                    data: vec![0xf8, 0xff, 0xfe],
                    at: std::time::Instant::now(),
                })
                .await
                .unwrap();
            for which in [&mut incoming, &mut native.incoming] {
                loop {
                    if let Some(packet) = which.take().unwrap() {
                        assert!(packet.as_ref().ends_with(&[0xf8, 0xff, 0xfe]));
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
            native.close().await.unwrap();
            peer.close().await.unwrap();
        })
        .await
        .unwrap();
    }
    #[test]
    fn remote_candidate_rejected() {
        assert!(validate_offer("a=candidate:1 1 UDP 1 8.8.8.8 9 typ host\r\n").is_err());
    }
}
