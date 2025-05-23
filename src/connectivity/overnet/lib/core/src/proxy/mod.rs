// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod handle;
mod run;
mod stream;

use self::handle::{ProxyableHandle, ReadValue};
use self::stream::StreamWriter;
use crate::labels::{NodeId, TransferKey};
use crate::peer::FramedStreamWriter;
use crate::router::Router;
use anyhow::{format_err, Error};
use fidl_fuchsia_overnet_protocol::{
    SignalUpdate, StreamId, StreamRef, TransferInitiator, TransferWaiter,
};
use futures::prelude::*;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use zx_status;

pub(crate) use self::handle::{IntoProxied, Proxyable, ProxyableRW};
pub(crate) use self::run::spawn::{recv as spawn_recv, send as spawn_send};

pub use self::run::set_proxy_drop_event_handler;

#[derive(Debug)]
pub(crate) enum RemoveFromProxyTable {
    InitiateTransfer {
        paired_handle: fidl::Handle,
        drain_stream: FramedStreamWriter,
        stream_ref_sender: StreamRefSender,
    },
    Dropped,
}

impl RemoveFromProxyTable {
    pub(crate) fn is_dropped(&self) -> bool {
        match self {
            RemoveFromProxyTable::Dropped => true,
            _ => false,
        }
    }
}

pub(crate) struct ProxyTransferInitiationReceiver(
    Pin<Box<dyn Send + Future<Output = Result<RemoveFromProxyTable, Error>>>>,
);

impl Future for ProxyTransferInitiationReceiver {
    type Output = Result<RemoveFromProxyTable, Error>;
    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.as_mut().poll(ctx)
    }
}

impl std::fmt::Debug for ProxyTransferInitiationReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        "_".fmt(f)
    }
}

impl ProxyTransferInitiationReceiver {
    pub(crate) fn new(
        f: impl 'static + Send + Future<Output = Result<RemoveFromProxyTable, Error>>,
    ) -> Self {
        Self(Box::pin(f))
    }
}

pub(crate) struct StreamRefSender {
    chan: futures::channel::oneshot::Sender<StreamRef>,
}

impl std::fmt::Debug for StreamRefSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        "StreamRefSender".fmt(f)
    }
}

impl StreamRefSender {
    pub(crate) fn new() -> (Self, futures::channel::oneshot::Receiver<StreamRef>) {
        let (tx, rx) = futures::channel::oneshot::channel();
        (Self { chan: tx }, rx)
    }

    fn send(self, stream_ref: StreamRef) -> Result<(), Error> {
        Ok(self.chan.send(stream_ref).map_err(|_| format_err!("Failed sending StreamRef"))?)
    }

    pub(crate) fn draining_initiate(
        self,
        stream_id: u64,
        new_destination_node: NodeId,
        transfer_key: TransferKey,
    ) -> Result<(), Error> {
        Ok(self.send(StreamRef::TransferInitiator(TransferInitiator {
            stream_id: StreamId { id: stream_id },
            new_destination_node: new_destination_node.into(),
            transfer_key,
        }))?)
    }

    pub(crate) fn draining_await(
        self,
        stream_id: u64,
        transfer_key: TransferKey,
    ) -> Result<(), Error> {
        Ok(self.send(StreamRef::TransferWaiter(TransferWaiter {
            stream_id: StreamId { id: stream_id },
            transfer_key,
        }))?)
    }
}

mod proxy_count {
    use std::sync::Mutex;

    pub struct ProxyCount {
        count: usize,
        high_water: usize,
        increment: usize,
    }

    pub static PROXY_COUNT: Mutex<ProxyCount> =
        Mutex::new(ProxyCount { count: 0, high_water: 0, increment: 100 });

    impl ProxyCount {
        pub fn increment(&mut self) {
            self.count += 1;

            if self.count == self.high_water + self.increment {
                self.high_water += self.increment;
                if self.count == self.increment * 10 {
                    self.increment *= 10;
                }
                log::info!("{} proxies extant or never reaped", self.count)
            }
        }

        pub fn decrement(&mut self) {
            if self.count == 0 {
                log::warn!("proxy counter went below zero");
            } else {
                self.count -= 1;
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct Proxy<Hdl: Proxyable + 'static> {
    hdl: Option<ProxyableHandle<Hdl>>,
}

impl<Hdl: 'static + Proxyable> Drop for Proxy<Hdl> {
    fn drop(&mut self) {
        proxy_count::PROXY_COUNT.lock().unwrap().decrement();
    }
}

impl<Hdl: 'static + Proxyable> Proxy<Hdl> {
    fn new(hdl: Hdl, router: Weak<Router>) -> Arc<Self> {
        proxy_count::PROXY_COUNT.lock().unwrap().increment();
        Arc::new(Self { hdl: Some(ProxyableHandle::new(hdl, router)) })
    }

    fn close_with_reason(mut self, msg: String) {
        if let Some(hdl) = self.hdl.take() {
            hdl.close_with_reason(msg);
        }
    }

    fn hdl(&self) -> &ProxyableHandle<Hdl> {
        self.hdl.as_ref().unwrap()
    }

    async fn write_to_handle(&self, msg: &mut Hdl::Message) -> Result<(), zx_status::Status>
    where
        Hdl: for<'a> ProxyableRW<'a>,
    {
        self.hdl().write(msg).await
    }

    fn apply_signal_update(&self, signal_update: SignalUpdate) -> Result<(), Error> {
        self.hdl().apply_signal_update(signal_update)
    }

    fn read_from_handle<'a>(
        &'a self,
        msg: &'a mut Hdl::Message,
    ) -> impl 'a + Future<Output = Result<ReadValue, zx_status::Status>> + Unpin
    where
        Hdl: ProxyableRW<'a>,
    {
        self.hdl().read(msg)
    }

    async fn drain_handle_to_stream(
        &self,
        stream_writer: &mut StreamWriter<Hdl::Message>,
    ) -> Result<(), Error>
    where
        Hdl: for<'a> ProxyableRW<'a>,
    {
        self.hdl().drain_to_stream(stream_writer).await
    }
}
