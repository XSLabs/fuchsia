// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{anyhow, Result};
use async_lock::Mutex;
use fidl::endpoints::{create_proxy, DiscoverableProtocolMarker};
use fidl_fuchsia_developer_remotecontrol::RemoteControlProxy;
use fidl_test_proxy_stress::{StressorMarker, StressorProxy};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use {fidl_fuchsia_sys2 as fsys, fuchsia_async as fasync};

/// Effectively arbitrarily high timeout. We don't use Duration::MAX here to avoid
/// overflow errors in underlying libraries.
const TARGET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60 * 60 * 24);
const STRESSOR_URL: &str =
    "fuchsia-pkg://fuchsia.com/ffx_connection_test_components#meta/proxy_stressor.cm";

/// A reference to a launched component on the target device. Used to tear down the component
/// when the test completes.
struct LaunchedComponent {
    moniker: String,
}

/// Helper for creating proxies to a launched component on the target device.
pub struct LaunchedComponentConnector {
    nodename: String,
    moniker: String,
    node: Arc<overnet_core::Router>,
    rcs_proxy: RemoteControlProxy,
    ascendd_path: PathBuf,
    daemon_tasks: Mutex<Vec<fasync::Task<Result<()>>>>,
    env_context: ffx_config::EnvironmentContext,
}

impl LaunchedComponent {
    async fn destroy(self, ffx: &ffx_isolate::Isolate) -> Result<()> {
        ffx.ffx(&["component", "destroy", &self.moniker]).await?;
        Ok(())
    }
}

impl LaunchedComponentConnector {
    async fn connect_with_rcs_proxy(
        rcs_proxy: &RemoteControlProxy,
        moniker: &str,
    ) -> Result<StressorProxy> {
        loop {
            // Try to connect via fuchsia.developer.remotecontrol/RemoteControl.ConnectCapability.
            let (proxy, server) = create_proxy::<StressorMarker>();
            if let Ok(response) = rcs_proxy
                .connect_capability(
                    &moniker,
                    fsys::OpenDirType::ExposedDir,
                    StressorMarker::PROTOCOL_NAME,
                    server.into_channel(),
                )
                .await
            {
                if response.is_ok() {
                    return Ok(proxy);
                }
            }
            // Fallback to fuchsia.developer.remotecontrol/RemoteControl.DeprecatedOpenCapability.
            // This can be removed once we drop support for API level 27.
            let (proxy, server) = create_proxy::<StressorMarker>();
            match rcs_proxy
                .deprecated_open_capability(
                    &moniker,
                    fsys::OpenDirType::ExposedDir,
                    StressorMarker::PROTOCOL_NAME,
                    server.into_channel(),
                    Default::default(),
                )
                .await
            {
                Ok(_) => return Ok(proxy),
                _ => continue,
            }
        }
    }

    /// Creates a new connection to the component running on the target.
    pub async fn connect(&self) -> Result<StressorProxy> {
        Self::connect_with_rcs_proxy(&self.rcs_proxy, &self.moniker).await
    }

    /// Creates a new connection to the daemon and uses it to connect to the component
    /// running on target.
    pub async fn connect_via_new_daemon_connection(&self) -> Result<StressorProxy> {
        let (rcs_proxy, daemon_task) =
            connect_to_rcs(&self.node, &self.nodename, &self.ascendd_path, &self.env_context)
                .await?;
        self.daemon_tasks.lock().await.push(daemon_task);
        Self::connect_with_rcs_proxy(&rcs_proxy, &self.moniker).await
    }
}

/// Launch an instance of the stressor component on target.
async fn launch(
    name: &str,
    nodename: &str,
    node: Arc<overnet_core::Router>,
    isolate: &ffx_isolate::Isolate,
) -> Result<(LaunchedComponent, LaunchedComponentConnector)> {
    let moniker = format!("/core/ffx-laboratory:{}", name);

    let env_context = isolate.env_context().clone();
    let create_output =
        isolate.ffx(&["-t", nodename, "component", "create", &moniker, STRESSOR_URL]).await?;
    if !create_output.status.success() {
        return Err(anyhow!("Failed to create component: {:?}", create_output));
    }

    let component = LaunchedComponent { moniker: moniker.clone() };

    let start_and_launch_result = async move {
        let output = isolate.ffx(&["-t", nodename, "component", "start", &moniker]).await?;
        if !output.status.success() {
            Err(anyhow!("Failed to start component: {:?}", output))
        } else {
            let ascendd_path = isolate.ascendd_path().to_owned();
            let (rcs_proxy, daemon_task) =
                connect_to_rcs(&node, nodename, &ascendd_path, &env_context).await?;
            Ok(LaunchedComponentConnector {
                nodename: nodename.to_string(),
                moniker,
                node,
                rcs_proxy,
                ascendd_path,
                daemon_tasks: Mutex::new(vec![daemon_task]),
                env_context,
            })
        }
    }
    .await;

    match start_and_launch_result {
        Ok(component_connector) => Ok((component, component_connector)),
        Err(e) => {
            // In case resolve or start fails, destroy the component to cleanup resources.
            let _ = component.destroy(isolate).await;
            Err(e)
        }
    }
}

/// Connects to a daemon running on |ascendd_path| and uses it to connect to RCS on the target.
async fn connect_to_rcs(
    node: &Arc<overnet_core::Router>,
    nodename: &str,
    ascendd_path: &Path,
    context: &ffx_config::EnvironmentContext,
) -> Result<(RemoteControlProxy, fasync::Task<Result<()>>)> {
    let (_node, daemon_proxy, daemon_fut) =
        ffx_daemon::get_daemon_proxy_single_link(&node, ascendd_path.to_owned(), None).await?;
    let daemon_task = fasync::Task::spawn(daemon_fut);
    let rcs_proxy = ffx_target::get_remote_proxy(
        Some(nodename.to_string()),
        daemon_proxy,
        TARGET_TIMEOUT,
        None,
        context,
    )
    .await?;
    Ok((rcs_proxy, daemon_task))
}

/// Test fixture that handles launching and tearing down a test after execution.
pub async fn setup_and_teardown_fixture<F, Fut>(case_name: &str, test_fn: F)
where
    F: FnOnce(LaunchedComponentConnector) -> Fut + Send + 'static,
    Fut: futures::future::Future<Output = ()>,
{
    let node = overnet_core::Router::new(None).expect("creating Overnet node");

    let ssh_path = std::env::var("FUCHSIA_SSH_KEY").unwrap().into();
    let test_env = ffx_config::test_init().await.expect("Setting up test environment");
    let isolate = ffx_isolate::Isolate::new_in_test(case_name, ssh_path, &test_env.context)
        .await
        .expect("create isolate");

    isolate.start_daemon().await.expect("failed to start daemon");

    // Ensure that the address is formatted properly, and include port is if it available.
    // Without this formatting, the connection does not work when using a remote workflow.
    let addr = format!(
        "[{}]{}",
        std::env::var("FUCHSIA_DEVICE_ADDR").unwrap(),
        std::env::var("FUCHSIA_SSH_PORT").map(|v| format!(":{}", v)).unwrap_or_default()
    );
    let nodename = std::env::var("FUCHSIA_NODENAME").unwrap();

    isolate.ffx(&["target", "add", &addr]).await.expect("add target");

    let (launched_component, component_connector) =
        launch(case_name, &nodename, node, &isolate).await.expect("launch component");

    // Spawn a new thread so that we can catch panics from the test. We check completion of
    // the thread using an mpsc channel, so that futures on the original executor continue
    // to be polled while the test runs in a different thread (as opposed to joining using
    // join, which is blocking and prevents any other futures from polling).
    let (done_sender, done) = futures::channel::oneshot::channel();
    let join_handle = std::thread::spawn(move || {
        let mut test_executor = fasync::LocalExecutor::new();
        test_executor.run_singlethreaded(test_fn(component_connector));
        let _ = done_sender.send(());
    });
    let _ = done.await;
    // after the receiver completes we know the test is done, so we can do a blocking join
    // without issue.
    let test_result = join_handle.join();

    let destroy_result = launched_component.destroy(&isolate).await;

    // Test error is a dyn Any. The only way we can display it is by propagating the panic.
    match (test_result, destroy_result) {
        (Ok(()), Ok(())) => (),
        (Err(test_err), Ok(())) => std::panic::resume_unwind(test_err),
        (Ok(()), Err(destroy_err)) => panic!("{}", destroy_err),
        (Err(test_err), Err(destroy_err)) => {
            log::error!("Destroy failed: {}", destroy_err);
            std::panic::resume_unwind(test_err);
        }
    }
}
