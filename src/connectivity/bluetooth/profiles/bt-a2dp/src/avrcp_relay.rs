// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{format_err, Context as _, Error};
use async_helpers::maybe_stream::MaybeStream;
use battery_client::{BatteryClient, BatteryInfo, BatteryLevel};
use fasync::TimeoutExt;
use fidl::endpoints;
use fidl_table_validation::ValidFidlTable;
use fuchsia_bluetooth::types::PeerId;
use fuchsia_inspect::{self as inspect, Property};
use fuchsia_inspect_contrib::inspect_log;
use fuchsia_inspect_contrib::nodes::BoundedListNode;
use fuchsia_inspect_derive::{AttachError, Inspect};
use futures::{select, StreamExt};
use log::{debug, info, trace};
use std::fmt::Debug;
use zx::MonotonicDuration;
use {
    fidl_fuchsia_bluetooth_avrcp as avrcp, fidl_fuchsia_media as media,
    fidl_fuchsia_media_sessions2 as sessions2, fuchsia_async as fasync,
};

// Typically, AVRCP peer responds to requests within 0.2s.
// We wait for ~2x longer to give ample time for peer to respond.
const INITIAL_AVRCP_RESPONSE_WAIT_TIME: MonotonicDuration = MonotonicDuration::from_millis(500);

#[derive(Debug, Clone, ValidFidlTable, PartialEq)]
#[fidl_table_src(sessions2::PlayerStatus)]
pub struct ValidPlayerStatus {
    #[fidl_field_type(optional)]
    pub duration: Option<i64>,
    pub player_state: sessions2::PlayerState,
    #[fidl_field_type(optional)]
    pub timeline_function: Option<media::TimelineFunction>,
    pub repeat_mode: sessions2::RepeatMode,
    pub shuffle_on: bool,
    pub content_type: sessions2::ContentType,
    #[fidl_field_type(optional)]
    pub error: Option<sessions2::Error>,
}

/// Interval to poll Get Play Status on the remote AVRCP peer.
const AVRCP_GET_PLAY_STATUS_POLL_INTERVAL: zx::MonotonicDuration =
    zx::MonotonicDuration::from_seconds(2);

impl ValidPlayerStatus {
    /// Sets the `player_state` given a state from AVRCP.
    fn set_state_from_avrcp(&mut self, avrcp_status: avrcp::PlaybackStatus) {
        self.player_state = match avrcp_status {
            avrcp::PlaybackStatus::Stopped => sessions2::PlayerState::Idle,
            avrcp::PlaybackStatus::Playing => sessions2::PlayerState::Playing,
            avrcp::PlaybackStatus::Paused => sessions2::PlayerState::Paused,
            avrcp::PlaybackStatus::FwdSeek => sessions2::PlayerState::Playing,
            avrcp::PlaybackStatus::RevSeek => sessions2::PlayerState::Playing,
            avrcp::PlaybackStatus::Error => sessions2::PlayerState::Idle,
        };
    }

    /// Sets the TimelineFunction relating the current state and position given.
    /// Sets the function to None if there is no correlation.
    fn set_position(&mut self, position_millis: i64) {
        let subject_delta = match self.player_state {
            sessions2::PlayerState::Playing => 1,
            sessions2::PlayerState::Paused => 0,
            _ => {
                self.timeline_function = None;
                return;
            }
        };
        self.timeline_function = Some(media::TimelineFunction {
            subject_time: zx::MonotonicDuration::from_millis(position_millis).into_nanos(),
            reference_time: fasync::MonotonicInstant::now().into_nanos(),
            subject_delta,
            reference_delta: 1,
        });
    }
}

pub(crate) struct AvrcpRelay {
    /// Whether the battery monitoring service is active.
    battery_watcher_active: inspect::BoolProperty,
    /// The last 5 Media Player requests that have been received.
    recent_player_requests: BoundedListNode,
    /// The Inspect node associated with the current player state.
    player_status_node: inspect::Node,
    /// Inspect node
    inspect_node: inspect::Node,
}

impl Inspect for &mut AvrcpRelay {
    fn iattach(self, parent: &inspect::Node, name: impl AsRef<str>) -> Result<(), AttachError> {
        self.inspect_node = parent.create_child(name.as_ref());
        self.battery_watcher_active =
            self.inspect_node.create_bool("battery_watcher_active", false);
        self.recent_player_requests =
            BoundedListNode::new(self.inspect_node.create_child("recent_player_requests"), 5);
        self.player_status_node = self.inspect_node.create_child("player_status");
        Ok(())
    }
}

impl Default for AvrcpRelay {
    fn default() -> Self {
        Self {
            battery_watcher_active: Default::default(),
            recent_player_requests: BoundedListNode::new(inspect::Node::default(), 5),
            player_status_node: Default::default(),
            inspect_node: Default::default(),
        }
    }
}

impl AvrcpRelay {
    fn update_player_status_inspect(&mut self, latest: &ValidPlayerStatus) {
        self.player_status_node.clear_recorded();
        self.player_status_node
            .record_string("player_state", &format!("{:?}", latest.player_state));
        self.player_status_node.record_string("repeat_mode", &format!("{:?}", latest.repeat_mode));
        self.player_status_node.record_bool("shuffle_on", latest.shuffle_on);
        self.player_status_node
            .record_string("content_type", &format!("{:?}", latest.content_type));
        self.player_status_node.record_int("duration", latest.duration.unwrap_or(0));
        let Some(timeline_fn) = latest.timeline_function else { return };
        self.player_status_node.record_child("timeline_function", |node| {
            node.record_int("subject_time", timeline_fn.subject_time);
            node.record_int("reference_time", timeline_fn.reference_time);
            node.record_uint("subject_delta", timeline_fn.subject_delta.into());
            node.record_uint("reference_delta", timeline_fn.reference_delta.into());
        });
    }

    fn record_recent_player_request(&mut self, request: &sessions2::PlayerRequest) {
        // We don't record the WatchInfoChange player request since it is a noisy hanging-get
        // request.
        if !matches!(request, sessions2::PlayerRequest::WatchInfoChange { .. }) {
            inspect_log!(self.recent_player_requests, request: request.method_name());
        }
    }

    /// Start a relay between AVRCP and MediaSession.
    /// A MediaSession is published with the information from the AVRCP target.
    /// This starts the relay.  The relay can be stopped by dropping it.
    pub(crate) fn start(
        self,
        peer_id: PeerId,
        player_request_stream: sessions2::PlayerRequestStream,
    ) -> Result<fasync::Task<()>, Error> {
        let avrcp_svc =
            fuchsia_component::client::connect_to_protocol::<avrcp::PeerManagerMarker>()
                .context("Failed to connect to Bluetooth AVRCP interface")?;
        let battery_client = BatteryClient::create().ok().into();
        let session_fut =
            self.session_relay(avrcp_svc, peer_id, player_request_stream, battery_client);
        Ok(fasync::Task::spawn(async move {
            if let Err(e) = session_fut.await {
                info!(e:?; "session completed");
            }
        }))
    }

    async fn session_relay(
        mut self,
        mut avrcp: avrcp::PeerManagerProxy,
        peer_id: PeerId,
        mut player_request_stream: sessions2::PlayerRequestStream,
        mut battery_client: MaybeStream<BatteryClient>,
    ) -> Result<(), Error> {
        let (controller, browse_controller) =
            connect_avrcp(&mut avrcp, peer_id).await.context("getting controller from AVRCP")?;

        let mut staged_info = Some(sessions2::PlayerInfoDelta {
            local: Some(true),
            player_capabilities: Some(sessions2::PlayerCapabilities {
                flags: Some(
                    sessions2::PlayerCapabilityFlags::PLAY
                        | sessions2::PlayerCapabilityFlags::PAUSE
                        | sessions2::PlayerCapabilityFlags::CHANGE_TO_NEXT_ITEM
                        | sessions2::PlayerCapabilityFlags::CHANGE_TO_PREV_ITEM,
                ),
                ..Default::default()
            }),
            ..Default::default()
        });

        let mut hanging_watcher = None;

        let mut last_player_status = ValidPlayerStatus {
            player_state: sessions2::PlayerState::Idle,
            repeat_mode: sessions2::RepeatMode::Single,
            shuffle_on: false,
            content_type: sessions2::ContentType::Audio,
            duration: None,
            timeline_function: None,
            error: None,
        };

        // Get the initial Media Attributes and status
        let mut building = staged_info.as_mut().unwrap();
        // TODO(https://fxbug.dev/42119160): sometimes these initially fail
        // with "Protocol Error" or because we don't get any response from
        // the peer.  Avoid exiting the relay for now.
        if let Err(e) = update_attributes(&controller, &mut building, &mut last_player_status)
            .on_timeout(INITIAL_AVRCP_RESPONSE_WAIT_TIME, || Err(Error::msg("timed out")))
            .await
        {
            info!(e:%, peer_id:%; "Failed to update initial attributes");
        }
        if let Err(e) = update_status(&controller, &mut last_player_status)
            .on_timeout(INITIAL_AVRCP_RESPONSE_WAIT_TIME, || Err(Error::msg("timed out")))
            .await
        {
            info!(e:%, peer_id:%; "Failed to update initial play status");
        }
        building.player_status = Some(last_player_status.clone().into());
        self.update_player_status_inspect(&last_player_status);
        self.battery_watcher_active.set(battery_client.is_some());

        let mut avrcp_notify_stream = controller.take_event_stream();
        let mut status_update_interval =
            fasync::Interval::new(AVRCP_GET_PLAY_STATUS_POLL_INTERVAL).fuse();

        loop {
            let mut player_request_fut = player_request_stream.next();
            let mut avrcp_notify_fut = avrcp_notify_stream.next();
            let mut update_status_fut = status_update_interval.next();
            let mut battery_client_fut = battery_client.next();

            select! {
                request = player_request_fut => {
                    let Some(request) = request else {
                        trace!("Player request stream is closed, quitting AVRCP.");
                        break;
                    };
                    let request = request.context("request from player")?;
                    self.record_recent_player_request(&request);
                    match request {
                        sessions2::PlayerRequest::WatchInfoChange { responder } => {
                            if let Some(_) = hanging_watcher.take() {
                                return Err(format_err!("Concurrent watches issued: not allowed"));
                            }
                            hanging_watcher = Some(responder);
                        }
                        sessions2::PlayerRequest::Pause { .. } => {
                            let _ = controller.send_command(avrcp::AvcPanelCommand::Pause).await;
                        },
                        sessions2::PlayerRequest::Play { .. } => {
                            let _ = controller.send_command(avrcp::AvcPanelCommand::Play).await;
                        },
                        sessions2::PlayerRequest::Stop { .. } => {
                            let _ = controller.send_command(avrcp::AvcPanelCommand::Stop).await;
                        },
                        sessions2::PlayerRequest::NextItem { .. } => {
                            let _ = controller.send_command(avrcp::AvcPanelCommand::Forward).await;
                        },
                        sessions2::PlayerRequest::PrevItem { .. } => {
                            let _ = controller.send_command(avrcp::AvcPanelCommand::Backward).await;
                        },
                        sessions2::PlayerRequest::BindVolumeControl { .. } => {
                            // Drop incoming channel, we don't support this interface.
                        },
                        x => info!(peer_id:%; "unhandled player request {x:?}"),
                    }
                }
                event = avrcp_notify_fut => {
                    if event.is_none() {
                        info!(peer_id:%; "AVRCP relay stop: notification stream end");
                        break;
                    }
                    let avrcp::ControllerEvent::OnNotification { timestamp: _, notification } = event.unwrap()?;
                    trace!(peer_id:%, notification:?; "Notification from AVRCP");

                    let mut player_status_updated = false;

                    if let Some(millis) = notification.pos {
                        last_player_status.set_position(millis as i64);
                        player_status_updated = true;
                    }

                    if let Some(true) = notification.available_players_changed {
                        let res = browse_controller
                                .get_media_player_items(0, avrcp::MAX_MEDIA_PLAYER_ITEMS.into())
                                .await;
                        match res {
                            Ok(Ok(players)) => {
                                debug!(peer_id:%; "Media players: {players:?}");
                                let mut valid_players = players.iter().filter_map(|p| {
                                    // Return player if and only if it's in active playback status.
                                    use avrcp::PlaybackStatus::*;
                                    match p.playback_status {
                                        Some(Stopped) | Some(Error) | None => None,
                                        _ => Some(p),
                                    }
                                });
                                if valid_players.next().is_none() {
                                    last_player_status.player_state = sessions2::PlayerState::Idle;
                                    player_status_updated = true;
                                }
                            }
                            // Sometimes, browse connection is not yet established when we make the call.
                            Ok(Err(avrcp::BrowseControllerError::RemoteNotConnected)) => debug!(peer_id:%; "Couldn't get media player items because browse connection isn't established"),
                            e => info!(peer_id:%, e:?; "Error checking available player"),
                        }
                    }

                    if notification.status.is_some() ||
                        notification.track_id.is_some() ||
                        notification.addressed_player.is_some() {
                        let mut building = staged_info.get_or_insert(sessions2::PlayerInfoDelta::default());
                        if let Err(e) = update_attributes(&controller, &mut building, &mut last_player_status).await {
                            info!(peer_id:%, e:?; "Couldn't update AVRCP attributes");
                        }
                        player_status_updated = true;

                    }

                    if notification.status.is_some() {
                        if let Err(e) = update_status(&controller, &mut last_player_status).await {
                            info!(peer_id:%, e:?; "Error updating AVRCP status (notification)");
                        }
                        player_status_updated = true;
                    }

                    if player_status_updated {
                        let building = staged_info.get_or_insert(sessions2::PlayerInfoDelta::default());
                        building.player_status = Some(last_player_status.clone().into());
                        debug!(peer_id:%, building:?; "Updated player status");
                        self.update_player_status_inspect(&last_player_status);
                    }

                    // Notify that the notification is handled so we can receive another one.
                    let _ = controller.notify_notification_handled().context("acknowledging notification")?;
                }
                _event = update_status_fut => {
                    if let Err(e) = update_status(&controller, &mut last_player_status).await {
                        info!(peer_id:%, e:?; "Error updating AVRCP status (interval)");
                    }
                    let building = staged_info.get_or_insert(sessions2::PlayerInfoDelta::default());
                    building.player_status = Some(last_player_status.clone().into());
                    self.update_player_status_inspect(&last_player_status);
                }
                update = battery_client_fut => {
                    match update {
                        None => {
                            debug!(peer_id:%; "BatteryClient finished");
                            self.battery_watcher_active.set(false);
                        }
                        Some(Err(e)) => info!(peer_id:%, e:?; "BatteryClient stream error"),
                        Some(Ok(info)) => {
                            if let Err(e) = update_battery_status(&controller, info).await {
                                info!(peer_id:%, e:?; "Error updating AVRCP battery status");
                            }
                        }
                    }
                }
                complete => unreachable!(),
            }

            if staged_info.is_some() && hanging_watcher.is_some() {
                trace!("Sending watcher info: {:?}", staged_info.as_ref().unwrap());
                hanging_watcher.take().unwrap().send(&staged_info.take().unwrap())?;
            }
        }
        Ok(())
    }
}

async fn update_attributes(
    controller: &avrcp::ControllerProxy,
    info_delta: &mut sessions2::PlayerInfoDelta,
    status: &mut ValidPlayerStatus,
) -> Result<(), Error> {
    let attributes = controller
        .get_media_attributes()
        .await?
        .or_else(|e| Err(format_err!("AVRCP error: {e:?}")))?;
    info_delta.metadata = Some(attributes_to_metadata(&attributes));

    if let Some(playing_time) = attributes.playing_time {
        if let Ok(millis) = playing_time.parse::<i64>() {
            status.duration = Some(zx::MonotonicDuration::from_millis(millis).into_nanos());
        }
    }
    Ok(())
}

async fn update_status(
    controller: &avrcp::ControllerProxy,
    status: &mut ValidPlayerStatus,
) -> Result<(), Error> {
    let avrcp_status =
        controller.get_play_status().await?.or_else(|e| Err(format_err!("AVRCP error: {e:?}")))?;
    let playback_status = avrcp_status
        .playback_status
        .ok_or_else(|| format_err!("PlayStatus must have playback status"))?;
    status.set_state_from_avrcp(playback_status);
    status.duration =
        avrcp_status.song_length.map(|m| zx::MonotonicDuration::from_millis(m as i64).into_nanos());
    let _ = avrcp_status.song_position.map(|m| status.set_position(m as i64));
    Ok(())
}

async fn update_battery_status(
    controller: &avrcp::ControllerProxy,
    status: BatteryInfo,
) -> Result<(), Error> {
    let avrcp_status = match status {
        BatteryInfo::External => avrcp::BatteryStatus::External,
        BatteryInfo::Battery(BatteryLevel::Normal(_)) => avrcp::BatteryStatus::Normal,
        BatteryInfo::Battery(BatteryLevel::Warning(_)) => avrcp::BatteryStatus::Warning,
        BatteryInfo::Battery(BatteryLevel::Critical(_)) => avrcp::BatteryStatus::Critical,
        BatteryInfo::Battery(BatteryLevel::FullCharge) => avrcp::BatteryStatus::FullCharge,
        BatteryInfo::NotAvailable => return Ok(()),
    };
    controller
        .inform_battery_status(avrcp_status)
        .await?
        .or_else(|e| Err(format_err!("AVRCP error: {e:?}")))
}

macro_rules! nonempty_to_property {
    ( $source:expr, $prop_str:expr, $target:ident ) => {
        if let Some(value) = $source {
            $target.push(media::Property { label: $prop_str.to_string(), value: value.clone() });
        }
    };
}

fn attributes_to_metadata(attributes: &avrcp::MediaAttributes) -> media::Metadata {
    let mut properties = Vec::new();
    nonempty_to_property!(&attributes.title, media::METADATA_LABEL_TITLE, properties);
    nonempty_to_property!(&attributes.artist_name, media::METADATA_LABEL_ARTIST, properties);
    nonempty_to_property!(&attributes.album_name, media::METADATA_LABEL_ALBUM, properties);
    nonempty_to_property!(&attributes.track_number, media::METADATA_LABEL_TRACK_NUMBER, properties);
    nonempty_to_property!(&attributes.total_number_of_tracks, "total_number_of_tracks", properties);
    nonempty_to_property!(&attributes.genre, media::METADATA_LABEL_GENRE, properties);
    nonempty_to_property!(
        &Some(String::from("Bluetooth")),
        media::METADATA_SOURCE_TITLE,
        properties
    );
    media::Metadata { properties }
}

async fn connect_avrcp(
    avrcp: &mut avrcp::PeerManagerProxy,
    peer_id: PeerId,
) -> Result<(avrcp::ControllerProxy, avrcp::BrowseControllerProxy), Error> {
    let (controller, server) = endpoints::create_proxy();
    let (browse_controller, browse_server) = endpoints::create_proxy();

    let _ = avrcp.get_controller_for_target(&peer_id.into(), server).await?;
    let _ = avrcp.get_browse_controller_for_target(&peer_id.into(), browse_server).await?;

    controller.set_notification_filter(
        avrcp::Notifications::PLAYBACK_STATUS
            | avrcp::Notifications::TRACK
            | avrcp::Notifications::CONNECTION
            | avrcp::Notifications::AVAILABLE_PLAYERS
            | avrcp::Notifications::ADDRESSED_PLAYER,
        0,
    )?;

    Ok((controller, browse_controller))
}

#[cfg(test)]
mod tests {
    use super::*;

    use assert_matches::assert_matches;
    use async_test_helpers::run_while;
    use async_utils::PollExt;
    use diagnostics_assertions::{assert_data_tree, AnyProperty};
    use fidl::endpoints::RequestStream;
    use fidl_fuchsia_power_battery as fpower;
    use fuchsia_async::DurationExt;
    use fuchsia_inspect_derive::WithInspect;
    use futures::task::Poll;
    use futures::Future;
    use std::pin::{pin, Pin};
    use test_battery_manager::TestBatteryManager;

    fn setup_media_relay() -> (sessions2::PlayerProxy, avrcp::PeerManagerRequestStream, impl Future)
    {
        let (player_proxy, player_requests) =
            endpoints::create_proxy_and_stream::<sessions2::PlayerMarker>();
        let (avrcp_proxy, avrcp_requests) =
            endpoints::create_proxy_and_stream::<avrcp::PeerManagerMarker>();
        let peer_id = PeerId(0);

        let relay = AvrcpRelay::default();
        let relay_fut = relay.session_relay(avrcp_proxy, peer_id, player_requests, None.into());
        (player_proxy, avrcp_requests, relay_fut)
    }

    fn setup_media_relay_with_battery_manager(
        exec: &mut fasync::TestExecutor,
    ) -> (sessions2::PlayerProxy, avrcp::PeerManagerRequestStream, impl Future, TestBatteryManager)
    {
        let (player_proxy, player_requests) =
            endpoints::create_proxy_and_stream::<sessions2::PlayerMarker>();
        let (avrcp_proxy, avrcp_requests) =
            endpoints::create_proxy_and_stream::<avrcp::PeerManagerMarker>();
        let peer_id = PeerId(1);

        let mut setup_fut = pin!(TestBatteryManager::make_battery_client_with_test_manager());
        let (battery_client, test_battery_manager) = exec.run_singlethreaded(&mut setup_fut);

        let relay = AvrcpRelay::default();
        let relay_fut =
            relay.session_relay(avrcp_proxy, peer_id, player_requests, Some(battery_client).into());

        (player_proxy, avrcp_requests, relay_fut, test_battery_manager)
    }

    #[track_caller]
    fn expect_media_attributes_request(
        exec: &mut fasync::TestExecutor,
        controller_requests: &mut avrcp::ControllerRequestStream,
    ) {
        // Should ask for the current media info and the status to return the correct results.
        match exec.run_until_stalled(&mut controller_requests.next()).expect("should be ready") {
            Some(Ok(avrcp::ControllerRequest::GetMediaAttributes { responder })) => {
                responder
                    .send(Ok(&avrcp::MediaAttributes {
                        title: Some("Might Be Right".to_string()),
                        artist_name: Some("White Reaper".to_string()),
                        album_name: Some("You Deserve Love".to_string()),
                        track_number: Some("7".to_string()),
                        total_number_of_tracks: Some("10".to_string()),
                        genre: Some("Alternative".to_string()),
                        playing_time: Some("237000".to_string()),
                        ..Default::default()
                    }))
                    .expect("should have succeeded");
            }
            x => panic!("Expected a GetMediaAttributes request, got {:?}", x),
        }
    }

    #[track_caller]
    fn expect_play_status_request(
        exec: &mut fasync::TestExecutor,
        controller_requests: &mut avrcp::ControllerRequestStream,
    ) {
        match exec.run_until_stalled(&mut controller_requests.next()).expect("should be ready") {
            Some(Ok(avrcp::ControllerRequest::GetPlayStatus { responder })) => {
                responder
                    .send(Ok(&avrcp::PlayStatus {
                        song_length: Some(237000),
                        song_position: Some(1000),
                        playback_status: Some(avrcp::PlaybackStatus::Playing),
                        ..Default::default()
                    }))
                    .expect("should have succeeded");
            }
            x => panic!("Expected a GetPlayStatus request, got {:?}", x),
        }
    }

    #[track_caller]
    fn setup_avrcp(
        mut relay_fut: &mut Pin<&mut impl Future>,
        exec: &mut fasync::TestExecutor,
        avrcp_request_stream: avrcp::PeerManagerRequestStream,
    ) -> (avrcp::ControllerRequestStream, avrcp::BrowseControllerRequestStream) {
        let mut avrcp_request_stream = pin!(avrcp_request_stream);
        // Connects to AVRCP.
        let mut controller_request_stream = match exec
            .run_until_stalled(&mut avrcp_request_stream.select_next_some())
        {
            Poll::Ready(Ok(avrcp::PeerManagerRequest::GetControllerForTarget {
                client,
                responder,
                ..
            })) => responder.send(Ok(())).map(|_| client.into_stream()).expect("should have sent"),
            x => panic!("Expected a GetController request, got {:?}", x),
        };

        // Finish serving GetControllerForTarget.
        exec.run_until_stalled(&mut relay_fut).expect_pending("should be pending");

        let browse_controller_request_stream = match exec
            .run_until_stalled(&mut avrcp_request_stream.select_next_some())
        {
            Poll::Ready(Ok(avrcp::PeerManagerRequest::GetBrowseControllerForTarget {
                client,
                responder,
                ..
            })) => responder.send(Ok(())).map(|_| client.into_stream()).expect("should have sent"),
            x => panic!("Expected a GetBrowseController request, got {:?}", x),
        };

        // Finish serving GetBrowseControllerForTarget.
        let res = exec.run_until_stalled(&mut relay_fut);
        assert!(res.is_pending());

        let complete = exec.run_until_stalled(&mut controller_request_stream.select_next_some());
        match complete {
            Poll::Ready(Ok(avrcp::ControllerRequest::SetNotificationFilter { .. })) => {}
            x => panic!("Expected notifications to be set, got {:?}", x),
        };

        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        (controller_request_stream, browse_controller_request_stream)
    }

    #[track_caller]
    fn finish_relay_setup(
        mut relay_fut: &mut Pin<&mut impl Future>,
        mut exec: &mut fasync::TestExecutor,
        avrcp_request_stream: avrcp::PeerManagerRequestStream,
    ) -> (avrcp::ControllerRequestStream, avrcp::BrowseControllerRequestStream) {
        let (mut controller_request_stream, browse_controller_request_stream) =
            setup_avrcp(relay_fut, exec, avrcp_request_stream);

        expect_media_attributes_request(&mut exec, &mut controller_request_stream);
        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        expect_play_status_request(&mut exec, &mut controller_request_stream);
        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        // Expect play status timer interval request.
        let _ = exec.wake_next_timer();
        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());
        expect_play_status_request(&mut exec, &mut controller_request_stream);
        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        // At this point, the relay is set up and should be waiting for requests from
        // player_client, and sending commands / getting notifications from avrcp.
        (controller_request_stream, browse_controller_request_stream)
    }

    fn run_to_stalled(exec: &mut fasync::TestExecutor) {
        let _ = exec.run_until_stalled(&mut futures::future::pending::<()>());
    }

    /// Test that the relay is set up even when the peer does not respond to initial AVRCP
    /// requests for media attributes and play status.
    #[fuchsia::test]
    fn test_finish_relay_setup_hanging_avrcp_requests() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(7000));

        let (player_client, avrcp_requests, relay_fut) = setup_media_relay();

        let mut relay_fut = pin!(relay_fut);

        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        let (mut controller_request_stream, _browse_controller_request_stream) =
            setup_avrcp(&mut relay_fut, &mut exec, avrcp_requests);

        // We should have sent an initial get media attributes request to the peer.
        let _request = exec
            .run_until_stalled(&mut controller_request_stream.next())
            .expect("should be ready")
            .expect("should be some")
            .expect("should be ok");

        // Imitate peer ignoring our request.
        // Advance time past the max amount of time we would wait for a response back.
        exec.set_fake_time(
            (INITIAL_AVRCP_RESPONSE_WAIT_TIME + MonotonicDuration::from_micros(10)).after_now(),
        );
        let _ = exec.wake_expired_timers();
        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        // We should have sent an initial get status request to the peer.
        let _request = exec
            .run_until_stalled(&mut controller_request_stream.next())
            .expect("should be ready")
            .expect("should be some")
            .expect("should be ok");

        // Imitate peer ignoring our request.
        // Advance time past the max amount of time we would wait for a response back.
        exec.set_fake_time(
            (INITIAL_AVRCP_RESPONSE_WAIT_TIME + MonotonicDuration::from_micros(10)).after_now(),
        );
        let _ = exec.wake_expired_timers();
        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        // Even when the initial AVRCP requests were not answered by the peer, we should
        // be able to publish the media session.
        // We should have sent an initial get play status request to the peer.
        let player_info_fut = pin!(player_client.watch_info_change());
        let (res, _relay_fut) = run_while(&mut exec, relay_fut, player_info_fut);
        let player = res.expect("should have published a player");
        assert_eq!(
            player.player_status.expect("default status should exist").player_state,
            Some(sessions2::PlayerState::Idle)
        );
    }

    /// Test that the relay sets up the connection to AVRCP and Sessions and stops on the stop
    /// signal.
    #[fuchsia::test]
    fn test_relay_setup() {
        let mut exec = fasync::TestExecutor::new();

        let (player_client, avrcp_requests, relay_fut) = setup_media_relay();
        let request_streams;

        {
            let mut relay_fut = pin!(relay_fut);

            let res = exec.run_until_stalled(&mut relay_fut);
            assert!(res.is_pending());

            request_streams = finish_relay_setup(&mut relay_fut, &mut exec, avrcp_requests);

            // Dropping the relay future drops all the connections.
        }
        run_to_stalled(&mut exec);

        let mut controller_requests = request_streams.0;

        match exec.run_until_stalled(&mut controller_requests.next()) {
            Poll::Ready(None) => {}
            x => panic!("Expected controller to be dropped, but got {:?}", x),
        };

        let mut watch_info_fut = player_client.watch_info_change();
        match exec.run_until_stalled(&mut watch_info_fut) {
            Poll::Ready(Err(_e)) => {}
            x => panic!("Expected player to be disconnected, but got {:?} from watch_info", x),
        };
    }

    /// Relay will stop when AVRCP closes the notification channel.
    #[fuchsia::test]
    fn test_relay_avrcp_ends() {
        let mut exec = fasync::TestExecutor::new();

        let (player_client, avrcp_requests, relay_fut) = setup_media_relay();

        let mut relay_fut = pin!(relay_fut);

        let res = exec.run_until_stalled(&mut relay_fut);
        assert!(res.is_pending());

        let (controller_requests, _browse_controller_requests) =
            finish_relay_setup(&mut relay_fut, &mut exec, avrcp_requests);

        // Closing the AVRCP controller should end the relay.
        drop(controller_requests);

        let res = exec.run_until_stalled(&mut relay_fut);
        assert!(res.is_ready());

        // The MediaSession should also drop based on this.
        let mut watch_info_fut = player_client.watch_info_change();
        match exec.run_until_stalled(&mut watch_info_fut) {
            Poll::Ready(Err(_e)) => {}
            x => panic!("Expected player to be disconnected, but got {:?} from watch_info", x),
        };
    }

    /// Relay will stop when Player stops asking for updates.
    #[fuchsia::test]
    fn test_relay_player_ends() {
        let mut exec = fasync::TestExecutor::new();

        let (player_client, avrcp_requests, relay_fut) = setup_media_relay();

        let mut relay_fut = pin!(relay_fut);

        let res = exec.run_until_stalled(&mut relay_fut);
        assert!(res.is_pending());

        let (mut controller_requests, _browse_controller_requests) =
            finish_relay_setup(&mut relay_fut, &mut exec, avrcp_requests);

        // Closing of the MediaSession should end the relay.
        drop(player_client);

        let res = exec.run_until_stalled(&mut relay_fut);
        assert!(res.is_ready());

        match exec.run_until_stalled(&mut controller_requests.next()) {
            Poll::Ready(None) => {}
            x => panic!("Expected controller to be dropped, but got {:?}", x),
        };
    }

    /// When mediasession initially asks for media info, a query of the remote AVRCP is made and
    /// the data is translated.
    #[fuchsia::test]
    fn test_relay_sends_correct_media_info() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(7000));

        let (player_client, avrcp_requests, relay_fut) = setup_media_relay();

        let mut relay_fut = pin!(relay_fut);

        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        let _request_streams = finish_relay_setup(&mut relay_fut, &mut exec, avrcp_requests);

        let mut watch_info_fut = player_client.watch_info_change();

        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        // Should return to the player the initial data.
        let info_delta = match exec.run_until_stalled(&mut watch_info_fut) {
            Poll::Ready(Ok(delta)) => delta,
            x => panic!("Expected WatchInfoChange to complete, instead: {:?}", x),
        };

        assert_eq!(
            ValidPlayerStatus {
                duration: Some(237000_000_000),
                player_state: sessions2::PlayerState::Playing,
                timeline_function: Some(media::TimelineFunction {
                    subject_time: 1000_000_000,
                    reference_time: 7000,
                    subject_delta: 1,
                    reference_delta: 1,
                }),
                repeat_mode: sessions2::RepeatMode::Single,
                shuffle_on: false,
                content_type: sessions2::ContentType::Audio,
                error: None
            },
            info_delta.player_status.unwrap().try_into().expect("valid player status")
        );
    }

    /// When playback status changes the new track info is sent to the Player client.
    #[fuchsia::test]
    fn test_relay_new_avrcp_track_info() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(7000));

        let (player_client, avrcp_requests, relay_fut) = setup_media_relay();

        let mut relay_fut = pin!(relay_fut);

        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        let (mut controller_requests, _browse_controller_requests) =
            finish_relay_setup(&mut relay_fut, &mut exec, avrcp_requests);

        let mut watch_info_fut = player_client.watch_info_change();

        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        // Should return to the player the initial data.
        let _info_delta = match exec.run_until_stalled(&mut watch_info_fut) {
            Poll::Ready(Ok(delta)) => delta,
            x => panic!("Expected WatchInfoChange to complete, instead: {:?}", x),
        };

        // Queueing up another one with no change should just hang.
        let mut watch_info_fut = player_client.watch_info_change();

        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());
        assert!(exec.run_until_stalled(&mut watch_info_fut).is_pending());

        // When a play status change notification happens, we get new requests.
        controller_requests
            .control_handle()
            .send_on_notification(
                7000,
                &avrcp::Notification {
                    status: Some(avrcp::PlaybackStatus::Paused),
                    ..Default::default()
                },
            )
            .expect("should have sent");

        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        // Should ask for the current media info and the status to return the correct results.
        match exec.run_until_stalled(&mut controller_requests.next()) {
            Poll::Ready(Some(Ok(avrcp::ControllerRequest::GetMediaAttributes { responder }))) => {
                responder
                    .send(Ok(&avrcp::MediaAttributes {
                        title: Some("Moneygrabber".to_string()),
                        artist_name: Some("Fitz and the Tantrums".to_string()),
                        album_name: Some("Pickin' Up the Pieces".to_string()),
                        track_number: Some("4".to_string()),
                        total_number_of_tracks: Some("11".to_string()),
                        genre: Some("Alternative".to_string()),
                        playing_time: Some("189000".to_string()),
                        ..Default::default()
                    }))
                    .expect("should have sent");
            }
            x => panic!("Expected a GetMediaAttributes request, got {:?}", x),
        }
        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        match exec.run_until_stalled(&mut controller_requests.next()) {
            Poll::Ready(Some(Ok(avrcp::ControllerRequest::GetPlayStatus { responder }))) => {
                responder
                    .send(Ok(&avrcp::PlayStatus {
                        song_length: Some(189000),
                        song_position: Some(1000),
                        playback_status: Some(avrcp::PlaybackStatus::Paused),
                        ..Default::default()
                    }))
                    .expect("should have sent");
            }
            x => panic!("Expected a GetPlayStatus request, got {:?}", x),
        };
        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        // After the AVRCP requests, the info should have the delta.
        let info_delta = match exec.run_until_stalled(&mut watch_info_fut) {
            Poll::Ready(Ok(delta)) => delta,
            x => panic!("Expected WatchInfoChange to complete, instead: {:?}", x),
        };

        // After the notification is handled we should get an ack.
        match exec.run_until_stalled(&mut controller_requests.next()) {
            Poll::Ready(Some(Ok(avrcp::ControllerRequest::NotifyNotificationHandled {
                ..
            }))) => {}
            x => panic!("Expected ack of notification, but got {:?}", x),
        };

        assert_eq!(
            ValidPlayerStatus {
                duration: Some(189000_000_000),
                player_state: sessions2::PlayerState::Paused,
                timeline_function: Some(media::TimelineFunction {
                    subject_time: 1000_000_000,
                    reference_time: 7000,
                    subject_delta: 0,
                    reference_delta: 1,
                }),
                repeat_mode: sessions2::RepeatMode::Single,
                shuffle_on: false,
                content_type: sessions2::ContentType::Audio,
                error: None
            },
            info_delta.player_status.unwrap().try_into().expect("valid player status")
        );

        // When addressed player change notification happens, we also get
        // attributes.
        controller_requests
            .control_handle()
            .send_on_notification(
                7000,
                &avrcp::Notification { addressed_player: Some(2), ..Default::default() },
            )
            .expect("should have sent");

        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        // Should ask for the current media info and the status to return the correct results.
        match exec.run_until_stalled(&mut controller_requests.next()) {
            Poll::Ready(Some(Ok(avrcp::ControllerRequest::GetMediaAttributes { responder }))) => {
                responder
                    .send(Ok(&avrcp::MediaAttributes {
                        title: Some("some track".to_string()),
                        ..Default::default()
                    }))
                    .expect("should have sent");
            }
            x => panic!("Expected a GetMediaAttributes request, got {:?}", x),
        }
        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());
    }

    /// When the position update happens, the new position is updated for the Player.
    #[fuchsia::test]
    fn test_relay_updates_position() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(7000));

        let (player_client, avrcp_requests, relay_fut) = setup_media_relay();

        let mut relay_fut = pin!(relay_fut);

        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        let (mut controller_requests, _browse_controller_requests) =
            finish_relay_setup(&mut relay_fut, &mut exec, avrcp_requests);

        let mut watch_info_fut = player_client.watch_info_change();

        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        // Should return to the player the initial data.
        let _info_delta = match exec.run_until_stalled(&mut watch_info_fut) {
            Poll::Ready(Ok(delta)) => delta,
            x => panic!("Expected WatchInfoChange to complete, instead: {:?}", x),
        };

        // Queueing up another one with no change should just hang.
        let mut watch_info_fut = player_client.watch_info_change();

        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());
        assert!(exec.run_until_stalled(&mut watch_info_fut).is_pending());

        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(9000));

        // When a play status change notification happens, we get new requests.
        controller_requests
            .control_handle()
            .send_on_notification(
                9000,
                &avrcp::Notification { pos: Some(3051), ..Default::default() },
            )
            .expect("should have sent");

        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        // After the notification is handled we should get an ack.
        match exec.run_until_stalled(&mut controller_requests.next()) {
            Poll::Ready(Some(Ok(avrcp::ControllerRequest::NotifyNotificationHandled {
                ..
            }))) => {}
            x => panic!("Expected ack of notification, but got {:?}", x),
        };

        // The info should have the delta.
        let info_delta = match exec.run_until_stalled(&mut watch_info_fut) {
            Poll::Ready(Ok(delta)) => delta,
            x => panic!("Expected WatchInfoChange to complete, instead: {:?}", x),
        };

        assert_eq!(
            ValidPlayerStatus {
                duration: Some(237000_000_000),
                player_state: sessions2::PlayerState::Playing,
                timeline_function: Some(media::TimelineFunction {
                    subject_time: 3051_000_000,
                    reference_time: 9000,
                    subject_delta: 1,
                    reference_delta: 1,
                }),
                repeat_mode: sessions2::RepeatMode::Single,
                shuffle_on: false,
                content_type: sessions2::ContentType::Audio,
                error: None
            },
            info_delta.player_status.unwrap().try_into().expect("valid player status")
        );
    }

    fn expect_panel_command(
        exec: &mut fasync::TestExecutor,
        controller_requests: &mut avrcp::ControllerRequestStream,
        expected_command: avrcp::AvcPanelCommand,
    ) {
        match exec.run_until_stalled(&mut controller_requests.next()) {
            Poll::Ready(Some(Ok(avrcp::ControllerRequest::SendCommand { command, responder }))) => {
                assert_eq!(expected_command, command);
                responder.send(Ok(())).expect("should have sent");
            }
            x => panic!("Expected a SendCommand({:?}) request, got {:?}", expected_command, x),
        }
    }

    /// When commands come from the Player, they are relayed to the AVRCP commands.
    #[fuchsia::test]
    fn test_relay_sends_commands() {
        let mut exec = fasync::TestExecutor::new();

        let (player_client, avrcp_requests, relay_fut) = setup_media_relay();

        let mut relay_fut = pin!(relay_fut);

        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        let (mut controller_requests, _browse_controller_requests) =
            finish_relay_setup(&mut relay_fut, &mut exec, avrcp_requests);

        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        player_client.pause().expect("should have been done");
        player_client.play().expect("should have been done");
        player_client.stop().expect("should have been done");
        player_client.next_item().expect("should have been done");
        player_client.prev_item().expect("should have been done");

        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());
        expect_panel_command(&mut exec, &mut controller_requests, avrcp::AvcPanelCommand::Pause);
        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());
        expect_panel_command(&mut exec, &mut controller_requests, avrcp::AvcPanelCommand::Play);
        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());
        expect_panel_command(&mut exec, &mut controller_requests, avrcp::AvcPanelCommand::Stop);
        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());
        expect_panel_command(&mut exec, &mut controller_requests, avrcp::AvcPanelCommand::Forward);
        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());
        expect_panel_command(&mut exec, &mut controller_requests, avrcp::AvcPanelCommand::Backward);
    }

    fn expect_inform_battery_status_command(
        exec: &mut fasync::TestExecutor,
        controller_requests: &mut avrcp::ControllerRequestStream,
        expected_battery_status: avrcp::BatteryStatus,
    ) {
        match exec.run_until_stalled(&mut controller_requests.next()) {
            Poll::Ready(Some(Ok(avrcp::ControllerRequest::InformBatteryStatus {
                battery_status,
                responder,
            }))) => {
                assert_eq!(battery_status, expected_battery_status);
                responder.send(Ok(())).expect("can respond to client");
            }
            x => panic!("Expected a InformBatteryStatus request, got {:?}", x),
        }
    }

    #[fuchsia::test]
    fn relay_sends_battery_update_to_avrcp() {
        let mut exec = fasync::TestExecutor::new();

        let (_player_client, avrcp_requests, relay_fut, test_battery_manager) =
            setup_media_relay_with_battery_manager(&mut exec);
        let mut relay_fut = pin!(relay_fut);
        exec.run_until_stalled(&mut relay_fut).expect_pending("relay fut still running");

        let (mut controller_requests, _browse_controller_requests) =
            finish_relay_setup(&mut relay_fut, &mut exec, avrcp_requests);
        exec.run_until_stalled(&mut relay_fut).expect_pending("relay fut still running");

        // Simulate a battery update via the TestBatteryManager.
        let update = fpower::BatteryInfo {
            status: Some(fpower::BatteryStatus::Ok),
            level_status: Some(fpower::LevelStatus::Low),
            level_percent: Some(33f32),
            ..Default::default()
        };
        let update_fut = pin!(test_battery_manager.send_update(update));
        let (res, mut relay_fut) = run_while(&mut exec, relay_fut, update_fut);
        assert_matches!(res, Ok(_));

        expect_inform_battery_status_command(
            &mut exec,
            &mut controller_requests,
            avrcp::BatteryStatus::Normal,
        );
        exec.run_until_stalled(&mut relay_fut).expect_pending("relay fut still running");
    }

    #[fuchsia::test]
    fn not_available_battery_update_is_not_relayed_to_avrcp() {
        let mut exec = fasync::TestExecutor::new();

        let (_player_client, avrcp_requests, relay_fut, test_battery_manager) =
            setup_media_relay_with_battery_manager(&mut exec);
        let mut relay_fut = pin!(relay_fut);
        exec.run_until_stalled(&mut relay_fut).expect_pending("relay fut still running");

        let (mut controller_requests, _browse_controller_requests) =
            finish_relay_setup(&mut relay_fut, &mut exec, avrcp_requests);
        exec.run_until_stalled(&mut relay_fut).expect_pending("relay fut still running");

        // Simulate a battery update via the TestBatteryManager.
        let update = fpower::BatteryInfo {
            status: Some(fpower::BatteryStatus::Unknown),
            ..Default::default()
        };
        let update_fut = pin!(test_battery_manager.send_update(update));
        let (res, mut relay_fut) = run_while(&mut exec, relay_fut, update_fut);
        assert_matches!(res, Ok(_));

        exec.run_until_stalled(&mut controller_requests.next()).expect_pending("No avrcp update");
        exec.run_until_stalled(&mut relay_fut).expect_pending("relay fut still running");
    }

    #[fuchsia::test]
    fn relay_still_active_when_battery_client_terminates() {
        let mut exec = fasync::TestExecutor::new();

        let (_player_client, avrcp_requests, relay_fut, test_battery_manager) =
            setup_media_relay_with_battery_manager(&mut exec);
        let mut relay_fut = pin!(relay_fut);
        exec.run_until_stalled(&mut relay_fut).expect_pending("relay fut still running");

        let _request_streams = finish_relay_setup(&mut relay_fut, &mut exec, avrcp_requests);
        exec.run_until_stalled(&mut relay_fut).expect_pending("relay fut still running");

        // Battery Manager server disappears. The Battery Client stream should finish and the relay
        // should be resilient.
        drop(test_battery_manager);
        exec.run_until_stalled(&mut relay_fut).expect_pending("relay fut still running");
    }

    /// When available players changes we fetch for the list of available
    /// players and change the media session player state based on the
    /// available players status.
    #[fuchsia::test]
    fn test_relay_avrcp_available_players_changed() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(7000));

        let (player_client, avrcp_requests, relay_fut) = setup_media_relay();

        let mut relay_fut = pin!(relay_fut);

        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        let (mut controller_requests, mut browse_controller_requests) =
            finish_relay_setup(&mut relay_fut, &mut exec, avrcp_requests);

        let mut watch_info_fut = player_client.watch_info_change();

        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        // Should return to the player the initial data.
        let _info_delta = match exec.run_until_stalled(&mut watch_info_fut) {
            Poll::Ready(Ok(delta)) => delta,
            x => panic!("Expected WatchInfoChange to complete, instead: {:?}", x),
        };

        // Queueing up another one with no change should just hang.
        let mut watch_info_fut = player_client.watch_info_change();

        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());
        assert!(exec.run_until_stalled(&mut watch_info_fut).is_pending());

        // When a play status change notification happens, we get new requests.
        controller_requests
            .control_handle()
            .send_on_notification(
                7000,
                &avrcp::Notification {
                    available_players_changed: Some(true),
                    ..Default::default()
                },
            )
            .expect("should have sent");

        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        // Should ask for the list of media players.
        match exec.run_until_stalled(&mut browse_controller_requests.next()) {
            Poll::Ready(Some(Ok(avrcp::BrowseControllerRequest::GetMediaPlayerItems {
                responder,
                ..
            }))) => {
                // Only return 1 inactive player.
                responder
                    .send(Ok(&[avrcp::MediaPlayerItem {
                        player_id: Some(1),
                        playback_status: Some(avrcp::PlaybackStatus::Stopped),
                        ..Default::default()
                    }]))
                    .expect("should have sent");
            }
            x => panic!("Expected a GetMediaPlayerItems request, got {:?}", x),
        }
        assert!(exec.run_until_stalled(&mut relay_fut).is_pending());

        // After the AVRCP requests, the info should have the delta.
        let info_delta = match exec.run_until_stalled(&mut watch_info_fut) {
            Poll::Ready(Ok(delta)) => delta,
            x => panic!("Expected WatchInfoChange to complete, instead: {:?}", x),
        };

        // After the notification is handled we should get an ack.
        match exec.run_until_stalled(&mut controller_requests.next()) {
            Poll::Ready(Some(Ok(avrcp::ControllerRequest::NotifyNotificationHandled {
                ..
            }))) => {}
            x => panic!("Expected ack of notification, but got {:?}", x),
        };

        // Player should have switched to idle state.
        assert_eq!(
            info_delta.player_status.unwrap().player_state.unwrap(),
            sessions2::PlayerState::Idle
        );
    }

    #[fuchsia::test]
    fn avrcp_relay_inspect() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(7000));
        let inspector = fuchsia_inspect::Inspector::default();

        let (player_client, player_requests) =
            endpoints::create_proxy_and_stream::<sessions2::PlayerMarker>();
        let (avrcp_proxy, avrcp_requests) =
            endpoints::create_proxy_and_stream::<avrcp::PeerManagerMarker>();
        let peer_id = PeerId(0);

        let relay = AvrcpRelay::default()
            .with_inspect(inspector.root(), "avrcp_relay")
            .expect("can attach");
        let relay_fut = relay.session_relay(avrcp_proxy, peer_id, player_requests, None.into());
        let mut relay_fut = pin!(relay_fut);

        exec.run_until_stalled(&mut relay_fut).expect_pending("relay active");

        // The default Inspect tree doesn't have any player data.
        assert_data_tree!(@executor exec, inspector, root: {
            avrcp_relay: {
                battery_watcher_active: false,
                recent_player_requests: {},
                player_status: {},
            }
        });

        let (mut controller_requests, _browse_controller_requests) =
            finish_relay_setup(&mut relay_fut, &mut exec, avrcp_requests);
        // After finishing the initial relay set up, the inspect data should be updated. This data
        // is populated by `expect_media_attributes_request` and `expect_play_status_request`.
        assert_data_tree!(@executor exec, inspector, root: {
            avrcp_relay: {
                battery_watcher_active: false,
                player_status: {
                    content_type: "Audio",
                    duration: 237000000000i64,
                    repeat_mode: "Single",
                    shuffle_on: false,
                    player_state: "Playing",
                    timeline_function: {
                        subject_time: 1000000000i64,
                        reference_time: 7000i64,
                        subject_delta: 1u64,
                        reference_delta: 1u64,
                    },
                },
                recent_player_requests: {},
            }
        });
        exec.run_until_stalled(&mut relay_fut).expect_pending("relay active");

        // Receiving player commands should also update the Inspect tree.
        player_client.pause().expect("should have been done");
        player_client.play().expect("should have been done");
        // WatchInfoChange should not be recorded to the Inspect tree.
        let watch_fut = player_client.watch_info_change();
        exec.run_until_stalled(&mut relay_fut).expect_pending("relay active");
        expect_panel_command(&mut exec, &mut controller_requests, avrcp::AvcPanelCommand::Pause);
        exec.run_until_stalled(&mut relay_fut).expect_pending("relay active");
        expect_panel_command(&mut exec, &mut controller_requests, avrcp::AvcPanelCommand::Play);
        exec.run_until_stalled(&mut relay_fut).expect_pending("relay active");
        let (_delta, _relay_fut) = run_while(&mut exec, relay_fut, watch_fut);
        assert_data_tree!(@executor exec, inspector, root: {
            avrcp_relay: contains {
                recent_player_requests: {
                    "0": { "@time": AnyProperty, request: "pause" },
                    "1": { "@time": AnyProperty, request: "play" },
                },
            }
        });
    }
}
