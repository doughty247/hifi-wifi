//! Bufferbloat shaper
//!
//! Owns every qdisc hifi-wifi installs:
//! - Upload: shaper on the interface root (egress).
//! - Download: ingress traffic redirected to a per-interface IFB device and shaped there.
//!
//! CAKE is preferred. Kernels without `sch_cake` get HTB + fq_codel, the classic SQM
//! "simple" setup. Rates are in kbit/s so autorate can make fine adjustments at low speeds.

use anyhow::{bail, Context, Result};
use log::{debug, info, warn};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Cake,
    HtbFqCodel,
    /// Last resort for minimal kernels without fq_codel: token bucket with a 20 ms queue bound.
    /// Not flow-fair, but latency stays bounded (TBF splits GSO packets itself).
    Tbf,
}

/// IFB device used for ingress shaping of `iface` (max 15 chars, like sqm-scripts' ifb4<iface>).
pub fn ifb_name(iface: &str) -> String {
    let mut name = format!("ifb4{}", iface);
    name.truncate(15);
    name
}

pub fn is_valid_iface(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// tc argument lists for the root shaper. Pure so they can be unit tested.
/// TBF queue bound must stay well under autorate's delay threshold, or autorate would read
/// its own queue as bloat. Bucket = 1 ms of traffic (hrtimer-driven), at least two MTUs.
fn tbf_params(kbit: u32) -> Vec<String> {
    let kbit = kbit.max(1);
    let burst = (kbit as u64 * 1000 / 8 / 1000).max(3_028);
    ["rate", &format!("{}kbit", kbit), "burst", &burst.to_string(), "latency", "5ms"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn root_args(kind: Kind, dev: &str, kbit: u32, ingress: bool) -> Vec<Vec<String>> {
    let rate = format!("{}kbit", kbit.max(1));
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    match kind {
        Kind::Cake => {
            // Upload: isolate per source host, keep DSCP marks so the AP can honor them.
            // Download: isolate per destination host, wash ISP-set DSCP, count drops (ingress).
            let mut a = v(&["qdisc", "replace", "dev", dev, "root", "cake", "bandwidth", &rate, "diffserv4"]);
            if ingress {
                a.extend(v(&["dual-dsthost", "wash", "ingress"]));
            } else {
                a.extend(v(&["dual-srchost", "ack-filter"]));
            }
            vec![a]
        }
        Kind::HtbFqCodel => vec![
            v(&["qdisc", "replace", "dev", dev, "root", "handle", "1:", "htb", "default", "10"]),
            v(&["class", "replace", "dev", dev, "parent", "1:", "classid", "1:10", "htb", "rate", &rate, "ceil", &rate]),
            v(&["qdisc", "replace", "dev", dev, "parent", "1:10", "handle", "10:", "fq_codel"]),
        ],
        Kind::Tbf => {
            let mut a = v(&["qdisc", "replace", "dev", dev, "root", "handle", "1:", "tbf"]);
            a.extend(tbf_params(kbit));
            vec![a]
        }
    }
}

fn change_args(kind: Kind, dev: &str, kbit: u32) -> Vec<String> {
    let rate = format!("{}kbit", kbit.max(1));
    let a: Vec<&str> = match kind {
        Kind::Cake => vec!["qdisc", "change", "dev", dev, "root", "cake", "bandwidth", &rate],
        Kind::HtbFqCodel => vec![
            "class", "change", "dev", dev, "parent", "1:", "classid", "1:10", "htb", "rate", &rate, "ceil", &rate,
        ],
        Kind::Tbf => {
            let mut a: Vec<String> = ["qdisc", "change", "dev", dev, "root", "handle", "1:", "tbf"]
                .iter()
                .map(|s| s.to_string())
                .collect();
            a.extend(tbf_params(kbit));
            return a;
        }
    };
    a.into_iter().map(String::from).collect()
}

fn run(cmd: &str, args: &[String]) -> Result<()> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {}", cmd))?;
    if !out.status.success() {
        bail!(
            "{} {} failed: {}",
            cmd,
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn tc(args: &[&str]) -> Result<()> {
    run("tc", &args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

fn install_root(dev: &str, kbit: u32, ingress: bool) -> Result<Kind> {
    let mut last_err = None;
    for kind in [Kind::Cake, Kind::HtbFqCodel, Kind::Tbf] {
        match root_args(kind, dev, kbit, ingress)
            .iter()
            .try_for_each(|args| run("tc", args))
        {
            Ok(()) => return Ok(kind),
            Err(e) => {
                debug!("{:?} unavailable on {}: {}", kind, dev, e);
                let _ = tc(&["qdisc", "del", "dev", dev, "root"]);
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no shaper available")))
}

/// Active shaper on one interface.
#[derive(Debug, Clone)]
pub struct Shaper {
    pub iface: String,
    pub kind: Kind,
    /// None when ingress shaping is unavailable (no IFB) or disabled.
    pub ifb: Option<String>,
    pub up_kbit: u32,
    pub down_kbit: Option<u32>,
}

impl Shaper {
    /// Install upload shaping, and download shaping when `down_kbit` is Some.
    /// Replaces whatever hifi-wifi qdiscs were there before.
    pub fn install(iface: &str, up_kbit: u32, down_kbit: Option<u32>) -> Result<Self> {
        if !is_valid_iface(iface) {
            bail!("invalid interface name: {}", iface);
        }
        remove(iface);

        let kind = install_root(iface, up_kbit, false)?;
        let mut shaper = Self {
            iface: iface.to_string(),
            kind,
            ifb: None,
            up_kbit,
            down_kbit: None,
        };

        if let Some(down) = down_kbit {
            match install_ingress(iface, down) {
                Ok(ifb) => {
                    shaper.ifb = Some(ifb);
                    shaper.down_kbit = Some(down);
                }
                Err(e) => warn!("Download shaping unavailable on {}: {}", iface, e),
            }
        }

        info!(
            "Shaper installed on {} ({:?}): upload {} kbit, download {}",
            iface,
            kind,
            up_kbit,
            shaper
                .down_kbit
                .map(|d| format!("{} kbit", d))
                .unwrap_or_else(|| "unshaped".into())
        );
        Ok(shaper)
    }

    /// Change rates in place (no qdisc rebuild, no packet loss).
    pub fn set_rates(&mut self, up_kbit: u32, down_kbit: Option<u32>) -> Result<()> {
        if up_kbit != self.up_kbit {
            run("tc", &change_args(self.kind, &self.iface, up_kbit))?;
            self.up_kbit = up_kbit;
        }
        if let (Some(ifb), Some(down)) = (&self.ifb, down_kbit) {
            if Some(down) != self.down_kbit {
                run("tc", &change_args(self.kind, ifb, down))?;
                self.down_kbit = Some(down);
            }
        }
        Ok(())
    }

    /// Whether our qdisc is still on the interface (NetworkManager or a reconnect can reset it).
    pub fn is_present(&self) -> bool {
        let Ok(out) = Command::new("tc").args(["qdisc", "show", "dev", &self.iface, "root"]).output() else {
            return false;
        };
        let s = String::from_utf8_lossy(&out.stdout);
        match self.kind {
            Kind::Cake => s.contains("qdisc cake"),
            Kind::HtbFqCodel => s.contains("qdisc htb 1:"),
            Kind::Tbf => s.contains("qdisc tbf 1:"),
        }
    }
}

fn install_ingress(iface: &str, down_kbit: u32) -> Result<String> {
    let ifb = ifb_name(iface);
    let _ = Command::new("modprobe").arg("ifb").arg("numifbs=0").output();
    let _ = Command::new("ip").args(["link", "add", "name", &ifb, "type", "ifb"]).output();
    run("ip", &["link".into(), "set".into(), "dev".into(), ifb.clone(), "up".into()])?;
    tc(&["qdisc", "replace", "dev", iface, "handle", "ffff:", "ingress"])?;
    // matchall is cheapest; u32 "match everything" is the classic sqm-scripts fallback
    let redirect = ["action", "mirred", "egress", "redirect", "dev", &ifb];
    let base = ["filter", "replace", "dev", iface, "parent", "ffff:", "protocol", "all", "prio", "10"];
    let matchall = [&base[..], &["matchall"], &redirect[..]].concat();
    let u32_all = [&base[..], &["u32", "match", "u32", "0", "0"], &redirect[..]].concat();
    if let Err(e) = tc(&matchall).or_else(|_| tc(&u32_all)) {
        let _ = tc(&["qdisc", "del", "dev", iface, "ingress"]);
        let _ = Command::new("ip").args(["link", "del", &ifb]).output();
        return Err(e);
    }
    if let Err(e) = install_root(&ifb, down_kbit, true) {
        let _ = tc(&["qdisc", "del", "dev", iface, "ingress"]);
        let _ = Command::new("ip").args(["link", "del", &ifb]).output();
        return Err(e);
    }
    Ok(ifb)
}

/// Remove all hifi-wifi shaping from `iface`. Idempotent.
pub fn remove(iface: &str) {
    if !is_valid_iface(iface) {
        return;
    }
    if has_shaper(iface) {
        // Deleting the root restores the kernel default (mq on multi-queue devices).
        let _ = tc(&["qdisc", "del", "dev", iface, "root"]);
    }
    let _ = tc(&["qdisc", "del", "dev", iface, "ingress"]);
    let _ = Command::new("ip").args(["link", "del", &ifb_name(iface)]).output();
    // Pre-3.1 releases shaped downloads on a shared ifb0 with CAKE.
    if has_shaper("ifb0") {
        let _ = tc(&["qdisc", "del", "dev", "ifb0", "root"]);
    }
}

/// True if hifi-wifi (or anyone) put CAKE or our HTB on the root of `iface`.
pub fn has_shaper(iface: &str) -> bool {
    Command::new("tc")
        .args(["qdisc", "show", "dev", iface, "root"])
        .output()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            s.contains("qdisc cake") || s.contains("qdisc htb 1:") || s.contains("qdisc tbf 1:")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ifb_name_fits_kernel_limit() {
        assert_eq!(ifb_name("wlan0"), "ifb4wlan0");
        assert_eq!(ifb_name("wlp1s0f0u1234"), "ifb4wlp1s0f0u12");
        assert!(ifb_name("wlp1s0f0u1234").len() <= 15);
    }

    #[test]
    fn cake_egress_keeps_dscp_and_ingress_washes() {
        let up = root_args(Kind::Cake, "wlan0", 20_000, false).concat().join(" ");
        assert!(up.contains("bandwidth 20000kbit diffserv4 dual-srchost ack-filter"));
        assert!(!up.contains("wash"));
        let down = root_args(Kind::Cake, "ifb4wlan0", 90_000, true).concat().join(" ");
        assert!(down.contains("dual-dsthost wash ingress"));
    }

    #[test]
    fn rate_change_does_not_rebuild() {
        assert_eq!(
            change_args(Kind::Cake, "wlan0", 5_000).join(" "),
            "qdisc change dev wlan0 root cake bandwidth 5000kbit"
        );
        assert!(change_args(Kind::HtbFqCodel, "wlan0", 5_000)
            .join(" ")
            .starts_with("class change dev wlan0 parent 1: classid 1:10 htb rate 5000kbit"));
    }

    #[test]
    fn tbf_bucket_is_one_millisecond() {
        assert!(tbf_params(1_000).join(" ").contains("burst 3028"));
        assert!(tbf_params(1_000_000).join(" ").contains("burst 125000 latency 5ms"));
    }

    #[test]
    fn zero_rate_is_clamped() {
        assert!(root_args(Kind::Cake, "x", 0, false)[0].contains(&"1kbit".to_string()));
    }
}
