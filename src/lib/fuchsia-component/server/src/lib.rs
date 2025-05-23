// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tools for providing Fuchsia services.

#![deny(missing_docs)]

use anyhow::Error;
use fidl::endpoints::{
    DiscoverableProtocolMarker, Proxy as _, RequestStream, ServerEnd, ServiceMarker, ServiceRequest,
};
use fuchsia_component_client::connect_channel_to_protocol;
use futures::channel::mpsc;
use futures::future::BoxFuture;
use futures::{FutureExt, Stream, StreamExt};
use log::warn;
use pin_project::pin_project;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use thiserror::Error;
use vfs::directory::entry::DirectoryEntry;
use vfs::directory::helper::DirectlyMutable;
use vfs::directory::immutable::Simple as PseudoDir;
use vfs::execution_scope::ExecutionScope;
use vfs::file::vmo::VmoFile;
use vfs::name::Name;
use vfs::remote::remote_dir;
use vfs::service::endpoint;
use zx::MonotonicDuration;
use {fidl_fuchsia_io as fio, fuchsia_async as fasync};

mod service;
pub use service::{
    FidlService, FidlServiceMember, FidlServiceServerConnector, Service, ServiceObj,
    ServiceObjLocal, ServiceObjTrait,
};
mod until_stalled;
pub use until_stalled::{Item, StallableServiceFs};

/// A filesystem which connects clients to services.
///
/// This type implements the `Stream` trait and will yield the values
/// returned from calling `Service::connect` on the services it hosts.
///
/// This can be used to, for example, yield streams of channels, request
/// streams, futures to run, or any other value that should be processed
/// as the result of a request.
#[must_use]
#[pin_project]
pub struct ServiceFs<ServiceObjTy: ServiceObjTrait> {
    // The execution scope for the backing VFS.
    scope: ExecutionScope,

    // The root directory.
    dir: Arc<PseudoDir>,

    // New connections are sent via an mpsc. The tuple is (index, channel) where index is the index
    // into the `services` member.
    new_connection_sender: mpsc::UnboundedSender<(usize, zx::Channel)>,
    new_connection_receiver: mpsc::UnboundedReceiver<(usize, zx::Channel)>,

    // A collection of objects that are able to handle new connections and convert them into a
    // stream of ServiceObjTy::Output requests.  There will be one for each service in the
    // filesystem (irrespective of its place in the hierarchy).
    services: Vec<ServiceObjTy>,

    // A future that completes when the VFS no longer has any connections.  These connections are
    // distinct from connections that might be to services or remotes within this filesystem.
    shutdown: BoxFuture<'static, ()>,

    // The filesystem does not start servicing any requests until ServiceFs is first polled.  This
    // preserves behaviour of ServiceFs from when it didn't use the Rust VFS, and is relied upon in
    // some cases.  The queue is used until first polled.  After that, `channel_queue` will be None
    // and requests to service channels will be actioned immediately (potentially on different
    // threads depending on the executor).
    channel_queue: Option<Vec<fidl::endpoints::ServerEnd<fio::DirectoryMarker>>>,
}

impl<'a, Output: 'a> ServiceFs<ServiceObjLocal<'a, Output>> {
    /// Create a new `ServiceFs` that is singlethreaded-only and does not
    /// require services to implement `Send`.
    pub fn new_local() -> Self {
        Self::new_impl()
    }
}

impl<'a, Output: 'a> ServiceFs<ServiceObj<'a, Output>> {
    /// Create a new `ServiceFs` that is multithreaded-capable and requires
    /// services to implement `Send`.
    pub fn new() -> Self {
        Self::new_impl()
    }
}

/// A directory within a `ServiceFs`.
///
/// Services and subdirectories can be added to it.
pub struct ServiceFsDir<'a, ServiceObjTy: ServiceObjTrait> {
    fs: &'a mut ServiceFs<ServiceObjTy>,
    dir: Arc<PseudoDir>,
}

/// A `Service` implementation that proxies requests
/// to the outside environment.
///
/// Not intended for direct use. Use the `add_proxy_service`
/// function instead.
#[doc(hidden)]
pub struct Proxy<P, O>(PhantomData<(P, fn() -> O)>);

impl<P: DiscoverableProtocolMarker, O> Service for Proxy<P, O> {
    type Output = O;
    fn connect(&mut self, channel: zx::Channel) -> Option<O> {
        if let Err(e) = connect_channel_to_protocol::<P>(channel) {
            eprintln!("failed to proxy request to {}: {:?}", P::PROTOCOL_NAME, e);
        }
        None
    }
}

/// A `Service` implementation that proxies requests to the given component.
///
/// Not intended for direct use. Use the `add_proxy_service_to` function instead.
#[doc(hidden)]
pub struct ProxyTo<P, O> {
    directory_request: Arc<fidl::endpoints::ClientEnd<fio::DirectoryMarker>>,
    _phantom: PhantomData<(P, fn() -> O)>,
}

impl<P: DiscoverableProtocolMarker, O> Service for ProxyTo<P, O> {
    type Output = O;
    fn connect(&mut self, channel: zx::Channel) -> Option<O> {
        if let Err(e) =
            fdio::service_connect_at(self.directory_request.channel(), P::PROTOCOL_NAME, channel)
        {
            eprintln!("failed to proxy request to {}: {:?}", P::PROTOCOL_NAME, e);
        }
        None
    }
}

// Not part of a trait so that clients won't have to import a trait
// in order to call these functions.
macro_rules! add_functions {
    () => {
        /// Adds a service connector to the directory.
        ///
        /// ```rust
        /// let mut fs = ServiceFs::new_local();
        /// fs
        ///     .add_service_connector(|server_end: ServerEnd<EchoMarker>| {
        ///         connect_channel_to_protocol::<EchoMarker>(
        ///             server_end.into_channel(),
        ///         )
        ///     })
        ///     .add_service_connector(|server_end: ServerEnd<CustomMarker>| {
        ///         connect_channel_to_protocol::<CustomMarker>(
        ///             server_end.into_channel(),
        ///         )
        ///     })
        ///     .take_and_serve_directory_handle()?;
        /// ```
        ///
        /// The FIDL service will be hosted at the name provided by the
        /// `[Discoverable]` annotation in the FIDL source.
        pub fn add_service_connector<F, P>(&mut self, service: F) -> &mut Self
        where
            F: FnMut(ServerEnd<P>) -> ServiceObjTy::Output,
            P: DiscoverableProtocolMarker,
            FidlServiceServerConnector<F, P, ServiceObjTy::Output>: Into<ServiceObjTy>,
        {
            self.add_service_at(P::PROTOCOL_NAME, FidlServiceServerConnector::from(service))
        }

        /// Adds a service to the directory at the given path.
        ///
        /// The path must be a single component containing no `/` characters.
        ///
        /// Panics if any node has already been added at the given path.
        pub fn add_service_at(
            &mut self,
            path: impl Into<String>,
            service: impl Into<ServiceObjTy>,
        ) -> &mut Self {
            let index = self.fs().services.len();
            self.fs().services.push(service.into());
            let sender = self.fs().new_connection_sender.clone();
            self.add_entry_at(
                path,
                endpoint(move |_, channel| {
                    // It's possible for this send to fail in the case where ServiceFs has been
                    // dropped.  When that happens, ServiceFs will drop ExecutionScope which
                    // contains the RemoteHandle for this task which will then cause this task to be
                    // dropped but not necessarily immediately.  This will only occur when ServiceFs
                    // has been dropped, so it's safe to ignore the error here.
                    let _ = sender.unbounded_send((index, channel.into()));
                }),
            )
        }

        /// Adds a FIDL service to the directory.
        ///
        /// `service` is a closure that accepts a `RequestStream`.
        /// Each service being served must return an instance of the same type
        /// (`ServiceObjTy::Output`). This is necessary in order to multiplex
        /// multiple services over the same dispatcher code. The typical way
        /// to do this is to create an `enum` with variants for each service
        /// you want to serve.
        ///
        /// ```rust
        /// enum MyServices {
        ///     EchoServer(EchoRequestStream),
        ///     CustomServer(CustomRequestStream),
        ///     // ...
        /// }
        /// ```
        ///
        /// The constructor for a variant of the `MyServices` enum can be passed
        /// as the `service` parameter.
        ///
        /// ```rust
        /// let mut fs = ServiceFs::new_local();
        /// fs
        ///     .add_fidl_service(MyServices::EchoServer)
        ///     .add_fidl_service(MyServices::CustomServer)
        ///     .take_and_serve_directory_handle()?;
        /// ```
        ///
        /// `ServiceFs` can now be treated as a `Stream` of type `MyServices`.
        ///
        /// ```rust
        /// const MAX_CONCURRENT: usize = 10_000;
        /// fs.for_each_concurrent(MAX_CONCURRENT, |request: MyServices| {
        ///     match request {
        ///         MyServices::EchoServer(request) => handle_echo(request),
        ///         MyServices::CustomServer(request) => handle_custom(request),
        ///     }
        /// }).await;
        /// ```
        ///
        /// The FIDL service will be hosted at the name provided by the
        /// `[Discoverable]` annotation in the FIDL source.
        pub fn add_fidl_service<F, RS>(&mut self, service: F) -> &mut Self
        where
            F: FnMut(RS) -> ServiceObjTy::Output,
            RS: RequestStream,
            RS::Protocol: DiscoverableProtocolMarker,
            FidlService<F, RS, ServiceObjTy::Output>: Into<ServiceObjTy>,
        {
            self.add_fidl_service_at(RS::Protocol::PROTOCOL_NAME, service)
        }

        /// Adds a FIDL service to the directory at the given path.
        ///
        /// The path must be a single component containing no `/` characters.
        ///
        /// See [`add_fidl_service`](#method.add_fidl_service) for details.
        pub fn add_fidl_service_at<F, RS>(
            &mut self,
            path: impl Into<String>,
            service: F,
        ) -> &mut Self
        where
            F: FnMut(RS) -> ServiceObjTy::Output,
            RS: RequestStream,
            RS::Protocol: DiscoverableProtocolMarker,
            FidlService<F, RS, ServiceObjTy::Output>: Into<ServiceObjTy>,
        {
            self.add_service_at(path, FidlService::from(service))
        }

        /// Adds a named instance of a FIDL service to the directory.
        ///
        /// The FIDL service will be hosted at `[SERVICE_NAME]/[instance]/` where `SERVICE_NAME` is
        /// constructed from the FIDL library path and the name of the FIDL service.
        ///
        /// The `instance` must be a single component containing no `/` characters.
        ///
        /// # Example
        ///
        /// For the following FIDL definition,
        /// ```fidl
        /// library lib.foo;
        ///
        /// service Bar {
        ///   ...
        /// }
        /// ```
        ///
        /// The `SERVICE_NAME` of FIDL Service `Bar` would be `lib.foo.Bar`.
        pub fn add_fidl_service_instance<F, SR>(
            &mut self,
            instance: impl Into<String>,
            service: F,
        ) -> &mut Self
        where
            F: Fn(SR) -> ServiceObjTy::Output,
            F: Clone,
            SR: ServiceRequest,
            FidlServiceMember<F, SR, ServiceObjTy::Output>: Into<ServiceObjTy>,
        {
            self.add_fidl_service_instance_at(SR::Service::SERVICE_NAME, instance, service)
        }

        /// Adds a named instance of a FIDL service to the directory at the given path.
        ///
        /// The FIDL service will be hosted at `[path]/[instance]/`.
        ///
        /// The `path` and `instance` must be single components containing no `/` characters.
        pub fn add_fidl_service_instance_at<F, SR>(
            &mut self,
            path: impl Into<String>,
            instance: impl Into<String>,
            service: F,
        ) -> &mut Self
        where
            F: Fn(SR) -> ServiceObjTy::Output,
            F: Clone,
            SR: ServiceRequest,
            FidlServiceMember<F, SR, ServiceObjTy::Output>: Into<ServiceObjTy>,
        {
            // Create the service directory, with an instance subdirectory.
            let mut dir = self.dir(path);
            let mut dir = dir.dir(instance);

            // Attach member protocols under the instance directory.
            for member in SR::member_names() {
                dir.add_service_at(*member, FidlServiceMember::new(service.clone(), member));
            }
            self
        }

        /// Adds a service that proxies requests to the current environment.
        // NOTE: we'd like to be able to remove the type parameter `O` here,
        //  but unfortunately the bound `ServiceObjTy: From<Proxy<P, ServiceObjTy::Output>>`
        //  makes type checking angry.
        pub fn add_proxy_service<P: DiscoverableProtocolMarker, O>(&mut self) -> &mut Self
        where
            ServiceObjTy: From<Proxy<P, O>>,
            ServiceObjTy: ServiceObjTrait<Output = O>,
        {
            self.add_service_at(P::PROTOCOL_NAME, Proxy::<P, ServiceObjTy::Output>(PhantomData))
        }

        /// Adds a service that proxies requests to the given component.
        // NOTE: we'd like to be able to remove the type parameter `O` here,
        //  but unfortunately the bound `ServiceObjTy: From<Proxy<P, ServiceObjTy::Output>>`
        //  makes type checking angry.
        pub fn add_proxy_service_to<P: DiscoverableProtocolMarker, O>(
            &mut self,
            directory_request: Arc<fidl::endpoints::ClientEnd<fio::DirectoryMarker>>,
        ) -> &mut Self
        where
            ServiceObjTy: From<ProxyTo<P, O>>,
            ServiceObjTy: ServiceObjTrait<Output = O>,
        {
            self.add_service_at(
                P::PROTOCOL_NAME,
                ProxyTo::<P, ServiceObjTy::Output> { directory_request, _phantom: PhantomData },
            )
        }

        /// Adds a VMO file to the directory at the given path.
        ///
        /// The path must be a single component containing no `/` characters. The vmo should have
        /// content size set as required.
        ///
        /// Panics if any node has already been added at the given path.
        pub fn add_vmo_file_at(&mut self, path: impl Into<String>, vmo: zx::Vmo) -> &mut Self {
            self.add_entry_at(path, VmoFile::new(vmo))
        }

        /// Adds an entry to the directory at the given path.
        ///
        /// The path must be a single component.
        /// The path must be a valid `fuchsia.io` [`Name`].
        ///
        /// Panics if any node has already been added at the given path.
        pub fn add_entry_at(
            &mut self,
            path: impl Into<String>,
            entry: Arc<dyn DirectoryEntry>,
        ) -> &mut Self {
            let path: String = path.into();
            let name: Name = path.try_into().expect("Invalid path");
            // This will fail if the name is invalid or already exists.
            self.dir.add_entry_impl(name, entry, false).expect("Unable to add entry");
            self
        }

        /// Returns a reference to the subdirectory at the given path,
        /// creating one if none exists.
        ///
        /// The path must be a single component.
        /// The path must be a valid `fuchsia.io` [`Name`].
        ///
        /// Panics if a service has already been added at the given path.
        pub fn dir(&mut self, path: impl Into<String>) -> ServiceFsDir<'_, ServiceObjTy> {
            let path: String = path.into();
            let name: Name = path.try_into().expect("Invalid path");
            let dir = Arc::downcast(self.dir.get_or_insert(name, new_simple_dir).into_any())
                .unwrap_or_else(|_| panic!("Not a directory"));
            ServiceFsDir { fs: self.fs(), dir }
        }

        /// Adds a new remote directory served over the given DirectoryProxy.
        ///
        /// The name must be a valid `fuchsia.io` [`Name`].
        pub fn add_remote(
            &mut self,
            name: impl Into<String>,
            proxy: fio::DirectoryProxy,
        ) -> &mut Self {
            let name: String = name.into();
            let name: Name = name.try_into().expect("Invalid path");
            self.dir.add_entry_impl(name, remote_dir(proxy), false).expect("Unable to add entry");
            self
        }
    };
}

impl<ServiceObjTy: ServiceObjTrait> ServiceFsDir<'_, ServiceObjTy> {
    fn fs(&mut self) -> &mut ServiceFs<ServiceObjTy> {
        self.fs
    }

    add_functions!();
}

impl<ServiceObjTy: ServiceObjTrait> ServiceFs<ServiceObjTy> {
    fn new_impl() -> Self {
        let (new_connection_sender, new_connection_receiver) = mpsc::unbounded();
        let scope = ExecutionScope::new();
        let dir = new_simple_dir();
        Self {
            scope: scope.clone(),
            dir,
            new_connection_sender,
            new_connection_receiver,
            services: Vec::new(),
            shutdown: async move { scope.wait().await }.boxed(),
            channel_queue: Some(Vec::new()),
        }
    }

    fn fs(&mut self) -> &mut ServiceFs<ServiceObjTy> {
        self
    }

    /// Get a reference to the root directory as a `ServiceFsDir`.
    ///
    /// This can be useful when writing code which hosts some set of services on
    /// a directory and wants to be agnostic to whether that directory
    /// is the root `ServiceFs` or a subdirectory.
    ///
    /// Such a function can take an `&mut ServiceFsDir<...>` as an argument,
    /// allowing callers to provide either a subdirectory or `fs.root_dir()`.
    pub fn root_dir(&mut self) -> ServiceFsDir<'_, ServiceObjTy> {
        let dir = self.dir.clone();
        ServiceFsDir { fs: self, dir }
    }

    add_functions!();

    /// When a connection is first made to the `ServiceFs` in the absence of a parent connection,
    /// it will be granted these rights.
    const fn base_connection_flags() -> fio::Flags {
        return fio::Flags::PROTOCOL_DIRECTORY
            .union(fio::PERM_READABLE)
            .union(fio::PERM_WRITABLE)
            .union(fio::PERM_EXECUTABLE);
    }

    fn serve_connection_impl(&self, chan: fidl::endpoints::ServerEnd<fio::DirectoryMarker>) {
        vfs::directory::serve_on(
            self.dir.clone(),
            Self::base_connection_flags(),
            self.scope.clone(),
            chan,
        );
    }

    /// Creates a protocol connector that can access the capabilities exposed by this ServiceFs.
    pub fn create_protocol_connector<O>(&mut self) -> Result<ProtocolConnector, Error>
    where
        ServiceObjTy: ServiceObjTrait<Output = O>,
    {
        let (directory_request, directory_server_end) = fidl::endpoints::create_endpoints();
        self.serve_connection(directory_server_end)?;

        Ok(ProtocolConnector { directory_request })
    }
}

fn new_simple_dir() -> Arc<PseudoDir> {
    let dir = PseudoDir::new();
    dir.clone().set_not_found_handler(Box::new(move |path| {
        warn!(
            "ServiceFs received request to `{}` but has not been configured to serve this path.",
            path
        );
    }));
    dir
}

/// `ProtocolConnector` allows connecting to capabilities exposed by ServiceFs
pub struct ProtocolConnector {
    directory_request: fidl::endpoints::ClientEnd<fio::DirectoryMarker>,
}

impl ProtocolConnector {
    /// Connect to a protocol provided by this environment.
    #[inline]
    pub fn connect_to_service<P: DiscoverableProtocolMarker>(&self) -> Result<P::Proxy, Error> {
        self.connect_to_protocol::<P>()
    }

    /// Connect to a protocol provided by this environment.
    #[inline]
    pub fn connect_to_protocol<P: DiscoverableProtocolMarker>(&self) -> Result<P::Proxy, Error> {
        let (client_channel, server_channel) = zx::Channel::create();
        self.pass_to_protocol::<P>(server_channel)?;
        Ok(P::Proxy::from_channel(fasync::Channel::from_channel(client_channel)))
    }

    /// Connect to a protocol by passing a channel for the server.
    #[inline]
    pub fn pass_to_protocol<P: DiscoverableProtocolMarker>(
        &self,
        server_channel: zx::Channel,
    ) -> Result<(), Error> {
        self.pass_to_named_protocol(P::PROTOCOL_NAME, server_channel)
    }

    /// Connect to a protocol by name.
    #[inline]
    pub fn pass_to_named_protocol(
        &self,
        protocol_name: &str,
        server_channel: zx::Channel,
    ) -> Result<(), Error> {
        fdio::service_connect_at(self.directory_request.channel(), protocol_name, server_channel)?;
        Ok(())
    }
}

/// An error indicating the startup handle on which the FIDL server
/// attempted to start was missing.
#[derive(Debug, Error)]
#[error("The startup handle on which the FIDL server attempted to start was missing.")]
pub struct MissingStartupHandle;

impl<ServiceObjTy: ServiceObjTrait> ServiceFs<ServiceObjTy> {
    /// Removes the `DirectoryRequest` startup handle for the current
    /// component and adds connects it to this `ServiceFs` as a client.
    ///
    /// Multiple calls to this function from the same component will
    /// result in `Err(MissingStartupHandle)`.
    pub fn take_and_serve_directory_handle(&mut self) -> Result<&mut Self, Error> {
        let startup_handle = fuchsia_runtime::take_startup_handle(
            fuchsia_runtime::HandleType::DirectoryRequest.into(),
        )
        .ok_or(MissingStartupHandle)?;

        self.serve_connection(fidl::endpoints::ServerEnd::new(zx::Channel::from(startup_handle)))
    }

    /// Add a channel to serve this `ServiceFs` filesystem on. The `ServiceFs`
    /// will continue to be provided over previously added channels, including
    /// the one added if `take_and_serve_directory_handle` was called.
    pub fn serve_connection(
        &mut self,
        chan: fidl::endpoints::ServerEnd<fio::DirectoryMarker>,
    ) -> Result<&mut Self, Error> {
        if let Some(channels) = &mut self.channel_queue {
            channels.push(chan);
        } else {
            self.serve_connection_impl(chan);
        }
        Ok(self)
    }

    /// TODO(https://fxbug.dev/326626515): this is an experimental method to run a FIDL
    /// directory connection until stalled, with the purpose to cleanly stop a component.
    /// We'll expect to revisit how this works to generalize to all connections later.
    /// Try not to use this function for other purposes.
    ///
    /// Normally the [`ServiceFs`] stream will block until all connections are closed.
    /// In order to escrow the outgoing directory server endpoint, you may use this
    /// function to get a [`StallableServiceFs`] that detects when no new requests
    /// hit the outgoing directory for `debounce_interval`, and all hosted protocols
    /// and other VFS connections to finish, then yield back the outgoing directory handle.
    ///
    /// The [`ServiceFs`] stream yields [`ServiceObjTy::Output`], which could be an enum
    /// of FIDL connection requests in a typical component. By contrast, [`StallableServiceFs`]
    /// yields an enum of either the request, or the unbound outgoing directory endpoint,
    /// allowing you to escrow it back to `component_manager` before exiting the component.
    pub fn until_stalled(
        self,
        debounce_interval: MonotonicDuration,
    ) -> StallableServiceFs<ServiceObjTy> {
        StallableServiceFs::<ServiceObjTy>::new(self, debounce_interval)
    }
}

impl<ServiceObjTy: ServiceObjTrait> Stream for ServiceFs<ServiceObjTy> {
    type Item = ServiceObjTy::Output;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // NOTE: Normally, it isn't safe to poll a stream after it returns None, but we support this
        // and StallabkeServiceFs depends on this.
        if let Some(channels) = self.channel_queue.take() {
            for chan in channels {
                self.serve_connection_impl(chan);
            }
        }
        while let Poll::Ready(Some((index, channel))) =
            self.new_connection_receiver.poll_next_unpin(cx)
        {
            if let Some(stream) = self.services[index].service().connect(channel) {
                return Poll::Ready(Some(stream));
            }
        }
        self.shutdown.poll_unpin(cx).map(|_| None)
    }
}
