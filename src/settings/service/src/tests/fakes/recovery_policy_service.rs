// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::tests::fakes::base::Service;
use anyhow::{format_err, Error};
use fidl::endpoints::ServerEnd;
use fidl::prelude::*;
use fuchsia_async as fasync;
use futures::lock::Mutex;
use futures::TryStreamExt;
use std::rc::Rc;

#[derive(Clone)]
pub(crate) struct RecoveryPolicy {
    is_local_reset_allowed: Rc<Mutex<Option<bool>>>,
}

impl RecoveryPolicy {
    pub(crate) fn create() -> Self {
        Self { is_local_reset_allowed: Rc::new(Mutex::new(None)) }
    }
}

impl Service for RecoveryPolicy {
    fn can_handle_service(&self, service_name: &str) -> bool {
        service_name == fidl_fuchsia_recovery_policy::DeviceMarker::PROTOCOL_NAME
    }

    fn process_stream(&mut self, service_name: &str, channel: zx::Channel) -> Result<(), Error> {
        if !self.can_handle_service(service_name) {
            return Err(format_err!("unsupported"));
        }

        let mut manager_stream =
            ServerEnd::<fidl_fuchsia_recovery_policy::DeviceMarker>::new(channel).into_stream();

        let local_reset_allowed_handle = self.is_local_reset_allowed.clone();

        fasync::Task::local(async move {
            while let Some(req) = manager_stream.try_next().await.unwrap() {
                // Support future expansion of FIDL.
                #[allow(unreachable_patterns)]
                #[allow(clippy::single_match)]
                match req {
                    fidl_fuchsia_recovery_policy::DeviceRequest::SetIsLocalResetAllowed {
                        allowed,
                        control_handle: _,
                    } => {
                        *local_reset_allowed_handle.lock().await = Some(allowed);
                    }
                    _ => {}
                }
            }
        })
        .detach();

        Ok(())
    }
}
