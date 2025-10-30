// Copyright (c) Microsoft Corporation. All rights reserved.

use crate::operations::{self, AddressFamily, SocketType, ipproto, listen, socket};

use crate::{Errno, OwnedFd};
use rustix::net::sockopt::{
    set_socket_keepalive, set_tcp_keepcnt, set_tcp_keepidle, set_tcp_keepintvl, set_tcp_nodelay,
};
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::time::Duration;

pub async fn create_server_socket(port: u16) -> Result<OwnedFd, Errno> {
    let server_fd = socket(AddressFamily::INET6, SocketType::STREAM, Some(ipproto::TCP)).await?;

    rustix::net::sockopt::set_tcp_nodelay(&server_fd, true)?;
    rustix::net::sockopt::set_socket_reuseaddr(&server_fd, true)?;

    // Bind to INADDR_ANY => UNSPECIFIED to listen for connections on any interface on the host
    let sock_addr = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0);
    rustix::net::bind(&server_fd, &sock_addr)?;
    listen(&server_fd, libc::SOMAXCONN)?;

    Ok(server_fd)
}

pub async fn create_client_socket(sock_addr: &SocketAddr) -> Result<OwnedFd, Errno> {
    let domain = if sock_addr.is_ipv4() {
        AddressFamily::INET
    } else {
        AddressFamily::INET6
    };
    let socket = operations::socket(domain, SocketType::STREAM, Some(ipproto::TCP)).await?;
    operations::connect(&socket, sock_addr).await?;

    // Disable the Nagle algorithm since that craters performance due to
    // current write-write-read patterns
    set_tcp_nodelay(&socket, true)?;

    // Enable TCP keep alive to ensure that we detect dead connections in ~60 seconds
    // (idle_time + keep_alive_interval*keep_alive_probes) after its idle.
    enable_tcp_keep_alive(&socket, Duration::from_secs(30), Duration::from_secs(1), 30)?;

    Ok(socket)
}

/// Enable TCP keep alive on the socket.
pub fn enable_tcp_keep_alive(
    socket: &OwnedFd,
    initial_idle_time_before_probe: Duration,
    keep_alive_interval: Duration,
    keep_alive_probes: u32,
) -> Result<(), Errno> {
    // Initial idle time before sending the first probe.
    set_tcp_keepidle(socket, initial_idle_time_before_probe)?;

    // Keep alive message interval.
    set_tcp_keepintvl(socket, keep_alive_interval)?;

    // Number of probes.
    set_tcp_keepcnt(socket, keep_alive_probes)?;

    set_socket_keepalive(socket, true)?;

    Ok(())
}

/// Update client socket properties on server side.
pub fn update_accept_socket(socket: &OwnedFd) -> Result<(), Errno> {
    // Disable the Nagle algorithm since that craters performance due to
    // current write-write-read patterns
    set_tcp_nodelay(socket, true)?;

    // Enable TCP keep alive to ensure that we detect dead connections in ~60 seconds
    // (idle_time + keep_alive_interval*keep_alive_probes) after its idle.
    enable_tcp_keep_alive(socket, Duration::from_secs(30), Duration::from_secs(1), 30)?;

    Ok(())
}
