// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use bt_a2dp::codec::MediaCodecConfig;
use bt_a2dp::media_task::*;
use bt_avdtp::{self as avdtp, MediaStream};
use fidl::endpoints::create_request_stream;
use fidl_fuchsia_bluetooth_bredr::AudioOffloadExtProxy;
use fuchsia_bluetooth::inspect::DataStreamInspect;
use fuchsia_bluetooth::types::PeerId;
use fuchsia_inspect::Node;
use fuchsia_inspect_derive::{AttachError, Inspect};
use futures::channel::oneshot;
use futures::future::{BoxFuture, Fuse, Future, Shared};
use futures::{select, FutureExt, StreamExt, TryFutureExt};
use log::{debug, info, trace, warn};
use thiserror::Error;
use {
    fidl_fuchsia_media as media, fidl_fuchsia_media_sessions2 as sessions2,
    fuchsia_async as fasync, fuchsia_trace as trace,
};

use crate::avrcp_relay::AvrcpRelay;
use crate::media::player;

#[derive(Clone)]
pub struct Builder {
    metrics_logger: bt_metrics::MetricsLogger,
    publisher: sessions2::PublisherProxy,
    audio_consumer_factory: media::SessionAudioConsumerFactoryProxy,
    domain: String,
    aac_available: bool,
}

fn build_sbc_sink() -> MediaCodecConfig {
    use bt_a2dp::media_types::*;
    let sbc_codec_info = SbcCodecInfo::new(
        SbcSamplingFrequency::MANDATORY_SNK,
        SbcChannelMode::MANDATORY_SNK,
        SbcBlockCount::MANDATORY_SNK,
        SbcSubBands::MANDATORY_SNK,
        SbcAllocation::MANDATORY_SNK,
        SbcCodecInfo::BITPOOL_MIN,
        53, // Recommended max bitpool value from A2DP 1.4 Table 4.7
    )
    .unwrap();

    let codec_cap = avdtp::ServiceCapability::MediaCodec {
        media_type: avdtp::MediaType::Audio,
        codec_type: avdtp::MediaCodecType::AUDIO_SBC,
        codec_extra: sbc_codec_info.to_bytes().to_vec(),
    };
    (&codec_cap).try_into().unwrap()
}

fn build_aac_sink(bitrate: u32) -> MediaCodecConfig {
    use bt_a2dp::media_types::*;
    let aac_codec_info = AacCodecInfo::new(
        AacObjectType::MANDATORY_SNK,
        AacSamplingFrequency::MANDATORY_SNK,
        AacChannels::MANDATORY_SNK,
        true,
        bitrate,
    )
    .unwrap();
    (&avdtp::ServiceCapability::MediaCodec {
        media_type: avdtp::MediaType::Audio,
        codec_type: avdtp::MediaCodecType::AUDIO_AAC,
        codec_extra: aac_codec_info.to_bytes().to_vec(),
    })
        .try_into()
        .unwrap()
}
impl Builder {
    pub fn new(
        metrics_logger: bt_metrics::MetricsLogger,
        publisher: sessions2::PublisherProxy,
        audio_consumer_factory: media::SessionAudioConsumerFactoryProxy,
        domain: String,
        aac_available: bool,
    ) -> Self {
        Self { metrics_logger, publisher, audio_consumer_factory, domain, aac_available }
    }
}

impl MediaTaskBuilder for Builder {
    fn configure(
        &self,
        peer_id: &PeerId,
        codec_config: &MediaCodecConfig,
    ) -> Result<Box<dyn MediaTaskRunner>, MediaTaskError> {
        let builder = self.clone();
        let peer_id = peer_id.clone();
        let codec_config = codec_config.clone();
        Ok::<Box<dyn MediaTaskRunner>, _>(Box::new(ConfiguredSinkTask::new(
            codec_config,
            builder,
            peer_id,
        )))
    }

    fn direction(&self) -> avdtp::EndpointType {
        avdtp::EndpointType::Sink
    }

    fn supported_configs(
        &self,
        _peer_id: &PeerId,
        _offload: Option<AudioOffloadExtProxy>,
    ) -> BoxFuture<'static, Result<Vec<MediaCodecConfig>, MediaTaskError>> {
        let media_configs = if self.aac_available {
            // 0 = Unknown constant bitrate support (A2DP Sec. 4.5.2.4)
            vec![build_aac_sink(0), build_sbc_sink()]
        } else {
            vec![build_sbc_sink()]
        };
        futures::future::ready(Ok(media_configs)).boxed()
    }
}

struct ConfiguredSinkTask {
    /// Configuration providing the format of encoded audio requested.
    codec_config: MediaCodecConfig,
    /// The ID of the peer that this is configured for.
    peer_id: PeerId,
    /// A clone of the Builder at the time this was configured.
    builder: Builder,
    /// Future that will return the Session ID for Media, if we have started the session.
    session_id_fut: Option<Shared<oneshot::Receiver<u64>>>,
    /// Session Task (AVRCP relay) if it is started.
    _session_task: Option<fasync::Task<()>>,
    /// Inspect node
    inspect: fuchsia_inspect::Node,
}

impl ConfiguredSinkTask {
    fn new(codec_config: MediaCodecConfig, builder: Builder, peer_id: PeerId) -> Self {
        Self {
            codec_config,
            builder,
            peer_id,
            session_id_fut: None,
            _session_task: None,
            inspect: Node::default(),
        }
    }

    fn establish_session(&mut self) -> impl Future<Output = u64> {
        if self.session_id_fut.is_none() {
            // Need to start the session task and send the result.
            let (sender, recv) = futures::channel::oneshot::channel();
            self.session_id_fut = Some(recv.shared());
            let peer_id = self.peer_id.clone();
            let builder = self.builder.clone();
            let inspect = self.inspect.clone_weak();
            let session_fut = async move {
                let _ = &builder;
                let (player_client, player_requests) = create_request_stream();

                let registration = sessions2::PlayerRegistration {
                    domain: Some(builder.domain),
                    ..Default::default()
                };

                match builder.publisher.publish(player_client, &registration).await {
                    Ok(session_id) => {
                        info!(peer_id:%, session_id:%; "Published session");
                        // If the receiver has hung up, this task will be dropped.
                        let _ = sender.send(session_id);
                        let mut avrcp = AvrcpRelay::default();
                        let _ = avrcp.iattach(&inspect, "avrcp_relay");
                        // We ignore AVRCP relay errors, they are logged.
                        if let Ok(relay_task) = avrcp.start(peer_id, player_requests) {
                            relay_task.await;
                        }
                    }
                    Err(e) => warn!(peer_id:%, e:?; "Couldn't publish session"),
                };
            };
            self._session_task = Some(fasync::Task::local(session_fut));
        }
        self.session_id_fut
            .as_ref()
            .cloned()
            .expect("just set this")
            .map_ok_or_else(|_e| 0, |id| id)
    }

    fn update_inspect(&self) {
        self.inspect.record_string("codec_config", &format!("{:?}", self.codec_config));
    }
}

impl Inspect for &mut ConfiguredSinkTask {
    fn iattach(
        self,
        parent: &fuchsia_inspect::Node,
        name: impl AsRef<str>,
    ) -> Result<(), AttachError> {
        self.inspect = parent.create_child(name.as_ref());
        self.update_inspect();
        Ok(())
    }
}

impl MediaTaskRunner for ConfiguredSinkTask {
    fn start(
        &mut self,
        stream: MediaStream,
        _offload: Option<AudioOffloadExtProxy>,
    ) -> Result<Box<dyn MediaTask>, MediaTaskError> {
        let codec_config = self.codec_config.clone();
        let audio_factory = self.builder.audio_consumer_factory.clone();
        let mut stream_inspect = DataStreamInspect::default();
        let _ = stream_inspect.iattach(&self.inspect, "data_stream");
        let session_id_fut = self.establish_session();
        let peer_id = self.peer_id;
        let media_player_fut = async move {
            let session_id = session_id_fut.await;
            let err = media_stream_task(
                peer_id,
                stream,
                Box::new(move || {
                    player::Player::new(session_id, codec_config.clone(), audio_factory.clone())
                }),
                stream_inspect,
            )
            .await;
            Err(MediaTaskError::Other(format!("Unrecoverable streaming error: {err:?}")))
        };
        let codec_type = self.codec_config.codec_type().clone();
        let metrics_logger = self.builder.metrics_logger.clone();
        let task = RunningSinkTask::start(media_player_fut, metrics_logger, codec_type);
        Ok(Box::new(task))
    }

    fn reconfigure(&mut self, codec_config: &MediaCodecConfig) -> Result<(), MediaTaskError> {
        self.codec_config = codec_config.clone();
        self.update_inspect();
        Ok(())
    }

    fn iattach(&mut self, parent: &Node, name: &str) -> Result<(), AttachError> {
        fuchsia_inspect_derive::Inspect::iattach(self, parent, name)
    }
}

#[derive(Error, Debug)]
enum StreamingError {
    /// The media stream ended.
    #[error("Media stream ended")]
    MediaStreamEnd,
    /// The media stream returned an error. The error is provided.
    #[error("Media stream error: {:?}", _0)]
    MediaStreamError(avdtp::Error),
    /// The media player failed to start.
    #[error("Player failed during startup")]
    PlayerFailedStart,
    /// The media player failed to generate.
    #[error("Player failed setup: {:?}", _0)]
    PlayerFailedSetup(Error),
}

/// Sink task which is running a given media_task future, and will send it's result to multiple
/// interested parties.
/// Reports the streaming metrics to Cobalt when streaming has completed.
struct RunningSinkTask {
    media_task: Option<fasync::Task<()>>,
    result_fut: Shared<fasync::Task<Result<(), MediaTaskError>>>,
}

impl RunningSinkTask {
    fn start(
        media_task: impl Future<Output = Result<(), MediaTaskError>> + Send + 'static,
        metrics: bt_metrics::MetricsLogger,
        codec_type: avdtp::MediaCodecType,
    ) -> Self {
        let (sender, receiver) = oneshot::channel();
        let wrapped_media_task = fasync::Task::spawn(async move {
            let result = media_task.await;
            let _ = sender.send(result);
        });
        let recv_task = fasync::Task::spawn(async move {
            // Receives the result of the media task, or Canceled, from the stop() dropping it
            receiver.await.unwrap_or(Ok(()))
        });
        let result_fut = recv_task.shared();
        fasync::Task::spawn({
            let task_finished = result_fut.clone();
            async move {
                let start_time = fasync::MonotonicInstant::now();
                trace::instant!(c"bt-a2dp", c"Media:Start", trace::Scope::Thread);
                let _ = task_finished.await;
                trace::instant!(c"bt-a2dp", c"Media:Stop", trace::Scope::Thread);
                let end_time = fasync::MonotonicInstant::now();

                report_stream_metrics(metrics, &codec_type, (end_time - start_time).into_seconds())
            }
        })
        .detach();
        Self { media_task: Some(wrapped_media_task), result_fut }
    }
}

impl MediaTask for RunningSinkTask {
    fn finished(&mut self) -> BoxFuture<'static, Result<(), MediaTaskError>> {
        self.result_fut.clone().boxed()
    }

    fn stop(&mut self) -> Result<(), MediaTaskError> {
        if let Some(_task) = self.media_task.take() {
            debug!("Media Task stopped via stop signal");
        }
        // Either there was already a result, or we just send Ok(()) by dropping the sender.
        self.result().unwrap_or(Ok(()))
    }
}

/// Task for media streaming. Creates the player once data first arrives, and rebuilds the player
/// to recover from errors when possible, ending when there is an unrecoverable streaming or
/// playback error.  Reports stream progress using `inspect`.
async fn media_stream_task(
    peer_id: PeerId,
    stream: (impl futures::Stream<Item = avdtp::Result<Vec<u8>>> + std::marker::Unpin),
    player_gen: Box<dyn Fn() -> Result<player::Player, Error> + Send>,
    mut inspect: DataStreamInspect,
) -> StreamingError {
    let mut stream = stream.fuse();
    let mut player: Option<player::Player> = None;
    let mut packet_count: u64 = 0;
    inspect.start();
    loop {
        let mut player_event_fut =
            player.as_mut().map(|p| p.next_event().fuse()).unwrap_or_else(Fuse::terminated);

        select! {
            stream_packet = stream.next() => {
                drop(player_event_fut);
                let pkt = match stream_packet {
                    None => break StreamingError::MediaStreamEnd,
                    Some(Err(e)) => break StreamingError::MediaStreamError(e),
                    Some(Ok(packet)) => packet,
                };

                packet_count += 1;
                // link incoming and outgoing flows together with shared duration event
                trace::duration!(c"bt-a2dp", c"Profile packet received");
                trace::flow_end!(c"bluetooth", c"ProfilePacket", packet_count.into());

                if player.is_none() {
                    info!(peer_id:%; "starting audio player");
                    let mut new_player = match player_gen() {
                        Ok(player) => player,
                        Err(e) => break StreamingError::PlayerFailedSetup(e),
                    };
                    // Get the first status from the player to confirm it is setup.
                    if let player::PlayerEvent::Closed = new_player.next_event().await {
                        break StreamingError::PlayerFailedStart;
                    }
                    player = Some(new_player);
                }

                inspect.record_transferred(pkt.len(), fasync::MonotonicInstant::now());
                if let Err(e) = player.as_mut().unwrap().push_payload(&pkt.as_slice()).await {
                    warn!(peer_id:%, e:?; "can't play audio packet");
                }
            },
            player_event = player_event_fut => {
                drop(player_event_fut);
                match player_event {
                    player::PlayerEvent::Closed => player = None,
                    player::PlayerEvent::Status(s) => {
                        trace!("PlayerEvent Status happened: {:?}", s);
                    },
                }
            },
        }
    }
}

fn report_stream_metrics(
    metrics_logger: bt_metrics::MetricsLogger,
    codec_type: &avdtp::MediaCodecType,
    duration_seconds: i64,
) {
    let codec = match codec_type {
        &avdtp::MediaCodecType::AUDIO_SBC => {
            bt_metrics::A2dpStreamDurationInSecondsMigratedMetricDimensionCodec::Sbc
        }
        &avdtp::MediaCodecType::AUDIO_AAC => {
            bt_metrics::A2dpStreamDurationInSecondsMigratedMetricDimensionCodec::Aac
        }
        _ => bt_metrics::A2dpStreamDurationInSecondsMigratedMetricDimensionCodec::Unknown,
    };
    metrics_logger.log_integer(
        bt_metrics::A2DP_STREAM_DURATION_IN_SECONDS_MIGRATED_METRIC_ID,
        duration_seconds,
        vec![codec as u32],
    );
}

#[cfg(all(test, feature = "test_encoding"))]
mod tests {
    use super::*;

    use async_test_helpers::run_while;
    use async_utils::PollExt;
    use bt_metrics::respond_to_metrics_req_for_test;
    use diagnostics_assertions::assert_data_tree;
    use fidl::endpoints::create_proxy_and_stream;
    use fidl_fuchsia_media::{
        AudioConsumerRequest, AudioConsumerStatus, SessionAudioConsumerFactoryMarker,
        StreamSinkRequest,
    };
    use fidl_fuchsia_media_sessions2::{PublisherMarker, PublisherRequest};
    use fuchsia_bluetooth::types::Channel;
    use fuchsia_inspect_derive::WithInspect;
    use fuchsia_sync::Mutex;
    use futures::channel::mpsc;
    use futures::io::AsyncWriteExt;
    use futures::task::Poll;
    use std::pin::pin;
    use std::sync::{Arc, RwLock};
    use {fidl_fuchsia_metrics as cobalt, fuchsia_inspect as inspect};

    fn fake_cobalt_sender() -> (bt_metrics::MetricsLogger, cobalt::MetricEventLoggerRequestStream) {
        let (c, s) = fidl::endpoints::create_proxy_and_stream::<cobalt::MetricEventLoggerMarker>();
        (bt_metrics::MetricsLogger::from_proxy(c), s)
    }

    #[fuchsia::test]
    fn sink_task_works_without_session() {
        let mut exec = fasync::TestExecutor::new();
        let (proxy, mut session_requests) =
            fidl::endpoints::create_proxy_and_stream::<PublisherMarker>();
        let (audio_consumer_factory_proxy, mut audio_factory_requests) =
            create_proxy_and_stream::<SessionAudioConsumerFactoryMarker>();
        let builder = Builder::new(
            bt_metrics::MetricsLogger::default(),
            proxy,
            audio_consumer_factory_proxy,
            "Tests".to_string(),
            false,
        );

        let sbc_config = MediaCodecConfig::min_sbc();
        let mut runner = builder.configure(&PeerId(1), &sbc_config).expect("configured");

        // Should't start session until we start a stream.
        assert!(exec.run_until_stalled(&mut session_requests.next()).is_pending());

        let (local, mut remote) = Channel::create();
        let local = Arc::new(RwLock::new(local));
        let stream =
            MediaStream::new(Arc::new(fuchsia_sync::Mutex::new(true)), Arc::downgrade(&local));

        let mut running_task = runner.start(stream, None).expect("task should start");

        // Should try to publish the session now.
        match exec.run_until_stalled(&mut session_requests.next()) {
            Poll::Ready(Some(Ok(PublisherRequest::Publish { responder, .. }))) => {
                drop(responder);
            }
            x => panic!("Expected a publisher request, got {:?}", x),
        };
        drop(session_requests);

        let finished_fut = running_task.finished();
        let mut finished_fut = pin!(finished_fut);

        // Shouldn't end the running media task
        assert!(exec.run_until_stalled(&mut finished_fut).is_pending());

        // If we send some data through the media stream...
        let _ = exec.run_singlethreaded(&mut remote.write_all(&[0xF0, 0x9F, 0x92, 0x96]));

        // Should try to start the player
        match exec.run_until_stalled(&mut audio_factory_requests.next()) {
            Poll::Ready(Some(Ok(
                media::SessionAudioConsumerFactoryRequest::CreateAudioConsumer { .. },
            ))) => {}
            x => panic!("Expected a audio consumer request, got {:?}", x),
        };
    }

    #[fuchsia::test]
    fn dropped_task_reports_metrics() {
        let mut exec = fasync::TestExecutor::new();
        let (metrics_logger, mut recv) = fake_cobalt_sender();
        let (proxy, mut session_requests) =
            fidl::endpoints::create_proxy_and_stream::<PublisherMarker>();
        let (audio_consumer_factory_proxy, _audio_factory_requests) =
            create_proxy_and_stream::<SessionAudioConsumerFactoryMarker>();
        let builder = Builder::new(
            metrics_logger,
            proxy,
            audio_consumer_factory_proxy,
            "Tests".to_string(),
            false,
        );

        let sbc_config = MediaCodecConfig::min_sbc();
        let mut runner = builder.configure(&PeerId(1), &sbc_config).expect("configured");

        // Should't start session until we start a stream.
        assert!(exec.run_until_stalled(&mut session_requests.next()).is_pending());

        let (local, _remote) = Channel::create();
        let local = Arc::new(RwLock::new(local));
        let stream =
            MediaStream::new(Arc::new(fuchsia_sync::Mutex::new(true)), Arc::downgrade(&local));

        let mut running_task = runner.start(stream, None).expect("media task should start");

        running_task.stop().expect("task to stop with okay");
        drop(running_task);

        // Should receive a metrics report.
        match exec.run_until_stalled(&mut recv.next()).expect("should be ready") {
            Some(Ok(req)) => {
                let got = respond_to_metrics_req_for_test(req);
                assert_eq!(
                    bt_metrics::A2DP_STREAM_DURATION_IN_SECONDS_MIGRATED_METRIC_ID,
                    got.metric_id
                );
            }
            x => panic!("should have received request: {:?}", x),
        }
    }

    fn setup_media_stream_test() -> (fasync::TestExecutor, MediaCodecConfig, DataStreamInspect) {
        let exec = fasync::TestExecutor::new();
        let sbc_config = MediaCodecConfig::min_sbc();
        let inspect = DataStreamInspect::default();
        (exec, sbc_config, inspect)
    }

    #[fuchsia::test]
    /// Test that cobalt metrics are sent after stream ends
    fn test_cobalt_metrics() {
        let mut exec = fasync::TestExecutor::new();
        let (metrics_logger, mut recv) = fake_cobalt_sender();
        const TEST_DURATION: i64 = 1;
        // First report the stream metrics.
        report_stream_metrics(metrics_logger, &avdtp::MediaCodecType::AUDIO_AAC, TEST_DURATION);
        match exec.run_singlethreaded(&mut recv.next()).expect("should be ready") {
            Ok(req) => {
                let got = respond_to_metrics_req_for_test(req);
                assert_eq!(
                    bt_metrics::A2DP_STREAM_DURATION_IN_SECONDS_MIGRATED_METRIC_ID,
                    got.metric_id
                );
                assert_eq!(
                    vec![
                        bt_metrics::A2dpStreamDurationInSecondsMigratedMetricDimensionCodec::Aac
                            as u32
                    ],
                    got.event_codes
                );
                assert_eq!(cobalt::MetricEventPayload::IntegerValue(TEST_DURATION), got.payload);
            }
            x => panic!("should have received request: {:?}", x),
        }
    }

    fn once_player_gen(
        player: player::Player,
    ) -> Box<dyn Fn() -> Result<player::Player, Error> + Send> {
        let player_hold = Mutex::new(Some(player));
        Box::new(move || {
            player_hold.lock().take().ok_or(anyhow::format_err!("Only one player available"))
        })
    }

    #[fuchsia::test]
    fn media_stream_empty() {
        let (mut exec, sbc_config, inspect) = setup_media_stream_test();
        let (player, _sink_requests, _consumer_requests, _vmo) =
            player::tests::setup_player(&mut exec, sbc_config);

        let mut empty_stream = futures::stream::empty();
        let player_gen = once_player_gen(player);

        let decode_fut = media_stream_task(PeerId(0), &mut empty_stream, player_gen, inspect);
        let mut decode_fut = pin!(decode_fut);

        match exec.run_until_stalled(&mut decode_fut) {
            Poll::Ready(StreamingError::MediaStreamEnd) => {}
            x => panic!("Expected decoding to end when media stream ended, got {:?}", x),
        };
    }

    #[fuchsia::test]
    fn media_stream_error() {
        let (mut exec, sbc_config, inspect) = setup_media_stream_test();
        let (player, _sink_requests, _consumer_requests, _vmo) =
            player::tests::setup_player(&mut exec, sbc_config);

        let mut error_stream =
            futures::stream::poll_fn(|_| -> Poll<Option<avdtp::Result<Vec<u8>>>> {
                Poll::Ready(Some(Err(avdtp::Error::PeerDisconnected)))
            });
        let player_gen = once_player_gen(player);

        let decode_fut = media_stream_task(PeerId(0), &mut error_stream, player_gen, inspect);
        let mut decode_fut = pin!(decode_fut);

        match exec.run_until_stalled(&mut decode_fut) {
            Poll::Ready(StreamingError::MediaStreamError(avdtp::Error::PeerDisconnected)) => {}
            x => panic!("Expected decoding to end with included error, got {:?}", x),
        };
    }

    /// Returns a stream of packets that can be used for testing.
    fn packets_stream() -> impl futures::Stream<Item = avdtp::Result<Vec<u8>>> + Unpin {
        futures::stream::repeat_with(|| Ok(vec![0xF0, 0x9F, 0x92, 0x96]))
    }

    #[fuchsia::test]
    fn media_player_fails_start() {
        let (mut exec, sbc_config, inspect) = setup_media_stream_test();
        let (player, mut sink_requests, mut consumer_requests, _vmo) =
            player::tests::setup_player(&mut exec, sbc_config);

        let mut stream = packets_stream();

        let player_gen = once_player_gen(player);

        let decode_fut = media_stream_task(PeerId(0), &mut stream, player_gen, inspect);
        let mut decode_fut = pin!(decode_fut);

        match exec.run_until_stalled(&mut decode_fut) {
            Poll::Pending => {}
            x => panic!("Expected pending while waiting to generate player but got {:?}", x),
        };
        let responder = match exec.run_until_stalled(&mut consumer_requests.select_next_some()) {
            Poll::Ready(Ok(AudioConsumerRequest::WatchStatus { responder, .. })) => responder,
            x => panic!("Expected a watch status request from the player setup, but got {:?}", x),
        };
        drop(responder);
        drop(consumer_requests);
        let (decode_result, _) = run_while(&mut exec, &mut sink_requests.next(), decode_fut);

        match decode_result {
            StreamingError::PlayerFailedStart => {}
            x => panic!("Expected player to fail to start. got {:?}", x),
        }
    }

    #[fuchsia::test]
    fn media_player_fails_generation() {
        let (mut exec, _sbc_config, inspect) = setup_media_stream_test();
        let player_gen = Box::new(|| Err(anyhow::format_err!("No player available")));

        let mut stream = packets_stream();

        let decode_fut = media_stream_task(PeerId(0), &mut stream, player_gen, inspect);
        let mut decode_fut = pin!(decode_fut);

        match exec.run_until_stalled(&mut decode_fut) {
            Poll::Ready(StreamingError::PlayerFailedSetup(_)) => {}
            x => panic!("Expected fail immediately after first data but got {:?}", x),
        };
    }

    #[fuchsia::test]
    fn media_stream_stats() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        let sbc_config = MediaCodecConfig::min_sbc();
        let (player, mut sink_requests, mut consumer_requests, _vmo) =
            player::tests::setup_player(&mut exec, sbc_config);
        let inspector = inspect::component::inspector();
        let root = inspector.root();
        let inspect =
            DataStreamInspect::default().with_inspect(root, "stream").expect("attach to tree");

        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(5_678900000));

        let (mut media_sender, mut media_receiver) = mpsc::channel(1);

        let player_gen = once_player_gen(player);

        let decode_fut = media_stream_task(PeerId(0), &mut media_receiver, player_gen, inspect);
        let mut decode_fut = pin!(decode_fut);

        assert!(exec.run_until_stalled(&mut decode_fut).is_pending());

        assert_data_tree!(@executor exec, inspector, root: {
        stream: {
            start_time: 5_678900000i64,
            total_bytes: 0 as u64,
            streaming_secs: 0 as u64,
            bytes_per_second_current: 0 as u64,
        }});

        // raw rtp header with sequence number of 1 followed by 1 sbc frame
        let raw = vec![
            128, 96, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0x9c, 0xb1, 0x20, 0x3b, 0x80, 0x00, 0x00,
            0x11, 0x7f, 0xfa, 0xab, 0xef, 0x7f, 0xfa, 0xab, 0xef, 0x80, 0x4a, 0xab, 0xaf, 0x80,
            0xf2, 0xab, 0xcf, 0x83, 0x8a, 0xac, 0x32, 0x8a, 0x78, 0x8a, 0x53, 0x90, 0xdc, 0xad,
            0x49, 0x96, 0xba, 0xaa, 0xe6, 0x9c, 0xa2, 0xab, 0xac, 0xa2, 0x72, 0xa9, 0x2d, 0xa8,
            0x9a, 0xab, 0x75, 0xae, 0x82, 0xad, 0x49, 0xb4, 0x6a, 0xad, 0xb1, 0xba, 0x52, 0xa9,
            0xa8, 0xc0, 0x32, 0xad, 0x11, 0xc6, 0x5a, 0xab, 0x3a,
        ];
        let sbc_packet_size = 85u64;

        media_sender.try_send(Ok(raw.clone())).expect("should be able to send into stream");
        exec.set_fake_time(fasync::MonotonicInstant::after(zx::MonotonicDuration::from_seconds(1)));
        assert!(exec.run_until_stalled(&mut decode_fut).is_pending());

        // Expect a request for status and respond as they setup the player after data is first
        // received.
        player::tests::respond_event_status(
            &mut exec,
            &mut consumer_requests,
            AudioConsumerStatus {
                min_lead_time: Some(50),
                max_lead_time: Some(500),
                error: None,
                presentation_timeline: None,
                ..Default::default()
            },
        );

        assert!(exec.run_until_stalled(&mut decode_fut).is_pending());

        // We should have updated the rx stats.
        assert_data_tree!(@executor exec, inspector, root: {
        stream: {
            start_time: 5_678900000i64,
            total_bytes: sbc_packet_size,
            streaming_secs: 1 as u64,
            bytes_per_second_current: sbc_packet_size,
        }});
        // Should get a packet send to the sink eventually as player gets around to it
        let (request, _decode_fut) =
            run_while(&mut exec, decode_fut, &mut sink_requests.select_next_some());
        match request {
            Ok(StreamSinkRequest::SendPacket { .. }) => {}
            x => panic!("Expected to receive a packet from sending data.. got {:?}", x),
        };
    }

    #[fuchsia::test]
    fn media_stream_task_reopens_player() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();

        let (audio_consumer_factory_proxy, mut audio_consumer_factory_request_stream) =
            create_proxy_and_stream::<SessionAudioConsumerFactoryMarker>();

        let sbc_config = MediaCodecConfig::min_sbc();
        let codec_type = sbc_config.codec_type().clone();
        let inspect = DataStreamInspect::default();
        let session_id = 1;

        let media_stream_fut = media_stream_task(
            PeerId(0),
            packets_stream(),
            Box::new(move || {
                player::Player::new(
                    session_id,
                    sbc_config.clone(),
                    audio_consumer_factory_proxy.clone(),
                )
            }),
            inspect,
        );
        let mut media_stream_fut = pin!(media_stream_fut);

        assert!(exec.run_until_stalled(&mut media_stream_fut).is_pending());

        let (_sink_request_stream, mut audio_consumer_request_stream, _sink_vmo) =
            player::tests::expect_player_setup(
                &mut exec,
                &mut audio_consumer_factory_request_stream,
                codec_type.clone(),
                session_id,
            );
        player::tests::respond_event_status(
            &mut exec,
            &mut audio_consumer_request_stream,
            AudioConsumerStatus {
                min_lead_time: Some(50),
                max_lead_time: Some(500),
                error: None,
                presentation_timeline: None,
                ..Default::default()
            },
        );

        drop(audio_consumer_request_stream);

        assert!(exec.run_until_stalled(&mut media_stream_fut).is_pending());

        // Should set up the player again after it closes.
        let (_sink_request_stream, audio_consumer_request_stream, _sink_vmo) =
            player::tests::expect_player_setup(
                &mut exec,
                &mut audio_consumer_factory_request_stream,
                codec_type,
                session_id,
            );

        // This time we don't respond to the event status, so the player failed immediately after
        // trying to be rebuilt and we end.
        drop(audio_consumer_request_stream);

        assert!(exec.run_until_stalled(&mut media_stream_fut).is_ready());
    }
}
