// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(fuchsia_api_level_at_least = "HEAD")]
use ::input_pipeline::interaction_state_handler::{
    InteractionStateHandler, InteractionStatePublisher,
};
use ::input_pipeline::light_sensor::{
    Calibration as LightSensorCalibration, Configuration as LightSensorConfiguration,
    FactoryFileLoader,
};
use ::input_pipeline::light_sensor_handler::make_light_sensor_handler_and_spawn_led_watcher;
use ::input_pipeline::text_settings_handler::TextSettingsHandler;
use ::input_pipeline::CursorMessage;
use anyhow::{Context, Error};
use fidl_fuchsia_factory::MiscFactoryStoreProviderMarker;
use fidl_fuchsia_input_injection::InputDeviceRegistryRequestStream;
use fidl_fuchsia_lightsensor::SensorRequestStream as LightSensorRequestStream;
use fidl_fuchsia_recovery_policy::DeviceRequestStream;
use fidl_fuchsia_recovery_ui::FactoryResetCountdownRequestStream;
use fidl_fuchsia_ui_brightness::ControlMarker as BrightnessControlMarker;
use fidl_fuchsia_ui_pointerinjector_configuration::SetupProxy;
use fidl_fuchsia_ui_policy::DeviceListenerRegistryRequestStream;
use focus_chain_provider::FocusChainProviderPublisher;
use fsettings::LightMarker;
use fuchsia_component::client::connect_to_protocol;
use futures::lock::Mutex;
use futures::StreamExt;
use input_pipeline::factory_reset_handler::FactoryResetHandler;
use input_pipeline::ime_handler::ImeHandler;
use input_pipeline::input_pipeline::{
    InputDeviceBindingHashMap, InputPipeline, InputPipelineAssembly,
};
use input_pipeline::light_sensor_handler::CalibratedLightSensorHandler;
use input_pipeline::media_buttons_handler::MediaButtonsHandler;
use input_pipeline::mouse_injector_handler::MouseInjectorHandler;
use input_pipeline::touch_injector_handler::TouchInjectorHandler;
use input_pipeline::{dead_keys_handler, input_device, keymap_handler, metrics};
use log::{error, info, warn};
use scene_management::SceneManagerTrait;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use {fidl_fuchsia_settings as fsettings, fuchsia_async as fasync, fuchsia_inspect as inspect};

/// Begins handling input events. The returned future will complete when
/// input events are no longer being handled.
///
/// # Parameters
/// - `scene_manager`: The scene manager used by the session.
/// - `input_device_registry_request_stream_receiver`: A receiving end of a MPSC channel for
///   `InputDeviceRegistry` messages.
/// - `light_sensor_request_stream_receiver`: A receiving end of an MPSC channel for
///   `Sensor` messages.
/// - `node`: The inspect node to insert individual inspect handler nodes into.
/// - `focus_chain_publisher`: Forwards focus chain changes to downstream watchers.
/// - `light_sensor_configuration`: An optional configuration used for light sensor requests.
pub async fn handle_input(
    scene_manager: Arc<Mutex<dyn SceneManagerTrait>>,
    input_device_registry_request_stream_receiver: futures::channel::mpsc::UnboundedReceiver<
        InputDeviceRegistryRequestStream,
    >,
    light_sensor_request_stream_receiver: Option<
        futures::channel::mpsc::UnboundedReceiver<LightSensorRequestStream>,
    >,
    media_buttons_listener_registry_request_stream_receiver: futures::channel::mpsc::UnboundedReceiver<
        DeviceListenerRegistryRequestStream,
    >,
    factory_reset_countdown_request_stream_receiver: futures::channel::mpsc::UnboundedReceiver<
        FactoryResetCountdownRequestStream,
    >,
    factory_reset_device_request_stream_receiver: futures::channel::mpsc::UnboundedReceiver<
        DeviceRequestStream,
    >,
    icu_data_loader: icu_data::Loader,
    node: inspect::Node,
    display_ownership_event: zx::Event,
    focus_chain_publisher: FocusChainProviderPublisher,
    supported_input_devices: Vec<String>,
    light_sensor_configuration: Option<LightSensorConfiguration>,
    idle_threshold_ms: i64,
    interaction_state_publisher: InteractionStatePublisher,
    suspend_enabled: bool,
) -> Result<InputPipeline, Error> {
    let input_handlers_node = node.create_child("input_handlers");
    let metrics_logger = metrics::MetricsLogger::new();

    #[cfg(fuchsia_api_level_at_least = "HEAD")]
    let interaction_state_handler = InteractionStateHandler::new(
        zx::MonotonicDuration::from_millis(idle_threshold_ms as i64),
        &input_handlers_node,
        interaction_state_publisher,
        suspend_enabled,
    )
    .await;
    let factory_reset_handler =
        FactoryResetHandler::new(&input_handlers_node, metrics_logger.clone());
    let media_buttons_handler =
        MediaButtonsHandler::new(&input_handlers_node, metrics_logger.clone());

    let supported_input_devices =
        input_device::InputDeviceType::list_from_structured_config_list(&supported_input_devices);

    let light_sensor_handler = if let Some(light_sensor_configuration) = light_sensor_configuration
    {
        if supported_input_devices.contains(&input_device::InputDeviceType::LightSensor) {
            let light_proxy = connect_to_protocol::<LightMarker>()
                .context("unable to connnect to light proxy for light sensor")?;
            let brightness_proxy = connect_to_protocol::<BrightnessControlMarker>()
                .context("unable to connnect to brightness control proxy for light sensor")?;
            let factory_store_proxy = connect_to_protocol::<MiscFactoryStoreProviderMarker>()
                .context("unable to connect to factory proxy for light sensor")?;
            let factory_file_loader = FactoryFileLoader::new(factory_store_proxy)
                .context("unable to connect to factory file loader for light sensor")?;
            let calibration = if let Some(configuration) = light_sensor_configuration.calibration {
                LightSensorCalibration::new(configuration, &factory_file_loader)
                    .await
                    .map_err(|e| {
                        warn!(
                            "Calculations will use uncalibrated data. No light sensor \
                               calibration: {e:?}"
                        )
                    })
                    .ok()
            } else {
                info!(
                    "Calculations will use uncalibrated data. No light sensor \
                           calibration: Configuration not supplied"
                );
                None
            };
            let (handler, task) = make_light_sensor_handler_and_spawn_led_watcher(
                light_proxy,
                brightness_proxy,
                calibration,
                light_sensor_configuration.sensor,
                &input_handlers_node,
            )
            .await
            .context("unable to create light sensor handler")?;
            if let Some(task) = task {
                task.detach();
            }
            Some(handler)
        } else {
            None
        }
    } else {
        None
    };

    // Create parent node of inspect nodes for device bindings.
    let injected_devices_node = node.create_child("injected_input_devices");

    let input_pipeline = InputPipeline::new(
        supported_input_devices.clone(),
        build_input_pipeline_assembly(
            scene_manager,
            icu_data_loader,
            &node,
            display_ownership_event,
            interaction_state_handler,
            factory_reset_handler.clone(),
            media_buttons_handler.clone(),
            light_sensor_handler.clone(),
            HashSet::from_iter(supported_input_devices.iter()),
            focus_chain_publisher,
            input_handlers_node,
            metrics_logger.clone(),
        )
        .await,
        node,
        metrics_logger.clone(),
    )
    .context("Failed to create InputPipeline.")?;

    if let (Some(light_sensor_handler), Some(light_sensor_request_stream_receiver)) =
        (light_sensor_handler, light_sensor_request_stream_receiver)
    {
        let light_sensor_fut = handle_light_sensor_request_stream(
            light_sensor_request_stream_receiver,
            light_sensor_handler,
        );
        fasync::Task::local(light_sensor_fut).detach();
    }

    let input_device_registry_fut = handle_input_device_registry_request_streams(
        input_device_registry_request_stream_receiver,
        input_pipeline.input_device_types().clone(),
        input_pipeline.input_event_sender().clone(),
        input_pipeline.input_device_bindings().clone(),
        injected_devices_node,
        metrics_logger.clone(),
    );
    fasync::Task::local(input_device_registry_fut).detach();

    let factory_reset_countdown_fut = handle_factory_reset_countdown_request_stream(
        factory_reset_countdown_request_stream_receiver,
        factory_reset_handler.clone(),
    );
    fasync::Task::local(factory_reset_countdown_fut).detach();

    let factory_reset_device_device_fut = handle_recovery_policy_device_request_stream(
        factory_reset_device_request_stream_receiver,
        factory_reset_handler.clone(),
    );
    fasync::Task::local(factory_reset_device_device_fut).detach();

    let media_buttons_listener_registry_fut = handle_device_listener_registry_request_stream(
        media_buttons_listener_registry_request_stream_receiver,
        media_buttons_handler.clone(),
    );
    fasync::Task::local(media_buttons_listener_registry_fut).detach();

    Ok(input_pipeline)
}

fn setup_pointer_injector_config_request_stream(
    scene_manager: Arc<Mutex<dyn SceneManagerTrait>>,
) -> SetupProxy {
    let (setup_proxy, setup_request_stream) = fidl::endpoints::create_proxy_and_stream::<
        fidl_fuchsia_ui_pointerinjector_configuration::SetupMarker,
    >();

    scene_management::handle_pointer_injector_configuration_setup_request_stream(
        setup_request_stream,
        scene_manager,
    );

    setup_proxy
}

async fn add_touchscreen_handler(
    scene_manager: Arc<Mutex<dyn SceneManagerTrait>>,
    mut assembly: InputPipelineAssembly,
    input_handlers_node: &inspect::Node,
    metrics_logger: metrics::MetricsLogger,
) -> InputPipelineAssembly {
    let setup_proxy = setup_pointer_injector_config_request_stream(scene_manager.clone());
    let size = scene_manager.lock().await.get_pointerinjection_display_size();
    // touch injector handler is the last handler for touch event handling, it sends out touch
    // events to scenic. Please double check tracing events, when changing the handlers assembly
    // order.
    let touch_handler = TouchInjectorHandler::new_with_config_proxy(
        setup_proxy,
        size,
        input_handlers_node,
        metrics_logger,
    )
    .await;
    match touch_handler {
        Ok(touch_handler) => {
            fasync::Task::local(touch_handler.clone().watch_viewport()).detach();
            assembly = assembly.add_handler(touch_handler);
        }
        Err(e) => error!(
            "build_input_pipeline_assembly(): Touch injector handler was not installed: {:?}",
            e
        ),
    };
    assembly
}

async fn add_mouse_handler(
    scene_manager: Arc<Mutex<dyn SceneManagerTrait>>,
    mut assembly: InputPipelineAssembly,
    sender: futures::channel::mpsc::Sender<CursorMessage>,
    input_handlers_node: &inspect::Node,
    metrics_logger: metrics::MetricsLogger,
) -> InputPipelineAssembly {
    let setup_proxy = setup_pointer_injector_config_request_stream(scene_manager.clone());
    let size = scene_manager.lock().await.get_pointerinjection_display_size();
    let mouse_handler = MouseInjectorHandler::new_with_config_proxy(
        setup_proxy,
        size,
        sender,
        input_handlers_node,
        metrics_logger,
    )
    .await;
    match mouse_handler {
        Ok(mouse_handler) => {
            fasync::Task::local(mouse_handler.clone().watch_viewport()).detach();
            assembly = assembly.add_handler(mouse_handler);
        }
        Err(e) => error!(
            "build_input_pipeline_assembly(): Mouse injector handler was not installed: {:?}",
            e
        ),
    };
    assembly
}

/// Registers the keyboard handlers that deal with keyboard.
async fn register_keyboard_related_input_handlers(
    assembly: InputPipelineAssembly,
    display_ownership_event: zx::Event,
    icu_data_loader: icu_data::Loader,
    focus_chain_publisher: FocusChainProviderPublisher,
    input_handlers_node: &inspect::Node,
    metrics_logger: metrics::MetricsLogger,
) -> InputPipelineAssembly {
    // Add as early as possible, but not before inspect handlers.
    let mut assembly = add_chromebook_keyboard_handler(assembly, input_handlers_node);

    // Display ownership deals with keyboard events.
    assembly = assembly.add_display_ownership(display_ownership_event, input_handlers_node);
    assembly = add_modifier_handler(assembly, input_handlers_node);

    // Add the text settings handler early in the pipeline to use the
    // keymap settings in the remainder of the pipeline.
    assembly = add_text_settings_handler(assembly, input_handlers_node, metrics_logger.clone());
    assembly = add_keymap_handler(assembly, input_handlers_node);
    assembly = add_key_meaning_modifier_handler(assembly, input_handlers_node);
    assembly = add_dead_keys_handler(assembly, icu_data_loader, input_handlers_node);

    // ime_handler is the last handler for key event handling, it sends out key events to
    // listeners. Please double check tracing events, when changing the handlers assembly order.
    assembly = add_ime(assembly, input_handlers_node, metrics_logger.clone()).await;

    // Forward focus to Text Manager.
    // This requires `fuchsia.ui.focus.FocusChainListenerRegistry`
    assembly = assembly.add_focus_listener(focus_chain_publisher);
    assembly
}

/// Installs the handlers for mouse input.
async fn register_mouse_related_input_handlers(
    assembly: InputPipelineAssembly,
    scene_manager: Arc<Mutex<dyn SceneManagerTrait>>,
    input_pipeline_node: &inspect::Node,
    input_handlers_node: &inspect::Node,
    metrics_logger: metrics::MetricsLogger,
) -> InputPipelineAssembly {
    let (sender, mut receiver) = futures::channel::mpsc::channel(0);

    // Add the touchpad gestures handler after the click-drag handler,
    // since the gestures handler creates mouse events but already
    // disambiguates between click and drag gestures.
    let mut assembly =
        add_touchpad_gestures_handler(assembly, input_pipeline_node, input_handlers_node);

    // Add handler to scale pointer motion based on speed of sensor
    // motion. This allows touchpads and mice to be easily used for
    // both precise pointing, and quick motion across the width
    // (or height) of the screen.
    //
    // This handler must come before the PointerMotionDisplayScaleHandler.
    // Otherwise the display scale will be applied quadratically in some
    // cases.
    assembly =
        add_pointer_sensor_scale_handler(assembly, input_handlers_node, metrics_logger.clone());

    // Add handler to scale pointer motion on high-DPI displays.
    //
    // * This handler is added _after_ the click-drag handler, since the
    //   motion denoising done by click drag handler is a property solely
    //   of the trackpad, and not of the display.
    //
    // * This handler is added _before_ the mouse handler, since _all_
    //   mouse events should be scaled.
    let pointer_scale =
        scene_manager.lock().await.get_display_metrics().physical_pixel_ratio().max(1.0);
    assembly = add_pointer_display_scale_handler(
        assembly,
        pointer_scale,
        input_handlers_node,
        metrics_logger.clone(),
    );

    // mouse injector handler is the last handler for mouse event handling, it sends out mouse
    // events to scenic. Please double check tracing events, when changing the handlers assembly
    // order.
    assembly = add_mouse_handler(
        scene_manager.clone(),
        assembly,
        sender,
        input_handlers_node,
        metrics_logger,
    )
    .await;

    let scene_manager = scene_manager.clone();
    fasync::Task::spawn(async move {
        while let Some(message) = receiver.next().await {
            let mut scene_manager = scene_manager.lock().await;
            match message {
                CursorMessage::SetPosition(position) => scene_manager.set_cursor_position(position),
                CursorMessage::SetVisibility(visible) => {
                    scene_manager.set_cursor_visibility(visible)
                }
            }
        }
    })
    .detach();
    assembly
}

async fn build_input_pipeline_assembly(
    scene_manager: Arc<Mutex<dyn SceneManagerTrait>>,
    icu_data_loader: icu_data::Loader,
    node: &inspect::Node,
    display_ownership_event: zx::Event,
    interaction_state_handler: Rc<InteractionStateHandler>,
    factory_reset_handler: Rc<FactoryResetHandler>,
    media_buttons_handler: Rc<MediaButtonsHandler>,
    light_sensor_handler: Option<Rc<CalibratedLightSensorHandler>>,
    supported_input_devices: HashSet<&input_device::InputDeviceType>,
    focus_chain_publisher: FocusChainProviderPublisher,
    input_handlers_node: inspect::Node,
    metrics_logger: metrics::MetricsLogger,
) -> InputPipelineAssembly {
    let mut assembly = InputPipelineAssembly::new(metrics_logger.clone());
    {
        // Keep this handler first because it keeps performance measurement counters
        // for the rest of the pipeline at entry.
        assembly = add_inspect_handler(
            node.create_child("input_pipeline_entry"),
            assembly,
            &supported_input_devices,
            /* displays_recent_events = */ true,
        );
        assembly = assembly.add_handler(interaction_state_handler);

        if supported_input_devices.contains(&input_device::InputDeviceType::Keyboard) {
            info!("Registering keyboard-related input handlers.");
            assembly = register_keyboard_related_input_handlers(
                assembly,
                display_ownership_event,
                icu_data_loader,
                focus_chain_publisher,
                &input_handlers_node,
                metrics_logger.clone(),
            )
            .await;
        }

        if supported_input_devices.contains(&input_device::InputDeviceType::ConsumerControls) {
            info!("Registering consumer controls-related input handlers.");
            // Add factory reset handler before media buttons handler.
            assembly = assembly.add_handler(factory_reset_handler);

            // media_buttons_handler is the last handler for media button handling, it sends out
            // button events to listeners. Please double check tracing events, when changing the
            // handlers assembly order.
            assembly = assembly.add_handler(media_buttons_handler);
        }

        if supported_input_devices.contains(&input_device::InputDeviceType::LightSensor) {
            if let Some(light_sensor_handler) = light_sensor_handler {
                info!("Registering light sensor-related input handlers.");
                assembly = assembly.add_handler(light_sensor_handler);
            }
        }

        if supported_input_devices.contains(&input_device::InputDeviceType::Mouse) {
            info!("Registering mouse-related input handlers.");
            assembly = register_mouse_related_input_handlers(
                assembly,
                scene_manager.clone(),
                node,
                &input_handlers_node,
                metrics_logger.clone(),
            )
            .await;
        }

        if supported_input_devices.contains(&input_device::InputDeviceType::Touch) {
            info!("Registering touchscreen-related input handlers.");
            assembly = add_touchscreen_handler(
                scene_manager.clone(),
                assembly,
                &input_handlers_node,
                metrics_logger,
            )
            .await;
        }
    }

    // Keep this handler last because it keeps performance measurement counters
    // for the rest of the pipeline at exit.  We compare these values to the
    // values at entry.
    assembly = add_inspect_handler(
        node.create_child("input_pipeline_exit"),
        assembly,
        &supported_input_devices,
        /* displays_recent_events = */ false,
    );

    // Record input_handlers_node to it's parent node so that it does not get dropped
    // from the Inspect tree when we exit this scope.
    node.record(input_handlers_node);

    assembly
}

fn add_chromebook_keyboard_handler(
    assembly: InputPipelineAssembly,
    input_handlers_node: &inspect::Node,
) -> InputPipelineAssembly {
    assembly.add_handler(
        input_pipeline::chromebook_keyboard_handler::ChromebookKeyboardHandler::new(
            input_handlers_node,
        ),
    )
}

/// Hooks up the modifier keys handler.
fn add_modifier_handler(
    assembly: InputPipelineAssembly,
    input_handlers_node: &inspect::Node,
) -> InputPipelineAssembly {
    assembly
        .add_handler(input_pipeline::modifier_handler::ModifierHandler::new(input_handlers_node))
}

/// Hooks up the modifier keys handler based on key meanings.  This must come
/// after the keymap handler.
fn add_key_meaning_modifier_handler(
    assembly: InputPipelineAssembly,
    input_handlers_node: &inspect::Node,
) -> InputPipelineAssembly {
    assembly.add_handler(input_pipeline::modifier_handler::ModifierMeaningHandler::new(
        input_handlers_node,
    ))
}

/// Hooks up the inspect handler.
fn add_inspect_handler(
    node: inspect::Node,
    assembly: InputPipelineAssembly,
    supported_input_devices: &HashSet<&input_device::InputDeviceType>,
    displays_recent_events: bool,
) -> InputPipelineAssembly {
    assembly.add_handler(input_pipeline::inspect_handler::make_inspect_handler(
        node,
        supported_input_devices,
        displays_recent_events,
    ))
}

/// Hooks up the text settings handler.
fn add_text_settings_handler(
    assembly: InputPipelineAssembly,
    input_handlers_node: &fuchsia_inspect::Node,
    metrics_logger: metrics::MetricsLogger,
) -> InputPipelineAssembly {
    let proxy = connect_to_protocol::<fsettings::KeyboardMarker>()
        .expect("needs a connection to fuchsia.settings.Keyboard");
    let text_handler = TextSettingsHandler::new(None, None, input_handlers_node, metrics_logger);
    text_handler.clone().serve(proxy);
    assembly.add_handler(text_handler)
}

/// Hooks up the keymapper.  The keymapper requires the text settings handler to
/// be added as well to support keymapping.  Otherwise, it defaults to applying
/// the US QWERTY keymap.
fn add_keymap_handler(
    assembly: InputPipelineAssembly,
    input_handlers_node: &inspect::Node,
) -> InputPipelineAssembly {
    assembly.add_handler(keymap_handler::KeymapHandler::new(input_handlers_node))
}

/// Hooks up the dead keys handler. This allows us to input accented characters by composing a
/// diacritic and a character.
fn add_dead_keys_handler(
    assembly: InputPipelineAssembly,
    loader: icu_data::Loader,
    input_handlers_node: &inspect::Node,
) -> InputPipelineAssembly {
    assembly.add_handler(dead_keys_handler::DeadKeysHandler::new(loader, input_handlers_node))
}

async fn add_ime(
    mut assembly: InputPipelineAssembly,
    input_handlers_node: &inspect::Node,
    metrics_logger: metrics::MetricsLogger,
) -> InputPipelineAssembly {
    if let Ok(ime_handler) = ImeHandler::new(input_handlers_node, metrics_logger).await {
        assembly = assembly.add_handler(ime_handler);
    }
    assembly
}

fn add_pointer_display_scale_handler(
    assembly: InputPipelineAssembly,
    scale_factor: f32,
    input_handlers_node: &inspect::Node,
    metrics_logger: metrics::MetricsLogger,
) -> InputPipelineAssembly {
    match input_pipeline::pointer_display_scale_handler::PointerDisplayScaleHandler::new(
        scale_factor,
        input_handlers_node,
        metrics_logger,
    ) {
        Ok(handler) => assembly.add_handler(handler),
        Err(e) => {
            error!("Failed to install pointer scaler: {}", e);
            assembly
        }
    }
}

fn add_pointer_sensor_scale_handler(
    assembly: InputPipelineAssembly,
    input_handlers_node: &inspect::Node,
    metrics_logger: metrics::MetricsLogger,
) -> InputPipelineAssembly {
    assembly.add_handler(
        input_pipeline::pointer_sensor_scale_handler::PointerSensorScaleHandler::new(
            input_handlers_node,
            metrics_logger,
        ),
    )
}

fn add_touchpad_gestures_handler(
    assembly: InputPipelineAssembly,
    inspect_node: &inspect::Node,
    input_handlers_node: &inspect::Node,
) -> InputPipelineAssembly {
    assembly.add_handler(input_pipeline::make_touchpad_gestures_handler(
        inspect_node,
        input_handlers_node,
    ))
}

pub async fn handle_device_listener_registry_request_stream(
    mut stream_receiver: futures::channel::mpsc::UnboundedReceiver<
        DeviceListenerRegistryRequestStream,
    >,
    media_buttons_handler: Rc<MediaButtonsHandler>,
) {
    while let Some(stream) = stream_receiver.next().await {
        let media_buttons_handler = media_buttons_handler.clone();
        fasync::Task::local(async move {
            match media_buttons_handler.handle_device_listener_registry_request_stream(stream).await
            {
                Ok(()) => (),
                Err(e) => {
                    warn!("failure while serving DeviceListenerRegistry: {}", e);
                }
            }
        })
        .detach();
    }
}

pub async fn handle_factory_reset_countdown_request_stream(
    mut stream_receiver: futures::channel::mpsc::UnboundedReceiver<
        FactoryResetCountdownRequestStream,
    >,
    factory_reset_handler: Rc<FactoryResetHandler>,
) {
    while let Some(stream) = stream_receiver.next().await {
        let factory_reset_handler = factory_reset_handler.clone();
        fasync::Task::local(async move {
            match factory_reset_handler.handle_factory_reset_countdown_request_stream(stream).await
            {
                Ok(()) => (),
                Err(e) => {
                    warn!("failure while serving FactoryResetCountdown: {}", e);
                }
            }
        })
        .detach();
    }
}

pub async fn handle_light_sensor_request_stream(
    mut stream_receiver: futures::channel::mpsc::UnboundedReceiver<LightSensorRequestStream>,
    light_sensor_handler: Rc<CalibratedLightSensorHandler>,
) {
    while let Some(stream) = stream_receiver.next().await {
        let light_sensor_handler = light_sensor_handler.clone();
        fasync::Task::local(async move {
            match light_sensor_handler.handle_light_sensor_request_stream(stream).await {
                Ok(()) => (),
                Err(e) => {
                    warn!("failure while serving fuchsia.lightsensor.Sensor: {e}");
                }
            }
        })
        .detach();
    }
}

pub async fn handle_recovery_policy_device_request_stream(
    mut stream_receiver: futures::channel::mpsc::UnboundedReceiver<DeviceRequestStream>,
    factory_reset_handler: Rc<FactoryResetHandler>,
) {
    while let Some(stream) = stream_receiver.next().await {
        let factory_reset_handler = factory_reset_handler.clone();
        fasync::Task::local(async move {
            match factory_reset_handler.handle_recovery_policy_device_request_stream(stream).await {
                Ok(()) => (),
                Err(e) => {
                    warn!("failure while serving fuchsia.recovery.policy.Device: {}", e);
                }
            }
        })
        .detach();
    }
}

pub async fn handle_input_device_registry_request_streams(
    mut stream_receiver: futures::channel::mpsc::UnboundedReceiver<
        InputDeviceRegistryRequestStream,
    >,
    input_device_types: Vec<input_device::InputDeviceType>,
    input_event_sender: futures::channel::mpsc::UnboundedSender<input_device::InputEvent>,
    input_device_bindings: InputDeviceBindingHashMap,
    injected_devices_node: inspect::Node,
    metrics_logger: metrics::MetricsLogger,
) {
    while let Some(stream) = stream_receiver.next().await {
        let input_device_types_clone = input_device_types.clone();
        let input_event_sender_clone = input_event_sender.clone();
        let input_device_bindings_clone = input_device_bindings.clone();
        let metrics_logger_clone = metrics_logger.clone();

        // Must clone inspect node since we move it to our async task, but we want to
        // continue to operate on this inspect tree in future iterations of the loop.
        let node_clone = injected_devices_node.clone_weak();

        // TODO(https://fxbug.dev/42061133): Push this task down to InputPipeline.
        // I didn't do that here, to keep the scope of this change small.
        fasync::Task::local(async move {
            match InputPipeline::handle_input_device_registry_request_stream(
                stream,
                &input_device_types_clone,
                &input_event_sender_clone,
                &input_device_bindings_clone,
                &node_clone,
                metrics_logger_clone,
            )
            .await
            {
                Ok(()) => (),
                Err(e) => {
                    warn!(
                        "failure while serving InputDeviceRegistry: {}; \
                         will continue serving other clients",
                        e
                    );
                }
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use fuchsia_async as fasync;

    #[fasync::run_singlethreaded(test)]
    async fn test_placeholder() {
        // TODO(https://fxbug.dev/42153238): Add tests that verify the construction of the input pipeline.
    }
}
