// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{format_err, Context as _, Error};
use fidl::endpoints;
use fidl_fuchsia_vsock::{
    AcceptorMarker, AcceptorRequest, ConnectionMarker, ConnectionProxy, ConnectionTransport,
    ConnectorMarker,
};
use fuchsia_async as fasync;
use fuchsia_component::client::connect_to_protocol;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use futures::StreamExt;
use zx::{self as zx, AsHandleRef};

const TEST_DATA_LEN: u64 = 60000;

fn make_socket_pair() -> Result<(fasync::Socket, zx::Socket), Error> {
    let (a, b) = zx::Socket::create_stream();
    let info = a.info()?;
    a.set_tx_threshold(&(info.tx_buf_max as usize))?;
    let a_stream = fasync::Socket::from_socket(a);
    Ok((a_stream, b))
}

fn wait_socket_empty(socket: &fasync::Socket) {
    socket
        .as_handle_ref()
        .wait(zx::Signals::SOCKET_WRITE_THRESHOLD, zx::MonotonicInstant::INFINITE)
        .unwrap();
}

async fn test_read_write<'a>(
    socket: &'a mut fasync::Socket,
    _con: &'a ConnectionProxy,
) -> Result<(), Error> {
    let data = Box::new([42u8; TEST_DATA_LEN as usize]);

    socket.write_all(&*data).await?;
    wait_socket_empty(&socket);

    // Expect a single value back
    let mut val = [0];
    socket.read_exact(&mut val).await?;
    if val[0] != 42 {
        return Err(format_err!("Expected to read '42' not '{}'", val[0]));
    }
    Ok(())
}

fn make_con() -> Result<(fasync::Socket, ConnectionProxy, ConnectionTransport), anyhow::Error> {
    let (data_stream, server_socket) = make_socket_pair()?;
    let (client_end, server_end) = endpoints::create_endpoints::<ConnectionMarker>();
    let client_end = client_end.into_proxy();
    let con = ConnectionTransport { data: server_socket, con: server_end };
    Ok((data_stream, client_end, con))
}

#[fasync::run_singlethreaded]
async fn main() -> Result<(), Error> {
    let vsock =
        connect_to_protocol::<ConnectorMarker>().context("failed to connect to vsock service")?;
    // Register the listeners early to avoid any race conditions later.
    let (acceptor_client, acceptor) = endpoints::create_endpoints::<AcceptorMarker>();
    vsock.listen(8001, acceptor_client).await?.map_err(zx::Status::from_raw)?;
    let (acceptor_client2, acceptor2) = endpoints::create_endpoints::<AcceptorMarker>();
    vsock.listen(8002, acceptor_client2).await?.map_err(zx::Status::from_raw)?;
    let (acceptor_client3, acceptor3) = endpoints::create_endpoints::<AcceptorMarker>();
    vsock.listen(8003, acceptor_client3).await?.map_err(zx::Status::from_raw)?;
    let mut acceptor = acceptor.into_stream();
    let mut acceptor2 = acceptor2.into_stream();
    let mut acceptor3 = acceptor3.into_stream();

    let (mut data_stream, client_end, con) = make_con()?;
    let _port = vsock.connect(2, 8000, con).await?.map_err(zx::Status::from_raw)?;
    test_read_write(&mut data_stream, &client_end).await?;

    client_end.shutdown()?;
    data_stream
        .as_handle_ref()
        .wait(zx::Signals::SOCKET_PEER_CLOSED, zx::MonotonicInstant::INFINITE)
        .to_result()?;

    // Wait for a connection
    let AcceptorRequest::Accept { addr: _, responder } =
        acceptor.next().await.ok_or_else(|| format_err!("Failed to get incoming connection"))??;
    let (mut data_stream, client_end, con) = make_con()?;
    responder.send(Some(con))?;

    // Send data then wait for other end to shut us down.
    test_read_write(&mut data_stream, &client_end).await?;
    data_stream
        .as_handle_ref()
        .wait(zx::Signals::SOCKET_PEER_CLOSED, zx::MonotonicInstant::INFINITE)
        .to_result()?;

    // Get next connection
    let AcceptorRequest::Accept { addr: _, responder } = acceptor2
        .next()
        .await
        .ok_or_else(|| format_err!("Failed to get incoming connection"))??;
    let (mut data_stream, _client_end, con) = make_con()?;
    responder.send(Some(con))?;
    // Send data until the peer closes
    let data = Box::new([42u8; TEST_DATA_LEN as usize]);

    loop {
        let result = data_stream.write_all(&*data).await;
        if let Err(e) = result {
            if e.kind() == std::io::ErrorKind::ConnectionAborted {
                break;
            }
        }
    }

    // Get next connection
    {
        let AcceptorRequest::Accept { addr: _, responder } = acceptor3
            .next()
            .await
            .ok_or_else(|| format_err!("Failed to get incoming connection"))??;
        let (mut data_stream, _client_end, con) = make_con()?;
        responder.send(Some(con))?;
        // Read some data then suddenly close the connection.
        let mut val = [0];
        data_stream.read_exact(&mut val).await?;
        if val[0] != 0 {
            return Err(format_err!("Expected to read '0' no '{}'", val[0]));
        }
    }

    println!("PASS");
    Ok(())
}
