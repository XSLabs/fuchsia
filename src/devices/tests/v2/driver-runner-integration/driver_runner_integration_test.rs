// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use component_events::descriptor::EventDescriptor;
use component_events::events::{self, Event, EventStream};
use component_events::matcher::EventMatcher;
use diagnostics_assertions::assert_data_tree;
use diagnostics_reader::ArchiveReader;
use fuchsia_component_test::RealmBuilder;
use fuchsia_driver_test::{DriverTestRealmBuilder, DriverTestRealmInstance};
use futures::StreamExt;
use {fidl_fuchsia_driver_test as fdt, fuchsia_async as fasync};

// This will only return once all `events` are seen in `event_stream`.
// The events do not have to happen in order.
// Extra events will be discarded.
async fn check_events(
    mut events: Vec<EventMatcher>,
    event_stream: &mut events::EventStream,
) -> Result<(), anyhow::Error> {
    while events.len() != 0 {
        let next = event_stream.next().await?;
        let next = EventDescriptor::try_from(&next)?;
        events.retain(|event| event.matches(&next).is_err());
    }
    Ok(())
}

#[fasync::run_singlethreaded(test)]
async fn driver_runner_test() -> Result<(), anyhow::Error> {
    // Set up our expected events.
    let mut started_stream = EventStream::open().await?;

    let builder = RealmBuilder::new().await?;
    // TODO(https://fxbug.dev/42166832): This should be a relative URL but then driver_host.cm doesn't resolve correctly.
    let _ = builder.driver_test_realm_setup().await?;

    let instance = builder.build().await?;

    let args = fdt::RealmArgs {
        root_driver: Some(
            "fuchsia-pkg://fuchsia.com/driver_runner_integration_test#meta/packaged_driver.cm"
                .to_string(),
        ),
        ..Default::default()
    };
    instance.driver_test_realm_start(args).await?;

    let _ = instance.driver_test_realm_connect_to_dev()?;

    // List the components that we expect to be created.
    // We list the components by monikers which are described at:
    // https://fuchsia.dev/fuchsia-src/concepts/components/v2/monikers?hl=en
    // Drivers live in collections, and their monikers will look like:
    //   /boot-drivers:{TOPOLOGICAL_NAME}
    //   /base-drivers:{TOPOLOGICAL_NAME}
    // Driver hosts live in a collection, and their monikers will look like:
    //   /driver-hosts:driver-host-{DRIVER_NUMBER}
    let events = vec![
        EventMatcher::ok().r#type(events::Started::TYPE).moniker_regex(r".*/driver_manager$"),
        EventMatcher::ok().r#type(events::Started::TYPE).moniker_regex(r".*/driver-index$"),
        EventMatcher::ok().r#type(events::Started::TYPE).moniker_regex(r".*/base-drivers:dev$"),
        EventMatcher::ok()
            .r#type(events::Started::TYPE)
            .moniker_regex(r".*/driver-hosts:driver-host-\d+$"),
    ];
    check_events(events, &mut started_stream).await?;
    let reader = ArchiveReader::logs();
    let mut results = reader.snapshot_then_subscribe().unwrap();
    let mut found = false;
    while let Some(result) = results.next().await {
        match result {
            Err(e) => {
                panic!("error in subscription: {}", e);
            }
            Ok(log) => {
                if log.msg().unwrap().contains("Debug world") {
                    panic!("Debug logs shouldn't print by default");
                }
                if log.msg().unwrap().contains("Hello world") {
                    assert_data_tree!(log.payload.as_ref().unwrap(), root:{
                        "keys": {
                            "The answer is": 42i64,
                        },
                        "message": contains {
                        "value": "Hello world",
                    }});
                    found = true;
                    break;
                }
            }
        }
    }
    if !found {
        panic!("hello world not found");
    }
    Ok(())
}
