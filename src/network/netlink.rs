//! Netlink Socket Listener for Link and Connection Events
//!
//! Listens to kernel link events (RTMGRP_LINK) using a raw Netlink socket
//! to trigger instant, event-driven updates in the governor.

use std::os::unix::io::AsRawFd;
use std::os::fd::{OwnedFd, FromRawFd};
use tokio::io::unix::AsyncFd;
use anyhow::{Result, Context};
use log::{info, debug};
use nix::libc;

pub struct NetlinkListener {
    async_fd: AsyncFd<OwnedFd>,
}

impl NetlinkListener {
    pub fn new() -> Result<Self> {
        // RTMGRP_LINK = 1 (listen to link changes, carrier changes, up/down)
        // AF_NETLINK = 16, SOCK_RAW = 3, NETLINK_ROUTE = 0
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                libc::NETLINK_ROUTE,
            )
        };

        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .context("Failed to create Netlink socket");
        }

        // Bind to RTMGRP_LINK (multicast group 1)
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        addr.nl_groups = 1; // RTMGRP_LINK

        let bind_ret = unsafe {
            libc::bind(
                fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };

        if bind_ret < 0 {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(fd); }
            return Err(err).context("Failed to bind Netlink socket to RTMGRP_LINK");
        }

        let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let async_fd = AsyncFd::new(owned_fd)?;

        info!("Netlink link event listener initialized successfully");
        Ok(Self { async_fd })
    }

    /// Wait for the next Netlink link event to occur (non-blocking async)
    pub async fn next_event(&self) -> Result<()> {
        let mut guard = self.async_fd.readable().await?;
        
        let mut buf = [0u8; 4096];
        let fd = self.async_fd.as_raw_fd();
        
        let n = unsafe {
            libc::recv(
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
            )
        };

        if n > 0 {
            debug!("Netlink event received (size: {} bytes)", n);
            guard.clear_ready();
            Ok(())
        } else if n == 0 {
            Err(anyhow::anyhow!("Netlink socket closed"))
        } else {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                guard.clear_ready();
                Ok(())
            } else {
                Err(err).context("Failed to receive from Netlink socket")
            }
        }
    }
}
