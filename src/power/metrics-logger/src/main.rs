// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod cpu_load_logger;
mod driver_utils;
mod gpu_usage_logger;
mod network_activity_logger;
mod sensor_logger;

use crate::cpu_load_logger::{generate_cpu_stats_driver, CpuLoadLogger, CpuStatsDriver};
use crate::driver_utils::Config;
use crate::gpu_usage_logger::{generate_gpu_drivers, GpuDriver, GpuUsageLogger};
use crate::network_activity_logger::{generate_network_devices, NetworkActivityLoggerBuilder};
use crate::sensor_logger::{
    generate_power_drivers, generate_temperature_drivers, ActivityListener, PowerDriver,
    PowerLogger, PowerLoggerArgs, TemperatureDriver, TemperatureLogger, TemperatureLoggerArgs,
};
use anyhow::{Error, Result};
use argh::FromArgs;
use fidl::endpoints::ProtocolMarker;
use fidl_fuchsia_power_metrics::{self as fmetrics, RecorderRequest};
use fuchsia_component::client::connect_to_protocol;
use fuchsia_component::server::ServiceFs;
use futures::future::join_all;
use futures::stream::{FuturesUnordered, StreamExt, TryStreamExt};
use futures::task::Context;
use futures::{FutureExt, TryFutureExt};
use log::{error, info, warn};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::rc::Rc;
use {
    fidl_fuchsia_hardware_network as fhwnet, fidl_fuchsia_ui_activity as factivity,
    fuchsia_async as fasync, fuchsia_inspect as inspect, serde_json as json,
};

// Max number of clients that can log concurrently. This limit is chosen mostly arbitrarily to allow
// a fixed number clients to keep memory use bounded.
const MAX_CONCURRENT_CLIENTS: usize = 20;

// Minimum interval for logging to syslog.
const MIN_INTERVAL_FOR_SYSLOG_MS: u32 = 500;

const CONFIG_PATH: &'static str = "/config/data/config.json";

const STANDALONE_SAMPLING_INTERVAL: u32 = 1000; // 1 sec.
const STANDALONE_STATISTICS_INTERVAL: u32 = 60 * 1000; // 60 secs.
const STANDALONE_CLIENT_ID: &'static str = "metrics-logger-standalone";

/// Builds a MetricsLoggerServer.
pub struct ServerBuilder<'a> {
    /// Optional drivers for test usage.
    temperature_drivers: Option<Vec<TemperatureDriver>>,

    /// Optional drivers for test usage.
    power_drivers: Option<Vec<PowerDriver>>,

    /// Optional drivers for test usage.
    gpu_drivers: Option<Vec<GpuDriver>>,

    /// Optional proxy for test usage.
    cpu_stats_driver: Option<CpuStatsDriver>,

    /// Optional proxies for test usage.
    network_devices: Option<Vec<fhwnet::DeviceProxy>>,

    /// Optional inspect root for test usage.
    inspect_root: Option<&'a inspect::Node>,

    /// Config for driver aliases.
    config: Option<Config>,
}

impl<'a> ServerBuilder<'a> {
    /// Constructs a new ServerBuilder from a JSON configuration.
    fn new_from_json(json_data: Option<json::Value>) -> Self {
        let config: Option<Config> = json_data.map(|d| json::from_value(d).unwrap());

        ServerBuilder {
            temperature_drivers: None,
            power_drivers: None,
            gpu_drivers: None,
            cpu_stats_driver: None,
            network_devices: None,
            inspect_root: None,
            config,
        }
    }

    /// For testing purposes, proxies may be provided directly to the Server builder.
    #[cfg(test)]
    fn with_temperature_drivers(mut self, temperature_drivers: Vec<TemperatureDriver>) -> Self {
        self.temperature_drivers = Some(temperature_drivers);
        self
    }

    #[cfg(test)]
    fn with_power_drivers(mut self, power_drivers: Vec<PowerDriver>) -> Self {
        self.power_drivers = Some(power_drivers);
        self
    }

    #[cfg(test)]
    fn with_gpu_drivers(mut self, gpu_drivers: Vec<GpuDriver>) -> Self {
        self.gpu_drivers = Some(gpu_drivers);
        self
    }

    #[cfg(test)]
    fn with_cpu_stats_driver(mut self, cpu_stats_driver: CpuStatsDriver) -> Self {
        self.cpu_stats_driver = Some(cpu_stats_driver);
        self
    }

    #[cfg(test)]
    fn with_network_devices(mut self, network_devices: Vec<fhwnet::DeviceProxy>) -> Self {
        self.network_devices = Some(network_devices);
        self
    }

    /// Injects an Inspect root for use in tests.
    #[cfg(test)]
    fn with_inspect_root(mut self, root: &'a inspect::Node) -> Self {
        self.inspect_root = Some(root);
        self
    }

    /// Builds a MetricsLoggerServer.
    async fn build(self) -> Result<Rc<MetricsLoggerServer>> {
        let temperature_drivers = match self.temperature_drivers {
            None => RefCell::new(None),
            Some(drivers) => RefCell::new(Some(Rc::new(drivers))),
        };

        let power_drivers = match self.power_drivers {
            None => RefCell::new(None),
            Some(drivers) => RefCell::new(Some(Rc::new(drivers))),
        };

        let cpu_stats_driver = match self.cpu_stats_driver {
            None => RefCell::new(None),
            Some(proxy) => RefCell::new(Some(Rc::new(proxy))),
        };

        let gpu_drivers = match self.gpu_drivers {
            None => RefCell::new(None),
            Some(drivers) => RefCell::new(Some(Rc::new(drivers))),
        };

        let network_devices = match self.network_devices {
            None => RefCell::new(None),
            Some(devices) => RefCell::new(Some(Rc::new(devices))),
        };

        // Optionally use the default inspect root node
        let inspect_root =
            self.inspect_root.unwrap_or_else(|| inspect::component::inspector().root());

        Ok(MetricsLoggerServer::new(
            temperature_drivers,
            power_drivers,
            gpu_drivers,
            network_devices,
            cpu_stats_driver,
            self.config,
            inspect_root.create_child("MetricsLogger"),
        ))
    }
}

struct MetricsLoggerServer {
    /// List of temperature sensor drivers for polling temperatures.
    temperature_drivers: RefCell<Option<Rc<Vec<TemperatureDriver>>>>,

    /// List of power sensor drivers for polling powers.
    power_drivers: RefCell<Option<Rc<Vec<PowerDriver>>>>,

    /// List of gpu drivers for polling GPU stats.
    gpu_drivers: RefCell<Option<Rc<Vec<GpuDriver>>>>,

    /// List of network devices for querying network activities.
    network_devices: RefCell<Option<Rc<Vec<fhwnet::DeviceProxy>>>>,

    /// Proxy for polling CPU stats.
    cpu_stats_driver: RefCell<Option<Rc<CpuStatsDriver>>>,

    /// Root node for MetricsLogger
    inspect_root: inspect::Node,

    /// Config for driver aliases.
    config: Option<Config>,

    /// Map that stores the logging task for all clients. Once a logging request is received
    /// with a new client_id, a task is lazily inserted into the map using client_id as the key.
    client_tasks: RefCell<HashMap<String, fasync::Task<()>>>,
}

impl MetricsLoggerServer {
    fn new(
        temperature_drivers: RefCell<Option<Rc<Vec<TemperatureDriver>>>>,
        power_drivers: RefCell<Option<Rc<Vec<PowerDriver>>>>,
        gpu_drivers: RefCell<Option<Rc<Vec<GpuDriver>>>>,
        network_devices: RefCell<Option<Rc<Vec<fhwnet::DeviceProxy>>>>,
        cpu_stats_driver: RefCell<Option<Rc<CpuStatsDriver>>>,
        config: Option<Config>,
        inspect_root: inspect::Node,
    ) -> Rc<Self> {
        Rc::new(Self {
            temperature_drivers,
            power_drivers,
            gpu_drivers,
            network_devices,
            cpu_stats_driver,
            inspect_root,
            config,
            client_tasks: RefCell::new(HashMap::new()),
        })
    }

    fn handle_new_service_connection(
        self: Rc<Self>,
        mut stream: fmetrics::RecorderRequestStream,
    ) -> fasync::Task<()> {
        fasync::Task::local(
            async move {
                while let Some(request) = stream.try_next().await? {
                    self.clone().handle_metrics_logger_request(request).await?;
                }
                Ok(())
            }
            .unwrap_or_else(|e: Error| error!("{:?}", e)),
        )
    }

    async fn handle_metrics_logger_request(
        self: &Rc<Self>,
        request: RecorderRequest,
    ) -> Result<()> {
        self.purge_completed_tasks();

        match request {
            RecorderRequest::StartLogging {
                client_id,
                metrics,
                duration_ms,
                output_samples_to_syslog,
                output_stats_to_syslog,
                responder,
            } => {
                let result = self
                    .start_logging(
                        &client_id,
                        metrics,
                        output_samples_to_syslog,
                        output_stats_to_syslog,
                        Some(duration_ms),
                    )
                    .await;
                responder.send(result)?;
            }
            RecorderRequest::StartLoggingForever {
                client_id,
                metrics,
                output_samples_to_syslog,
                output_stats_to_syslog,
                responder,
            } => {
                let result = self
                    .start_logging(
                        &client_id,
                        metrics,
                        output_samples_to_syslog,
                        output_stats_to_syslog,
                        None,
                    )
                    .await;
                responder.send(result)?;
            }
            RecorderRequest::StopLogging { client_id, responder } => {
                responder.send(self.client_tasks.borrow_mut().remove(&client_id).is_some())?;
            }
        }

        Ok(())
    }

    async fn start_logging(
        &self,
        client_id: &str,
        metrics: Vec<fmetrics::Metric>,
        output_samples_to_syslog: bool,
        output_stats_to_syslog: bool,
        duration_ms: Option<u32>,
    ) -> fmetrics::RecorderStartLoggingResult {
        if self.client_tasks.borrow_mut().contains_key(client_id) {
            return Err(fmetrics::RecorderError::AlreadyLogging);
        }

        if self.client_tasks.borrow().len() >= MAX_CONCURRENT_CLIENTS {
            return Err(fmetrics::RecorderError::TooManyActiveClients);
        }

        let incoming_metric_types: HashSet<_> =
            HashSet::from_iter(metrics.iter().map(|m| std::mem::discriminant(m)));
        if incoming_metric_types.len() != metrics.len() {
            return Err(fmetrics::RecorderError::DuplicatedMetric);
        }

        let client_inspect = self.inspect_root.create_child(&client_id.to_string());
        let futures: FuturesUnordered<Box<dyn futures::Future<Output = ()>>> =
            FuturesUnordered::new();

        for metric in metrics {
            match metric {
                fmetrics::Metric::CpuLoad(fmetrics::CpuLoad { interval_ms }) => {
                    let driver = self.get_cpu_driver().await?;
                    let cpu_load_logger = CpuLoadLogger::new(
                        driver,
                        interval_ms,
                        duration_ms,
                        &client_inspect,
                        String::from(&client_id.to_string()),
                        output_samples_to_syslog,
                    )
                    .await?;
                    futures.push(Box::new(cpu_load_logger.log_cpu_usages()));
                }
                fmetrics::Metric::GpuUsage(fmetrics::GpuUsage { interval_ms }) => {
                    let drivers = self.get_gpu_drivers().await?;
                    let gpu_usage_logger = GpuUsageLogger::new(
                        drivers,
                        interval_ms,
                        duration_ms,
                        &client_inspect,
                        String::from(&client_id.to_string()),
                        output_samples_to_syslog,
                    )
                    .await?;
                    futures.push(Box::new(gpu_usage_logger.log_gpu_usages()));
                }
                fmetrics::Metric::NetworkActivity(fmetrics::NetworkActivity { interval_ms }) => {
                    let devices = self.get_network_devices().await?;
                    let network_activity_logger = NetworkActivityLoggerBuilder::new(
                        devices,
                        interval_ms,
                        duration_ms,
                        &client_inspect,
                        String::from(&client_id.to_string()),
                        output_samples_to_syslog,
                    )
                    .build()
                    .await?;
                    futures.push(Box::new(network_activity_logger.log_network_activities()));
                }
                fmetrics::Metric::Temperature(fmetrics::Temperature {
                    sampling_interval_ms,
                    statistics_args,
                }) => {
                    let drivers = self.get_temperature_drivers().await?;
                    let activity_listener = create_activity_listener();

                    let temperature_logger = TemperatureLogger::new(TemperatureLoggerArgs {
                        drivers,
                        activity_listener,
                        sampling_interval_ms,
                        statistics_interval_ms: statistics_args.map(|i| i.statistics_interval_ms),
                        duration_ms,
                        client_inspect: &client_inspect,
                        client_id: String::from(&client_id.to_string()),
                        output_samples_to_syslog,
                        output_stats_to_syslog,
                    })
                    .await?;
                    futures.push(Box::new(temperature_logger.log_data()));
                }
                fmetrics::Metric::Power(fmetrics::Power {
                    sampling_interval_ms,
                    statistics_args,
                }) => {
                    let drivers = self.get_power_drivers().await?;
                    let activity_listener = create_activity_listener();

                    let power_logger = PowerLogger::new(PowerLoggerArgs {
                        drivers: drivers,
                        activity_listener: activity_listener,
                        sampling_interval_ms: sampling_interval_ms,
                        statistics_interval_ms: statistics_args.map(|i| i.statistics_interval_ms),
                        duration_ms: duration_ms,
                        client_inspect: &client_inspect,
                        client_id: String::from(&client_id.to_string()),
                        output_samples_to_syslog: output_samples_to_syslog,
                        output_stats_to_syslog: output_stats_to_syslog,
                    })
                    .await?;
                    futures.push(Box::new(power_logger.log_data()));
                }
            }
        }

        self.client_tasks.borrow_mut().insert(
            client_id.to_string(),
            fasync::Task::local(async move {
                // Move ownership of `client_inspect` to the client task.
                let _inspect = client_inspect;
                join_all(futures.into_iter().map(|f| Pin::from(f))).await;
            }),
        );

        Ok(())
    }

    fn purge_completed_tasks(&self) {
        self.client_tasks.borrow_mut().retain(|_n, task| {
            task.poll_unpin(&mut Context::from_waker(futures::task::noop_waker_ref())).is_pending()
        });
    }

    async fn get_temperature_drivers(
        &self,
    ) -> Result<Rc<Vec<TemperatureDriver>>, fmetrics::RecorderError> {
        match self.temperature_drivers.borrow().as_ref() {
            Some(drivers) => return Ok(drivers.clone()),
            _ => (),
        }

        let driver_aliases = match &self.config {
            None => HashMap::new(),
            Some(c) => c.temperature_drivers.as_ref().map_or_else(
                || HashMap::new(),
                |d| d.into_iter().map(|m| (m.sensor_name.clone(), m.alias.clone())).collect(),
            ),
        };

        let drivers =
            Rc::new(generate_temperature_drivers(driver_aliases).await.map_err(|err| {
                error!(err:%; "Request failed with internal error");
                fmetrics::RecorderError::Internal
            })?);
        self.temperature_drivers.replace(Some(drivers.clone()));
        Ok(drivers)
    }

    async fn get_power_drivers(&self) -> Result<Rc<Vec<PowerDriver>>, fmetrics::RecorderError> {
        match self.power_drivers.borrow().as_ref() {
            Some(drivers) => return Ok(drivers.clone()),
            _ => (),
        }

        let driver_aliases = match &self.config {
            None => HashMap::new(),
            Some(c) => c.power_drivers.as_ref().map_or_else(
                || HashMap::new(),
                |d| d.into_iter().map(|m| (m.sensor_name.clone(), m.alias.clone())).collect(),
            ),
        };

        let drivers = Rc::new(generate_power_drivers(driver_aliases).await.map_err(|err| {
            error!(err:%; "Request failed with internal error");
            fmetrics::RecorderError::Internal
        })?);
        self.power_drivers.replace(Some(drivers.clone()));
        Ok(drivers)
    }

    async fn get_gpu_drivers(&self) -> Result<Rc<Vec<GpuDriver>>, fmetrics::RecorderError> {
        match self.gpu_drivers.borrow().as_ref() {
            Some(drivers) => return Ok(drivers.clone()),
            _ => (),
        }

        let driver_aliases = match &self.config {
            None => HashMap::new(),
            Some(c) => c.gpu_drivers.as_ref().map_or_else(
                || HashMap::new(),
                |d| d.into_iter().map(|m| (m.sensor_name.clone(), m.alias.clone())).collect(),
            ),
        };

        let drivers = Rc::new(generate_gpu_drivers(driver_aliases).await.map_err(|err| {
            error!(err:%; "Request failed with internal error");
            fmetrics::RecorderError::Internal
        })?);
        self.gpu_drivers.replace(Some(drivers.clone()));
        Ok(drivers)
    }

    async fn get_cpu_driver(&self) -> Result<Rc<CpuStatsDriver>, fmetrics::RecorderError> {
        match self.cpu_stats_driver.borrow().as_ref() {
            Some(driver) => return Ok(driver.clone()),
            _ => (),
        }

        let driver = Rc::new(generate_cpu_stats_driver().await.map_err(|err| {
            error!(err:%; "Request failed with internal error");
            fmetrics::RecorderError::Internal
        })?);
        self.cpu_stats_driver.replace(Some(driver.clone()));
        Ok(driver)
    }

    async fn get_network_devices(
        &self,
    ) -> Result<Rc<Vec<fhwnet::DeviceProxy>>, fmetrics::RecorderError> {
        match &*self.network_devices.borrow() {
            Some(device) => return Ok(device.clone()),
            _ => (),
        }

        let device = Rc::new(generate_network_devices().await.map_err(|err| {
            error!(err:%; "Request failed with internal error");
            fmetrics::RecorderError::Internal
        })?);
        self.network_devices.replace(Some(device.clone()));
        Ok(device)
    }

    async fn run_logging_standalone(
        &self,
        sampling_interval_ms: u32,
        statistics_interval_ms: u32,
        duration_ms: Option<u32>,
    ) {
        self.start_logging(
            STANDALONE_CLIENT_ID,
            vec![
                fmetrics::Metric::Power(fmetrics::Power {
                    sampling_interval_ms,
                    statistics_args: Some(Box::new(fmetrics::StatisticsArgs {
                        statistics_interval_ms,
                    })),
                }),
                fmetrics::Metric::Temperature(fmetrics::Temperature {
                    sampling_interval_ms,
                    statistics_args: Some(Box::new(fmetrics::StatisticsArgs {
                        statistics_interval_ms,
                    })),
                }),
            ],
            false,
            true,
            duration_ms,
        )
        .await
        .expect("logging to start");
        self.client_tasks.borrow_mut().remove(STANDALONE_CLIENT_ID).unwrap().await;
    }
}

fn create_activity_listener() -> Option<ActivityListener> {
    let proxy = match connect_to_protocol::<factivity::ProviderMarker>() {
        Ok(proxy) => proxy,
        Err(e) => {
            warn!("Failed to connect to {}: {:?}", factivity::ProviderMarker::DEBUG_NAME, e);
            return None;
        }
    };

    match ActivityListener::new(proxy) {
        Ok(a) => Some(a),
        Err(e) => {
            warn!("Failed to create activity listener: {:?}", e);
            None
        }
    }
}

#[derive(Debug, Default, FromArgs, PartialEq)]
/// metrics-logger
struct CmdArgs {
    #[argh(switch)]
    /// whether or not to run the component in standalone mode. In standalone
    /// mode access to fuchsia.metricslogger.test protocols is disabled and
    /// the component collects logs forever at a preconfigured interval.
    standalone: bool,
}

#[fuchsia::main(logging_tags = ["metrics-logger"])]
async fn main() {
    // v2 components can't surface stderr yet, so we need to explicitly log errors.
    match inner_main(argh::from_env()).await {
        Err(err) => error!(err:%; "Terminated with error"),
        Ok(()) => info!("Terminated with Ok(())"),
    }
}

async fn inner_main(args: CmdArgs) -> Result<()> {
    info!("Starting metrics logger");

    // Set up tracing
    fuchsia_trace_provider::trace_provider_create_with_fdio();

    // Required call to serve the inspect tree
    let inspector = inspect::component::inspector();
    let _inspect_server_task =
        inspect_runtime::publish(inspector, inspect_runtime::PublishOptions::default());

    // Construct the server, and begin serving.
    let config: Option<json::Value> = std::fs::File::open(CONFIG_PATH)
        .ok()
        .and_then(|file| json::from_reader(std::io::BufReader::new(file)).ok());
    let server = ServerBuilder::new_from_json(config).build().await?;

    if args.standalone {
        info!("Running in standalone mode");
        server
            .run_logging_standalone(
                STANDALONE_SAMPLING_INTERVAL,
                STANDALONE_STATISTICS_INTERVAL,
                None,
            )
            .await;
    } else {
        info!("Running test protocols");
        let mut fs = ServiceFs::new_local();

        // Allow our services to be discovered.
        fs.take_and_serve_directory_handle()?;

        fs.dir("svc").add_fidl_service(move |stream: fmetrics::RecorderRequestStream| {
            MetricsLoggerServer::handle_new_service_connection(server.clone(), stream).detach();
        });

        // This future never completes.
        fs.collect::<()>().await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu_load_logger::tests::create_cpu_stats_driver;
    use crate::gpu_usage_logger::tests::create_gpu_drivers;
    use crate::sensor_logger::tests::{create_power_drivers, create_temperature_drivers};
    use assert_matches::assert_matches;
    use diagnostics_assertions::assert_data_tree;
    use fmetrics::{
        CpuLoad, GpuUsage, Metric, NetworkActivity, Power, StatisticsArgs, Temperature,
    };
    use futures::task::Poll;

    // A helper struct to create Fuchsia Executor and optionally add drivers for different logging
    // request tests.
    struct RunnerBuilder {
        executor: fasync::TestExecutor,
        temperature_drivers: Option<Vec<TemperatureDriver>>,
        power_drivers: Option<Vec<PowerDriver>>,
        gpu_drivers: Option<Vec<GpuDriver>>,
        cpu_stats_driver: Option<CpuStatsDriver>,
        network_drivers: Option<Vec<fhwnet::DeviceProxy>>,
    }

    impl RunnerBuilder {
        fn new() -> Self {
            // Fuchsia Executor must be created first.
            let executor = fasync::TestExecutor::new_with_fake_time();
            executor.set_fake_time(fasync::MonotonicInstant::from_nanos(0));

            Self {
                executor,
                temperature_drivers: None,
                power_drivers: None,
                gpu_drivers: None,
                cpu_stats_driver: None,
                network_drivers: None,
            }
        }

        fn with_temperature_drivers(mut self, temperature_drivers: Vec<TemperatureDriver>) -> Self {
            self.temperature_drivers = Some(temperature_drivers);
            self
        }

        fn with_power_drivers(mut self, power_drivers: Vec<PowerDriver>) -> Self {
            self.power_drivers = Some(power_drivers);
            self
        }

        fn with_gpu_drivers(mut self, gpu_drivers: Vec<GpuDriver>) -> Self {
            self.gpu_drivers = Some(gpu_drivers);
            self
        }

        fn with_cpu_stats_driver(mut self, cpu_stats_driver: CpuStatsDriver) -> Self {
            self.cpu_stats_driver = Some(cpu_stats_driver);
            self
        }

        fn with_network_drivers(mut self, network_drivers: Vec<fhwnet::DeviceProxy>) -> Self {
            self.network_drivers = Some(network_drivers);
            self
        }

        /// Builds a Runner.
        fn build(self) -> Runner {
            Runner::new(
                self.executor,
                self.temperature_drivers,
                self.power_drivers,
                self.gpu_drivers,
                self.cpu_stats_driver,
                self.network_drivers,
            )
        }
    }

    struct Runner {
        server: Rc<MetricsLoggerServer>,
        server_task: fasync::Task<()>,
        proxy: fmetrics::RecorderProxy,

        inspector: inspect::Inspector,

        // Fields are dropped in declaration order. Always drop executor last because we hold other
        // zircon objects tied to the executor in this struct, and those can't outlive the executor.
        //
        // See
        // - https://fuchsia-docs.firebaseapp.com/rust/fuchsia_async/struct.TestExecutor.html
        // - https://doc.rust-lang.org/reference/destructors.html.
        executor: fasync::TestExecutor,
    }

    impl Runner {
        fn new(
            mut executor: fasync::TestExecutor,
            temperature_drivers: Option<Vec<TemperatureDriver>>,
            power_drivers: Option<Vec<PowerDriver>>,
            gpu_drivers: Option<Vec<GpuDriver>>,
            cpu_stats_driver: Option<CpuStatsDriver>,
            network_drivers: Option<Vec<fhwnet::DeviceProxy>>,
        ) -> Self {
            let inspector = inspect::Inspector::default();

            // Build the server.
            let mut builder =
                ServerBuilder::new_from_json(None).with_inspect_root(inspector.root());

            builder = match temperature_drivers {
                Some(drivers) => builder.with_temperature_drivers(drivers),
                None => builder,
            };

            builder = match power_drivers {
                Some(drivers) => builder.with_power_drivers(drivers),
                None => builder,
            };

            builder = match gpu_drivers {
                Some(drivers) => builder.with_gpu_drivers(drivers),
                None => builder,
            };

            builder = match cpu_stats_driver {
                Some(driver) => builder.with_cpu_stats_driver(driver),
                None => builder,
            };

            builder = match network_drivers {
                Some(devices) => builder.with_network_devices(devices),
                None => builder,
            };

            let poll = executor.run_until_stalled(&mut builder.build().boxed_local());
            let server = match poll {
                Poll::Ready(Ok(server)) => server,
                _ => panic!("Failed to build MetricsLoggerServer"),
            };

            // Construct the server task.
            let (proxy, stream) =
                fidl::endpoints::create_proxy_and_stream::<fmetrics::RecorderMarker>();
            let server_task = server.clone().handle_new_service_connection(stream);

            Self { executor, server, server_task, proxy, inspector }
        }

        // If the server has an active logging task, run until the next log and return true.
        // Otherwise, return false.
        fn iterate_logging_task(&mut self) -> bool {
            let Some(next_time) = fasync::TestExecutor::next_timer() else { return false };
            self.executor.set_fake_time(next_time);
            assert_eq!(
                futures::task::Poll::Pending,
                self.executor.run_until_stalled(&mut self.server_task)
            );
            true
        }

        fn run_server_task_until_stalled(&mut self) {
            assert_matches!(self.executor.run_until_stalled(&mut self.server_task), Poll::Pending);
        }

        fn run_logging_standalone(&mut self) {
            let server = self.server.clone();
            self.server_task = fasync::Task::local(async move {
                server
                    .run_logging_standalone(
                        STANDALONE_SAMPLING_INTERVAL,
                        STANDALONE_STATISTICS_INTERVAL,
                        None,
                    )
                    .await;
            });
        }
    }

    #[test]
    fn test_spawn_client_tasks() {
        let runner_builder = RunnerBuilder::new();

        let (_temperature_driver_tasks, temperature_drivers, cpu_temperature, gpu_temperature) =
            create_temperature_drivers();
        let (_cpu_stats_driver_tasks, cpu_stats_driver, _) = create_cpu_stats_driver();

        let mut runner = runner_builder
            .with_temperature_drivers(temperature_drivers)
            .with_cpu_stats_driver(cpu_stats_driver)
            .build();

        // Check the root Inspect node for MetricsLogger is created.
        assert_data_tree!(
            @executor runner.executor,
            runner.inspector,
            root: {
                MetricsLogger: {
                }
            }
        );

        // Create a logging request.
        let mut _query = runner.proxy.start_logging(
            "test",
            &[
                Metric::CpuLoad(CpuLoad { interval_ms: 99 }),
                Metric::Temperature(Temperature {
                    sampling_interval_ms: 100,
                    statistics_args: None,
                }),
            ],
            1000,
            false,
            false,
        );

        // Run `server_task` until stalled to create futures for logging temperatures and CpuLoads.
        runner.run_server_task_until_stalled();

        // Check the Inspect nodes for the loggers are created.
        assert_data_tree!(
            @executor runner.executor,
            runner.inspector,
            root: {
                MetricsLogger: {
                    test: {
                        CpuLoadLogger: {
                        },
                        TemperatureLogger: {
                        }
                    }
                }
            }
        );

        cpu_temperature.set(35.0);
        gpu_temperature.set(45.0);

        // Run the initial logging tasks.
        for _ in 0..2 {
            assert_eq!(runner.iterate_logging_task(), true);
        }

        // Check data is logged to Inspect.
        assert_data_tree!(
            @executor runner.executor,
            runner.inspector,
            root: {
                MetricsLogger: {
                    test: {
                        CpuLoadLogger: {
                            "elapsed time (ms)": 99i64,
                            "Cluster 0": {
                                "Max perf scale": 0.5,
                                "CPU usage (%)": 100.0,
                            },
                            "Cluster 1": {
                                "Max perf scale": 1.0,
                                "CPU usage (%)": 100.0,
                            }
                        },
                        TemperatureLogger: {
                            "elapsed time (ms)": 100i64,
                            "cpu": {
                                "data (°C)": 35.0,
                            },
                            "audio_alias": {
                                "data (°C)": 45.0,
                            }
                        }
                    }
                }
            }
        );

        // Run the remaining logging tasks (8 CpuLoad tasks + 8 Temperature tasks).
        for _ in 0..16 {
            assert_eq!(runner.iterate_logging_task(), true);
        }

        // Check data is logged to Inspect.
        assert_data_tree!(
            @executor runner.executor,
            runner.inspector,
            root: {
                MetricsLogger: {
                    test: {
                        CpuLoadLogger: {
                            "elapsed time (ms)": 891i64,
                            "Cluster 0": {
                                "Max perf scale": 0.5,
                                "CPU usage (%)": 100.0,
                            },
                            "Cluster 1": {
                                "Max perf scale": 1.0,
                                "CPU usage (%)": 100.0,
                            }
                        },
                        TemperatureLogger: {
                            "elapsed time (ms)": 900i64,
                            "cpu": {
                                "data (°C)": 35.0,
                            },
                            "audio_alias": {
                                "data (°C)": 45.0,
                            }
                        }
                    }
                }
            }
        );

        // Run the last 2 tasks which hits `now >= self.end_time` and ends the logging.
        for _ in 0..3 {
            assert_eq!(runner.iterate_logging_task(), true);
        }

        // Check Inspect node for the client is removed.
        assert_data_tree!(
            @executor runner.executor,
            runner.inspector,
            root: {
                MetricsLogger: {
                }
            }
        );

        assert_eq!(runner.iterate_logging_task(), false);
    }

    /// Tests that well-formed alias JSON does not panic the `new_from_json` function.
    #[test]
    fn test_new_from_json() {
        // Test config file for one sensor.
        let json_data = json::json!({
            "power_drivers": [{
                "sensor_name": "power_1",
                "alias": "power"
            }]
        });
        let _ = ServerBuilder::new_from_json(Some(json_data));

        // Test config file for two sensors.
        let json_data = json::json!({
            "temperature_drivers": [{
                "sensor_name": "temp_1",
                "alias": "temp"
            }],
            "power_drivers": [{
                "sensor_name": "power_1",
                "alias": "power"
            }]
        });
        let _ = ServerBuilder::new_from_json(Some(json_data));
    }

    #[test]
    fn test_logging_duration() {
        let runner_builder = RunnerBuilder::new();

        let (_cpu_stats_driver_tasks, cpu_stats_driver, _) = create_cpu_stats_driver();

        let mut runner = runner_builder.with_cpu_stats_driver(cpu_stats_driver).build();

        // Start logging every 100ms for a total of 2000ms.
        let _query = runner.proxy.start_logging(
            "test",
            &[Metric::CpuLoad(CpuLoad { interval_ms: 100 })],
            2000,
            false,
            false,
        );
        runner.run_server_task_until_stalled();

        // Ensure that we get exactly 20 samples.
        for _ in 0..20 {
            assert_eq!(runner.iterate_logging_task(), true);
        }
        assert_eq!(runner.iterate_logging_task(), false);
    }

    #[test]
    fn test_duplicated_metrics_in_one_request() {
        let mut runner = RunnerBuilder::new().build();

        // Attempt to start logging CPU Load twice. The request should fail as the logging request
        // contains duplicated metric type.
        let mut query = runner.proxy.start_logging(
            "test",
            &[
                Metric::CpuLoad(CpuLoad { interval_ms: 100 }),
                Metric::CpuLoad(CpuLoad { interval_ms: 100 }),
            ],
            200,
            false,
            false,
        );
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::DuplicatedMetric)))
        );
    }

    #[test]
    fn test_logging_forever() {
        let runner_builder = RunnerBuilder::new();

        let (_cpu_stats_driver_tasks, cpu_stats_driver, _) = create_cpu_stats_driver();

        let mut runner = runner_builder.with_cpu_stats_driver(cpu_stats_driver).build();

        // Start logging every 100ms with no predetermined end time.
        let _query = runner.proxy.start_logging_forever(
            "test",
            &[Metric::CpuLoad(CpuLoad { interval_ms: 100 })],
            false,
            false,
        );
        runner.run_server_task_until_stalled();

        // Samples should continue forever. Obviously we can't check infinitely many samples, but
        // we can check that they don't stop for a relatively large number of iterations.
        for _ in 0..1000 {
            assert_eq!(runner.iterate_logging_task(), true);
        }

        let mut query = runner.proxy.stop_logging("test");
        assert_matches!(runner.executor.run_until_stalled(&mut query), Poll::Ready(Ok(true)));

        // Check that we can start another request for the same client_id after
        // `stop_logging` is called.
        let mut query = runner.proxy.start_logging_forever(
            "test",
            &[Metric::CpuLoad(CpuLoad { interval_ms: 100 })],
            false,
            false,
        );
        assert_matches!(runner.executor.run_until_stalled(&mut query), Poll::Ready(Ok(Ok(()))));
    }

    #[test]
    fn test_stop_logging() {
        let runner_builder = RunnerBuilder::new();

        let (_temperature_driver_tasks, temperature_drivers, cpu_temperature, gpu_temperature) =
            create_temperature_drivers();

        let mut runner = runner_builder.with_temperature_drivers(temperature_drivers).build();

        let _query = runner.proxy.start_logging_forever(
            "test",
            &[Metric::Temperature(Temperature {
                sampling_interval_ms: 100,
                statistics_args: None,
            })],
            false,
            false,
        );
        runner.run_server_task_until_stalled();

        // Check logger added to client before first temperature poll.
        assert_data_tree!(
            @executor runner.executor,
            runner.inspector,
            root: {
                MetricsLogger: {
                    test: {
                        TemperatureLogger: {
                        }
                    }
                }
            }
        );

        cpu_temperature.set(35.0);
        gpu_temperature.set(45.0);

        // Run a few logging tasks to populate Inspect node before we test `stop_logging`.
        for _ in 0..10 {
            assert_eq!(runner.iterate_logging_task(), true);
        }

        // Checked data populated to Inspect node.
        assert_data_tree!(
            @executor runner.executor,
            runner.inspector,
            root: {
                MetricsLogger: {
                    test: {
                        TemperatureLogger: {
                            "elapsed time (ms)": 1_000i64,
                            "cpu": {
                                "data (°C)": 35.0,
                            },
                            "audio_alias": {
                                "data (°C)": 45.0,
                            }
                        }
                    }
                }
            }
        );

        let mut query = runner.proxy.stop_logging("test");
        assert_matches!(runner.executor.run_until_stalled(&mut query), Poll::Ready(Ok(true)));
        runner.run_server_task_until_stalled();

        assert_eq!(runner.iterate_logging_task(), false);

        // Check temperature logger removed in Inspect.
        assert_data_tree!(
            @executor runner.executor,
            runner.inspector,
            root: {
                MetricsLogger: {}
            }
        );
    }

    #[test]
    fn test_multi_clients() {
        let runner_builder = RunnerBuilder::new();

        let (_temperature_driver_tasks, temperature_drivers, cpu_temperature, gpu_temperature) =
            create_temperature_drivers();
        let (_cpu_stats_driver_tasks, cpu_stats_driver, _) = create_cpu_stats_driver();

        let mut runner = runner_builder
            .with_temperature_drivers(temperature_drivers)
            .with_cpu_stats_driver(cpu_stats_driver)
            .build();

        // Create a request for logging CPU load and Temperature.
        let _query1 = runner.proxy.start_logging(
            "test1",
            &[
                Metric::CpuLoad(CpuLoad { interval_ms: 300 }),
                Metric::Temperature(Temperature {
                    sampling_interval_ms: 199,
                    statistics_args: None,
                }),
            ],
            500,
            false,
            false,
        );

        // Create a request for logging Temperature.
        let _query2 = runner.proxy.start_logging(
            "test2",
            &[Metric::Temperature(Temperature {
                sampling_interval_ms: 200,
                statistics_args: None,
            })],
            300,
            false,
            false,
        );
        runner.run_server_task_until_stalled();

        // Check TemperatureLogger added before first temperature poll.
        assert_data_tree!(
            @executor runner.executor,
            runner.inspector,
            root: {
                MetricsLogger: {
                    test1: {
                        CpuLoadLogger: {
                        },
                        TemperatureLogger: {
                        }
                    },
                    test2: {
                        TemperatureLogger: {
                        }
                    }
                }
            }
        );

        cpu_temperature.set(35.0);
        gpu_temperature.set(45.0);

        // Run the first task which is the first logging task for client `test1`.
        assert_eq!(runner.iterate_logging_task(), true);

        // Check temperature data in Inspect.
        assert_data_tree!(
            @executor runner.executor,
            runner.inspector,
            root: {
                MetricsLogger: {
                    test1: {
                        CpuLoadLogger: {
                        },
                        TemperatureLogger: {
                            "elapsed time (ms)": 199i64,
                            "cpu": {
                                "data (°C)": 35.0,
                            },
                            "audio_alias": {
                                "data (°C)": 45.0,
                            }
                        }
                    },
                    test2: {
                        TemperatureLogger: {
                        }
                    }
                }
            }
        );

        // Set new temperature data.
        cpu_temperature.set(36.0);
        gpu_temperature.set(46.0);

        assert_eq!(runner.iterate_logging_task(), true);

        // Check `test1` data remaining the same, `test2` data updated.
        assert_data_tree!(
            @executor runner.executor,
            runner.inspector,
            root: {
                MetricsLogger: {
                    test1: {
                        CpuLoadLogger: {
                        },
                        TemperatureLogger: {
                            "elapsed time (ms)": 199i64,
                            "cpu": {
                                "data (°C)": 35.0,
                            },
                            "audio_alias": {
                                "data (°C)": 45.0,
                            }
                        }
                    },
                    test2: {
                        TemperatureLogger: {
                            "elapsed time (ms)": 200i64,
                            "cpu": {
                                "data (°C)": 36.0,
                            },
                            "audio_alias": {
                                "data (°C)": 46.0,
                            }
                        }
                    }
                }
            }
        );

        assert_eq!(runner.iterate_logging_task(), true);

        // Check `test2` data remaining the same, `test1` data updated.
        assert_data_tree!(
            @executor runner.executor,
            runner.inspector,
            root: {
                MetricsLogger: {
                    test1: {
                        CpuLoadLogger: {
                            "elapsed time (ms)": 300i64,
                            "Cluster 0": {
                                "Max perf scale": 0.5,
                                "CPU usage (%)": 100.0,
                            },
                            "Cluster 1": {
                                "Max perf scale": 1.0,
                                "CPU usage (%)": 100.0,
                            }
                        },
                        TemperatureLogger: {
                            "elapsed time (ms)": 199i64,
                            "cpu": {
                                "data (°C)": 35.0,
                            },
                            "audio_alias": {
                                "data (°C)": 45.0,
                            }
                        }
                    },
                    test2: {
                        TemperatureLogger: {
                            "elapsed time (ms)": 200i64,
                            "cpu": {
                                "data (°C)": 36.0,
                            },
                            "audio_alias": {
                                "data (°C)": 46.0,
                            }
                        }
                    }
                }
            }
        );

        assert_eq!(runner.iterate_logging_task(), true);

        // Check `test1` data updated, `test2` data remaining the same.
        assert_data_tree!(
            @executor runner.executor,
            runner.inspector,
            root: {
                MetricsLogger: {
                    test1: {
                        CpuLoadLogger: {
                            "elapsed time (ms)": 300i64,
                            "Cluster 0": {
                                "Max perf scale": 0.5,
                                "CPU usage (%)": 100.0,
                            },
                            "Cluster 1": {
                                "Max perf scale": 1.0,
                                "CPU usage (%)": 100.0,
                            }
                        },
                        TemperatureLogger: {
                            "elapsed time (ms)": 398i64,
                            "cpu": {
                                "data (°C)": 36.0,
                            },
                            "audio_alias": {
                                "data (°C)": 46.0,
                            }
                        }
                    },
                    test2: {
                        TemperatureLogger: {
                            "elapsed time (ms)": 200i64,
                            "cpu": {
                                "data (°C)": 36.0,
                            },
                            "audio_alias": {
                                "data (°C)": 46.0,
                            }
                        }
                    }
                }
            }
        );

        // Run the remaining 3 tasks.
        for _ in 0..3 {
            assert_eq!(runner.iterate_logging_task(), true);
        }
        assert_eq!(runner.iterate_logging_task(), false);

        assert_data_tree!(
            @executor runner.executor,
            runner.inspector,
            root: {
                MetricsLogger: {}
            }
        );
    }

    #[test]
    fn test_large_number_of_clients() {
        let runner_builder = RunnerBuilder::new();

        let (_cpu_stats_driver_tasks, cpu_stats_driver, _) = create_cpu_stats_driver();

        let mut runner = runner_builder.with_cpu_stats_driver(cpu_stats_driver).build();

        // Create MAX_CONCURRENT_CLIENTS clients.
        for i in 0..MAX_CONCURRENT_CLIENTS {
            let mut query = runner.proxy.start_logging_forever(
                &(i as u32).to_string(),
                &[Metric::CpuLoad(CpuLoad { interval_ms: 300 })],
                false,
                false,
            );
            assert_matches!(runner.executor.run_until_stalled(&mut query), Poll::Ready(Ok(Ok(()))));
            runner.run_server_task_until_stalled();
        }

        // Check new client logging request returns TOO_MANY_ACTIVE_CLIENTS error.
        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::CpuLoad(CpuLoad { interval_ms: 100 })],
            400,
            false,
            false,
        );
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::TooManyActiveClients)))
        );

        // Remove one active client.
        let mut query = runner.proxy.stop_logging("3");
        assert_matches!(runner.executor.run_until_stalled(&mut query), Poll::Ready(Ok(true)));

        // Check we can add another client.
        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::CpuLoad(CpuLoad { interval_ms: 100 })],
            400,
            false,
            false,
        );
        assert_matches!(runner.executor.run_until_stalled(&mut query), Poll::Ready(Ok(Ok(()))));
    }

    #[test]
    fn test_already_logging() {
        let runner_builder = RunnerBuilder::new();

        let (_temperature_driver_tasks, temperature_drivers, _cpu_temperature, _gpu_temperature) =
            create_temperature_drivers();
        let (_cpu_stats_driver_tasks, cpu_stats_driver, _) = create_cpu_stats_driver();

        let mut runner = runner_builder
            .with_temperature_drivers(temperature_drivers)
            .with_cpu_stats_driver(cpu_stats_driver)
            .build();

        // Start the first logging task.
        let _query = runner.proxy.start_logging(
            "test",
            &[Metric::CpuLoad(CpuLoad { interval_ms: 100 })],
            400,
            false,
            false,
        );

        runner.run_server_task_until_stalled();

        assert_eq!(runner.iterate_logging_task(), true);

        // Attempt to start another task for logging the same metric while the first one is still
        // running. The request to start should fail.
        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::CpuLoad(CpuLoad { interval_ms: 100 })],
            400,
            false,
            false,
        );
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::AlreadyLogging)))
        );

        // Attempt to start another task for logging a different metric while the first one is
        // running. The request to start should fail.
        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::Temperature(Temperature {
                sampling_interval_ms: 100,
                statistics_args: Some(Box::new(StatisticsArgs { statistics_interval_ms: 100 })),
            })],
            200,
            false,
            false,
        );
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::AlreadyLogging)))
        );

        // Starting a new logging task of a different client should succeed.
        let mut query = runner.proxy.start_logging(
            "test2",
            &[Metric::Temperature(Temperature {
                sampling_interval_ms: 500,
                statistics_args: Some(Box::new(StatisticsArgs { statistics_interval_ms: 500 })),
            })],
            1000,
            false,
            false,
        );
        assert_matches!(runner.executor.run_until_stalled(&mut query), Poll::Ready(Ok(Ok(()))));

        // Run logging tasks of the first client to completion.
        for _ in 0..4 {
            assert_eq!(runner.iterate_logging_task(), true);
        }

        // Starting a new logging task of the first client should succeed now.
        let _query = runner.proxy.start_logging(
            "test",
            &[Metric::Temperature(Temperature {
                sampling_interval_ms: 100,
                statistics_args: Some(Box::new(StatisticsArgs { statistics_interval_ms: 100 })),
            })],
            200,
            false,
            false,
        );
        runner.run_server_task_until_stalled();

        // Starting a new logging task of the second client should still fail.
        let mut query = runner.proxy.start_logging(
            "test2",
            &[Metric::Temperature(Temperature {
                sampling_interval_ms: 100,
                statistics_args: Some(Box::new(StatisticsArgs { statistics_interval_ms: 100 })),
            })],
            200,
            false,
            false,
        );
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::AlreadyLogging)))
        );
    }

    #[test]
    fn test_multiple_stops_ok() {
        let runner_builder = RunnerBuilder::new();

        let (_cpu_stats_driver_tasks, cpu_stats_driver, _) = create_cpu_stats_driver();

        let mut runner = runner_builder.with_cpu_stats_driver(cpu_stats_driver).build();

        let mut query = runner.proxy.stop_logging("test");
        assert_matches!(runner.executor.run_until_stalled(&mut query), Poll::Ready(Ok(false)));

        let mut query = runner.proxy.stop_logging("test");
        assert_matches!(runner.executor.run_until_stalled(&mut query), Poll::Ready(Ok(false)));

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::CpuLoad(CpuLoad { interval_ms: 100 })],
            200,
            false,
            false,
        );
        assert_matches!(runner.executor.run_until_stalled(&mut query), Poll::Ready(Ok(Ok(()))));
        runner.run_server_task_until_stalled();

        let mut query = runner.proxy.stop_logging("test");
        assert_matches!(runner.executor.run_until_stalled(&mut query), Poll::Ready(Ok(true)));
        let mut query = runner.proxy.stop_logging("test");
        assert_matches!(runner.executor.run_until_stalled(&mut query), Poll::Ready(Ok(false)));
        let mut query = runner.proxy.stop_logging("test");
        assert_matches!(runner.executor.run_until_stalled(&mut query), Poll::Ready(Ok(false)));
    }

    #[test]
    fn test_logging_cpu_stats_request_errors() {
        let runner_builder = RunnerBuilder::new();

        let (_cpu_stats_driver_tasks, cpu_stats_driver, _) = create_cpu_stats_driver();

        let mut runner = runner_builder.with_cpu_stats_driver(cpu_stats_driver).build();

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::CpuLoad(CpuLoad { interval_ms: 0 })],
            200,
            false,
            false,
        );
        // Check `InvalidSamplingInterval` is returned when interval_ms is 0.
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::InvalidSamplingInterval)))
        );

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::CpuLoad(CpuLoad { interval_ms: 200 })],
            1_000,
            true,
            false,
        );
        // Check `InvalidSamplingInterval` is returned when logging samples to syslog at an interval
        // smaller than MIN_INTERVAL_FOR_SYSLOG_MS.
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::InvalidSamplingInterval)))
        );

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::CpuLoad(CpuLoad { interval_ms: 200 })],
            100,
            false,
            false,
        );
        // Check `InvalidSamplingInterval` is returned when logging samples to syslog at an interval
        // larger than `duration_ms`.
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::InvalidSamplingInterval)))
        );
    }

    #[test]
    fn test_logging_cpu_stats_request_dispatch() {
        let runner_builder = RunnerBuilder::new();

        let (_cpu_stats_driver_tasks, cpu_stats_driver, _) = create_cpu_stats_driver();

        let mut runner = runner_builder.with_cpu_stats_driver(cpu_stats_driver).build();

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::CpuLoad(CpuLoad { interval_ms: 200 })],
            1_000,
            false,
            false,
        );

        // Check the request is dispatched without error.
        assert_matches!(runner.executor.run_until_stalled(&mut query), Poll::Ready(Ok(Ok(()))));
    }

    #[test]
    fn test_logging_gpu_usage_request_errors() {
        let runner_builder = RunnerBuilder::new();

        let mut runner = runner_builder.build();

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::GpuUsage(GpuUsage { interval_ms: 0 })],
            200,
            false,
            false,
        );
        // Check `InvalidSamplingInterval` is returned when interval_ms is 0.
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::InvalidSamplingInterval)))
        );

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::GpuUsage(GpuUsage { interval_ms: 200 })],
            1_000,
            true,
            false,
        );
        // Check `InvalidSamplingInterval` is returned when logging samples to syslog at an interval
        // smaller than MIN_INTERVAL_FOR_SYSLOG_MS.
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::InvalidSamplingInterval)))
        );

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::GpuUsage(GpuUsage { interval_ms: 200 })],
            100,
            false,
            false,
        );
        // Check `InvalidSamplingInterval` is returned when logging samples to syslog at an interval
        // larger than `duration_ms`.
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::InvalidSamplingInterval)))
        );

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::GpuUsage(GpuUsage { interval_ms: 200 })],
            1_000,
            false,
            false,
        );
        // Check `NoDrivers` is returned when other logging request parameters are correct.
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::NoDrivers)))
        );
    }

    #[test]
    fn test_logging_gpu_usage_request_dispatch() {
        let runner_builder = RunnerBuilder::new();

        let (_gpu_driver_tasks, gpu_drivers, _, _) = create_gpu_drivers();

        let mut runner = runner_builder.with_gpu_drivers(gpu_drivers).build();
        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::GpuUsage(GpuUsage { interval_ms: 200 })],
            1_000,
            false,
            false,
        );
        // Check the request is dispatched without error.
        assert_matches!(runner.executor.run_until_stalled(&mut query), Poll::Ready(Ok(Ok(()))));
    }

    #[test]
    fn test_logging_power_request_errors() {
        let runner_builder = RunnerBuilder::new();

        let mut runner = runner_builder.build();

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::Power(Power {
                sampling_interval_ms: 500,
                statistics_args: Some(Box::new(StatisticsArgs { statistics_interval_ms: 500 })),
            })],
            300,
            false,
            false,
        );

        // Check `InvalidStatisticsInterval` is returned when statistics is enabled and
        // `statistics_interval_ms` is larger than `duration_ms`.
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::InvalidStatisticsInterval)))
        );

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::Power(Power {
                sampling_interval_ms: 600,
                statistics_args: Some(Box::new(StatisticsArgs { statistics_interval_ms: 500 })),
            })],
            800,
            false,
            false,
        );

        // Check `InvalidStatisticsInterval` is returned when statistics is enabled and
        // `statistics_interval_ms` is less than `sampling_interval_ms`.
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::InvalidStatisticsInterval)))
        );

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::Power(Power {
                sampling_interval_ms: 200,
                statistics_args: Some(Box::new(StatisticsArgs { statistics_interval_ms: 200 })),
            })],
            800,
            false,
            true,
        );

        // Check `InvalidStatisticsInterval` is returned when statistics is enabled and
        // `statistics_interval_ms` is less than MIN_INTERVAL_FOR_SYSLOG_MS.
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::InvalidStatisticsInterval)))
        );

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::Power(Power { sampling_interval_ms: 0, statistics_args: None })],
            800,
            false,
            false,
        );
        // Check `InvalidSamplingInterval` is returned when sampling_interval_ms is 0.
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::InvalidSamplingInterval)))
        );

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::Power(Power { sampling_interval_ms: 200, statistics_args: None })],
            800,
            true,
            false,
        );
        // Check `InvalidSamplingInterval` is returned when logging samples to syslog at an interval
        // smaller than MIN_INTERVAL_FOR_SYSLOG_MS.
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::InvalidSamplingInterval)))
        );

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::Power(Power { sampling_interval_ms: 200, statistics_args: None })],
            100,
            false,
            false,
        );
        // Check `InvalidSamplingInterval` is returned when logging samples to syslog at an interval
        // larger than `duration_ms`.
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::InvalidSamplingInterval)))
        );

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::Power(Power { sampling_interval_ms: 200, statistics_args: None })],
            1_000,
            false,
            false,
        );
        // Check `NoDrivers` is returned when other logging request parameters are correct.
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::NoDrivers)))
        );
    }

    #[test]
    fn test_logging_power_request_dispatch() {
        let runner_builder = RunnerBuilder::new();

        let (_power_driver_tasks, power_drivers, _, _) = create_power_drivers();

        let mut runner = runner_builder.with_power_drivers(power_drivers).build();

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::Power(Power { sampling_interval_ms: 200, statistics_args: None })],
            1_000,
            false,
            false,
        );
        // Check the request is dispatched without error.
        assert_matches!(runner.executor.run_until_stalled(&mut query), Poll::Ready(Ok(Ok(()))));
    }

    #[test]
    fn test_logging_network_activity_request_errors() {
        let runner_builder = RunnerBuilder::new();

        let mut runner = runner_builder.build();

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::NetworkActivity(NetworkActivity { interval_ms: 0 })],
            200,
            false,
            false,
        );
        // Check `InvalidSamplingInterval` is returned when interval_ms is 0.
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::InvalidSamplingInterval)))
        );

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::NetworkActivity(NetworkActivity { interval_ms: 200 })],
            1_000,
            true,
            false,
        );
        // Check `InvalidSamplingInterval` is returned when logging samples to syslog at an interval
        // smaller than MIN_INTERVAL_FOR_SYSLOG_MS.
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::InvalidSamplingInterval)))
        );

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::NetworkActivity(NetworkActivity { interval_ms: 200 })],
            100,
            false,
            false,
        );
        // Check `InvalidSamplingInterval` is returned when logging samples to syslog at an interval
        // larger than `duration_ms`.
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::InvalidSamplingInterval)))
        );

        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::NetworkActivity(NetworkActivity { interval_ms: 200 })],
            1_000,
            false,
            false,
        );
        // Check `NoDrivers` is returned when other logging request parameters are correct.
        assert_matches!(
            runner.executor.run_until_stalled(&mut query),
            Poll::Ready(Ok(Err(fmetrics::RecorderError::NoDrivers)))
        );
    }

    #[test]
    fn test_logging_network_activity_request_dispatch() {
        let runner_builder = RunnerBuilder::new();

        let (proxy, _) = fidl::endpoints::create_proxy_and_stream::<fhwnet::DeviceMarker>();
        let network_drivers = vec![proxy];

        let mut runner = runner_builder.with_network_drivers(network_drivers).build();
        let mut query = runner.proxy.start_logging(
            "test",
            &[Metric::NetworkActivity(NetworkActivity { interval_ms: 200 })],
            1_000,
            false,
            false,
        );
        // Check the request is dispatched without error.
        assert_matches!(runner.executor.run_until_stalled(&mut query), Poll::Ready(Ok(Ok(()))));
    }

    #[test]
    fn test_logging_standalone_runs() {
        let runner_builder = RunnerBuilder::new();

        let (_power_driver_tasks, power_drivers, power1, power2) = create_power_drivers();
        power1.set(14.0);
        power2.set(106.0);

        let (_temperature_driver_tasks, temperature_drivers, cpu_temperature, gpu_temperature) =
            create_temperature_drivers();

        // Set new temperature data.
        cpu_temperature.set(21.0);
        gpu_temperature.set(52.0);

        let mut runner = runner_builder
            .with_power_drivers(power_drivers)
            .with_temperature_drivers(temperature_drivers)
            .build();

        // Check the task is run without error.
        runner.run_logging_standalone();
        runner.run_server_task_until_stalled();

        // Samples should continue forever. Obviously we can't check infinitely many samples, but
        // we can check that they don't stop for a relatively large number of iterations.
        for _ in 0..1000 {
            assert_eq!(runner.iterate_logging_task(), true);
        }

        // Check that inspect data is updated.
        assert_data_tree!(
            @executor runner.executor,
            runner.inspector,
            root: {
                MetricsLogger: {
                    "metrics-logger-standalone": {
                        TemperatureLogger: contains {
                            "cpu": contains {
                                "data (°C)": 21.0,
                                "statistics": contains {
                                    "min (°C)": 21.0,
                                    "max (°C)": 21.0,
                                    "average (°C)": 21.0,
                                    "median (°C)": 21.0,
                                }
                            },
                            "audio_alias": contains {
                                "data (°C)": 52.0,
                                "statistics": contains {
                                    "min (°C)": 52.0,
                                    "max (°C)": 52.0,
                                    "average (°C)": 52.0,
                                    "median (°C)": 52.0,
                                }
                            }
                        },
                        PowerLogger: contains {
                            "power_1": contains {
                                "data (W)": 14.0,
                                "statistics": contains {
                                    "min (W)": 14.0,
                                    "max (W)": 14.0,
                                    "average (W)": 14.0,
                                    "median (W)": 14.0,
                                }
                            },
                            "power_alias": contains {
                                "data (W)": 106.0,
                                "statistics": contains {
                                    "min (W)": 106.0,
                                    "max (W)": 106.0,
                                    "average (W)": 106.0,
                                    "median (W)": 106.0,
                                }
                            },
                        },
                    }
                }
            }
        );
    }
}
