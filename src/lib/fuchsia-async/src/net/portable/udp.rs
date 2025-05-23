// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::io::Result;
use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use tokio::net::UdpSocket as AsyncUdpSocket;

#[derive(Debug)]
pub struct UdpSocket(AsyncUdpSocket);

impl UdpSocket {
    pub fn bind(addr: &SocketAddr) -> Result<UdpSocket> {
        UdpSocket::from_socket(StdUdpSocket::bind(addr)?)
    }

    pub fn from_socket(socket: StdUdpSocket) -> Result<UdpSocket> {
        socket.set_nonblocking(true)?;
        Ok(UdpSocket(AsyncUdpSocket::from_std(socket)?))
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.0.local_addr()
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        self.0.recv_from(buf).await
    }

    pub async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> Result<usize> {
        self.0.send_to(buf, addr).await
    }

    pub fn set_broadcast(&self, broadcast: bool) -> Result<()> {
        self.0.set_broadcast(broadcast)
    }

    pub fn broadcast(&self) -> Result<bool> {
        self.0.broadcast()
    }
}
