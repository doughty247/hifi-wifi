//! Netlink Socket Listener for Link and Connection Events
//!
//! Listens to kernel link events (RTMGRP_LINK) using a raw Netlink socket
//! to trigger instant, event-driven updates in the governor.

use anyhow::{Context, Result};
use log::{debug, info};
use nix::libc;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::io::AsRawFd;
use tokio::io::unix::AsyncFd;

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
            return Err(std::io::Error::last_os_error()).context("Failed to create Netlink socket");
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
            unsafe {
                libc::close(fd);
            }
            return Err(err).context("Failed to bind Netlink socket to RTMGRP_LINK");
        }

        let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let async_fd = AsyncFd::new(owned_fd)?;

        info!("Netlink link event listener initialized successfully");
        Ok(Self { async_fd })
    }

    /// Wait for the next batch of link events (one recv can carry several messages)
    pub async fn next_events(&self) -> Result<Vec<LinkEvent>> {
        loop {
            let mut guard = self.async_fd.readable().await?;
            let mut buf = [0u8; 8192];
            let fd = self.async_fd.as_raw_fd();
            let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };

            if n > 0 {
                debug!("Netlink message received (size: {} bytes)", n);
                return Ok(parse_link_events(&buf[..n as usize]));
            } else if n == 0 {
                return Err(anyhow::anyhow!("Netlink socket closed"));
            }
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                guard.clear_ready();
                continue;
            }
            return Err(err).context("Failed to receive from Netlink socket");
        }
    }
}

/// One RTM_NEWLINK / RTM_DELLINK notification
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkEvent {
    pub ifindex: u32,
    /// Interface is gone (RTM_DELLINK)
    pub removed: bool,
    /// Carrier is up (IFF_LOWER_UP)
    pub carrier: bool,
}

const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const IFF_LOWER_UP: u32 = 0x10000;

/// Parse a netlink datagram: nlmsghdr (16 bytes) followed by ifinfomsg (16 bytes), repeated,
/// each message padded to 4 bytes.
pub fn parse_link_events(buf: &[u8]) -> Vec<LinkEvent> {
    let u16_at = |o: usize| u16::from_ne_bytes([buf[o], buf[o + 1]]);
    let u32_at = |o: usize| u32::from_ne_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
    let mut events = Vec::new();
    let mut off = 0;
    while off + 16 <= buf.len() {
        let len = u32_at(off) as usize;
        if len < 16 || off + len > buf.len() {
            break;
        }
        let ty = u16_at(off + 4);
        // ifinfomsg: family u8, pad u8, type u16, index i32, flags u32, change u32
        if (ty == RTM_NEWLINK || ty == RTM_DELLINK) && len >= 32 {
            events.push(LinkEvent {
                ifindex: u32_at(off + 20),
                removed: ty == RTM_DELLINK,
                carrier: u32_at(off + 24) & IFF_LOWER_UP != 0,
            });
        }
        off += (len + 3) & !3;
    }
    events
}

/// Interface name for an index, from /sys/class/net/*/ifindex
pub fn ifindex_name(ifindex: u32) -> Option<String> {
    std::fs::read_dir("/sys/class/net")
        .ok()?
        .flatten()
        .find_map(|e| {
            let idx = std::fs::read_to_string(e.path().join("ifindex")).ok()?;
            (idx.trim().parse::<u32>().ok()? == ifindex)
                .then(|| e.file_name().to_string_lossy().into_owned())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(ty: u16, ifindex: u32, flags: u32) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&32u32.to_ne_bytes()); // nlmsg_len
        m.extend_from_slice(&ty.to_ne_bytes());
        m.extend_from_slice(&[0u8; 10]); // flags, seq, pid
        m.extend_from_slice(&[0u8, 0u8]); // family, pad
        m.extend_from_slice(&1u16.to_ne_bytes()); // ARPHRD_ETHER
        m.extend_from_slice(&ifindex.to_ne_bytes());
        m.extend_from_slice(&flags.to_ne_bytes());
        m.extend_from_slice(&0u32.to_ne_bytes()); // change
        m
    }

    #[test]
    fn parses_batched_link_messages() {
        let mut buf = msg(RTM_NEWLINK, 3, 0x1 | IFF_LOWER_UP);
        buf.extend(msg(RTM_DELLINK, 7, 0));
        buf.extend(msg(24, 9, 0)); // RTM_NEWROUTE: ignored
        let ev = parse_link_events(&buf);
        assert_eq!(
            ev,
            vec![
                LinkEvent {
                    ifindex: 3,
                    removed: false,
                    carrier: true
                },
                LinkEvent {
                    ifindex: 7,
                    removed: true,
                    carrier: false
                },
            ]
        );
    }

    /// Needs root and iproute2: `cargo test -- --ignored netlink_real_kernel`
    #[tokio::test]
    #[ignore]
    async fn netlink_real_kernel() {
        let nl = NetlinkListener::new().unwrap();
        let ip = |args: &[&str]| {
            std::process::Command::new("ip")
                .args(args)
                .status()
                .unwrap()
        };
        ip(&[
            "link", "add", "hwtest0", "type", "veth", "peer", "name", "hwtest1",
        ]);
        ip(&["link", "set", "hwtest0", "up"]);
        ip(&["link", "set", "hwtest1", "up"]);
        let idx: u32 = std::fs::read_to_string("/sys/class/net/hwtest0/ifindex")
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(ifindex_name(idx).as_deref(), Some("hwtest0"));
        let mut saw_up = false;
        for _ in 0..20 {
            let evs =
                tokio::time::timeout(std::time::Duration::from_secs(1), nl.next_events()).await;
            let Ok(Ok(evs)) = evs else { break };
            if evs
                .iter()
                .any(|e| e.ifindex == idx && e.carrier && !e.removed)
            {
                saw_up = true;
                break;
            }
        }
        ip(&["link", "del", "hwtest0"]);
        assert!(saw_up, "no carrier-up event for hwtest0");
    }

    #[test]
    fn truncated_input_is_ignored() {
        let buf = msg(RTM_NEWLINK, 3, IFF_LOWER_UP);
        assert!(parse_link_events(&buf[..20]).is_empty());
        assert!(parse_link_events(&[]).is_empty());
    }
}
