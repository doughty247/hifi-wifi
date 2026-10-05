//! `hifi-wifi doctor`: explain what is limiting this Wi-Fi connection
//!
//! Most stutter comes from the radio environment (weak signal, a busy channel, 2.4 GHz),
//! which no client-side tuning can fix. This reports those causes with concrete fixes.

use std::process::Command;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Good,
    Warn,
    Bad,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub severity: Severity,
    pub title: String,
    pub detail: String,
    pub fix: Option<String>,
}

fn finding(
    severity: Severity,
    title: impl Into<String>,
    detail: impl Into<String>,
    fix: Option<&str>,
) -> Finding {
    Finding {
        severity,
        title: title.into(),
        detail: detail.into(),
        fix: fix.map(String::from),
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Link {
    pub ssid: Option<String>,
    pub freq_mhz: Option<f64>,
    pub signal_dbm: Option<i32>,
    pub tx_mbit: Option<f64>,
    pub rx_mbit: Option<f64>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Station {
    pub tx_packets: u64,
    pub tx_retries: u64,
    pub tx_failed: u64,
    pub beacon_loss: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Survey {
    pub active_ms: u64,
    pub busy_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bss {
    pub ssid: String,
    pub freq_mhz: f64,
    pub signal_dbm: f64,
}

fn value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.trim().strip_prefix(key).map(str::trim)
}

fn first_num<T: std::str::FromStr>(s: &str) -> Option<T> {
    s.split_whitespace().next()?.parse().ok()
}

pub fn parse_link(out: &str) -> Option<Link> {
    if !out.contains("Connected to") {
        return None;
    }
    let mut l = Link::default();
    for line in out.lines() {
        if let Some(v) = value(line, "SSID:") {
            l.ssid = Some(v.to_string());
        } else if let Some(v) = value(line, "freq:") {
            l.freq_mhz = first_num(v);
        } else if let Some(v) = value(line, "signal:") {
            l.signal_dbm = first_num(v);
        } else if let Some(v) = value(line, "tx bitrate:") {
            l.tx_mbit = first_num(v);
        } else if let Some(v) = value(line, "rx bitrate:") {
            l.rx_mbit = first_num(v);
        }
    }
    Some(l)
}

pub fn parse_station(out: &str) -> Option<Station> {
    let mut s = Station::default();
    let mut seen = false;
    for line in out.lines() {
        let num = |k: &str| value(line, k).and_then(first_num::<u64>);
        if let Some(v) = num("tx packets:") {
            s.tx_packets = v;
            seen = true;
        } else if let Some(v) = num("tx retries:") {
            s.tx_retries = v;
        } else if let Some(v) = num("tx failed:") {
            s.tx_failed = v;
        } else if let Some(v) = num("beacon loss:") {
            s.beacon_loss = v;
        }
    }
    seen.then_some(s)
}

/// Survey entry for the channel in use
pub fn parse_survey(out: &str) -> Option<Survey> {
    let mut in_use = false;
    let mut s = Survey::default();
    for line in out.lines() {
        if let Some(v) = value(line, "frequency:") {
            if in_use {
                break;
            }
            in_use = v.contains("[in use]");
        } else if in_use {
            if let Some(v) = value(line, "channel active time:") {
                s.active_ms = first_num(v).unwrap_or(0);
            } else if let Some(v) = value(line, "channel busy time:") {
                s.busy_ms = first_num(v).unwrap_or(0);
            }
        }
    }
    (in_use && s.active_ms > 0).then_some(s)
}

pub fn parse_scan(out: &str) -> Vec<Bss> {
    let mut list = Vec::new();
    let mut cur: Option<(Option<String>, Option<f64>, Option<f64>)> = None;
    let flush = |c: Option<(Option<String>, Option<f64>, Option<f64>)>, list: &mut Vec<Bss>| {
        if let Some((Some(ssid), Some(freq_mhz), Some(signal_dbm))) = c {
            list.push(Bss {
                ssid,
                freq_mhz,
                signal_dbm,
            });
        }
    };
    for line in out.lines() {
        if line.starts_with("BSS ") {
            flush(cur.take(), &mut list);
            cur = Some((None, None, None));
        } else if let Some(c) = cur.as_mut() {
            if let Some(v) = value(line, "SSID:") {
                c.0 = Some(v.to_string());
            } else if let Some(v) = value(line, "freq:") {
                c.1 = first_num(v);
            } else if let Some(v) = value(line, "signal:") {
                c.2 = first_num(v);
            }
        }
    }
    flush(cur, &mut list);
    list
}

fn band(freq: f64) -> &'static str {
    match freq as u32 {
        0..=2500 => "2.4 GHz",
        2501..=5924 => "5 GHz",
        _ => "6 GHz",
    }
}

/// Retry ratio over a window when there was enough traffic, else since association
fn retry_ratio(before: &Station, after: &Station) -> (f64, f64, bool) {
    let dp = after.tx_packets.saturating_sub(before.tx_packets);
    if dp >= 100 {
        let dr = after.tx_retries.saturating_sub(before.tx_retries);
        let df = after.tx_failed.saturating_sub(before.tx_failed);
        (dr as f64 / dp as f64, df as f64 / dp as f64, true)
    } else {
        let p = after.tx_packets.max(1) as f64;
        (
            after.tx_retries as f64 / p,
            after.tx_failed as f64 / p,
            false,
        )
    }
}

pub struct Inputs {
    pub link: Link,
    pub station: Option<(Station, Station)>,
    pub survey: Option<Survey>,
    pub power_save_on: Option<bool>,
    pub scan: Vec<Bss>,
}

/// Turn measurements into findings. Pure, so the thresholds are unit tested.
pub fn analyze(i: &Inputs) -> Vec<Finding> {
    let mut f = Vec::new();
    let freq = i.link.freq_mhz.unwrap_or(0.0);

    if let Some(sig) = i.link.signal_dbm {
        let (sev, word) = match sig {
            s if s >= -60 => (Severity::Good, "strong"),
            s if s >= -70 => (Severity::Good, "fine"),
            s if s >= -78 => (Severity::Warn, "weak"),
            _ => (Severity::Bad, "very weak"),
        };
        f.push(finding(
            sev,
            format!("Signal {} dBm ({})", sig, word),
            "Below about -70 dBm the link drops to slower rates and retransmits more, which shows up as latency spikes.",
            (sev != Severity::Good).then_some("Move closer to the access point, remove obstructions, or add a mesh node / access point nearer to where you play."),
        ));
    }

    if freq > 0.0 {
        if band(freq) == "2.4 GHz" {
            let better = i
                .scan
                .iter()
                .filter(|b| {
                    Some(&b.ssid) == i.link.ssid.as_ref()
                        && b.freq_mhz > 2500.0
                        && b.signal_dbm >= -72.0
                })
                .max_by(|a, b| a.signal_dbm.total_cmp(&b.signal_dbm));
            match better {
                Some(b) => f.push(finding(
                    Severity::Warn,
                    format!("On 2.4 GHz, but {} is available ({:.0} dBm)", band(b.freq_mhz), b.signal_dbm),
                    "2.4 GHz is slower and shared with neighbors, Bluetooth and microwaves.",
                    Some("hifi-wifi band steering prefers 5/6 GHz when it is strong enough. If you stay on 2.4 GHz, give the 5 GHz network its own name on the router and connect to it."),
                )),
                None => f.push(finding(
                    Severity::Warn,
                    "On 2.4 GHz",
                    "2.4 GHz is slower and crowded. No usable 5/6 GHz network with this name was seen in the last scan.",
                    Some("Enable 5 GHz on the router, or use a 5 GHz network if one exists."),
                )),
            }
        } else {
            f.push(finding(
                Severity::Good,
                format!("Band {}", band(freq)),
                "",
                None,
            ));
        }
    }

    if let Some(tx) = i.link.tx_mbit {
        if tx < 50.0 {
            f.push(finding(
                Severity::Warn,
                format!("Low link rate ({:.0} Mbit/s TX)", tx),
                "The radio negotiated a slow rate, usually from weak signal or interference. Game streams at 50+ Mbit/s will not fit.",
                Some("Improve signal (see above). For streaming, lower the stream bitrate until the link improves."),
            ));
        }
    }

    if let Some((before, after)) = &i.station {
        let (retry, failed, live) = retry_ratio(before, after);
        let when = if live {
            "over the last 3 s"
        } else {
            "since connecting"
        };
        let sev = match retry {
            r if r < 0.10 => Severity::Good,
            r if r < 0.25 => Severity::Warn,
            _ => Severity::Bad,
        };
        f.push(finding(
            sev,
            format!("Retransmissions {:.0}% {}", retry * 100.0, when),
            "Each retry costs airtime and adds delay. High retries mean interference, a busy channel or weak signal.",
            (sev != Severity::Good).then_some("Check channel busy time and signal below. On the router, pick a less crowded channel (or let it auto-select) and avoid 160 MHz on DFS channels if it keeps changing."),
        ));
        if failed > 0.01 {
            f.push(finding(
                Severity::Bad,
                format!("{:.1}% of packets failed after all retries", failed * 100.0),
                "These are real packet losses, felt as stutter or rubber-banding.",
                Some("Same fixes as for retransmissions; this link is at its limit."),
            ));
        }
        if after.beacon_loss > 0 {
            f.push(finding(
                Severity::Warn,
                format!("{} missed beacons", after.beacon_loss),
                "The device sometimes cannot hear the access point at all; this precedes disconnects.",
                Some("Usually signal or interference. Also check for a driver/firmware update."),
            ));
        }
    }

    if let Some(s) = i.survey {
        let busy = s.busy_ms as f64 / s.active_ms as f64;
        let sev = match busy {
            b if b < 0.40 => Severity::Good,
            b if b < 0.70 => Severity::Warn,
            _ => Severity::Bad,
        };
        f.push(finding(
            sev,
            format!("Channel busy {:.0}% of the time", busy * 100.0),
            "Airtime used by every device on this channel, including neighbors' networks. Your device has to wait for the rest.",
            (sev != Severity::Good).then_some("Move the router to a less used channel (a Wi-Fi analyzer app shows them), prefer 5/6 GHz, and limit other heavy users while playing."),
        ));
    }

    match i.power_save_on {
        Some(true) => f.push(finding(
            Severity::Warn,
            "Wi-Fi power save is on",
            "The radio naps between beacons, adding tens of ms of delay to incoming packets.",
            Some("sudo hifi-wifi power-save off (or 'adaptive' to keep it only on battery when idle)."),
        )),
        Some(false) => f.push(finding(Severity::Good, "Wi-Fi power save off", "", None)),
        None => {}
    }

    f
}

fn iw(args: &[&str]) -> Option<String> {
    let out = Command::new("iw").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

pub async fn collect(iface: &str) -> Option<Inputs> {
    let link = parse_link(&iw(&["dev", iface, "link"])?)?;
    let before = iw(&["dev", iface, "station", "dump"]).and_then(|o| parse_station(&o));
    tokio::time::sleep(Duration::from_secs(3)).await;
    let after = iw(&["dev", iface, "station", "dump"]).and_then(|o| parse_station(&o));
    Some(Inputs {
        link,
        station: before.zip(after),
        survey: iw(&["dev", iface, "survey", "dump"]).and_then(|o| parse_survey(&o)),
        power_save_on: iw(&["dev", iface, "get", "power_save"]).map(|o| o.contains(": on")),
        scan: iw(&["dev", iface, "scan", "dump"])
            .map(|o| parse_scan(&o))
            .unwrap_or_default(),
    })
}

pub fn print(iface: &str, link: &Link, findings: &[Finding]) {
    println!(
        "hifi-wifi doctor: {} on {} ({})\n",
        iface,
        link.ssid.as_deref().unwrap_or("?"),
        link.freq_mhz.map(band).unwrap_or("?")
    );
    let mut sorted: Vec<&Finding> = findings.iter().collect();
    sorted.sort_by_key(|f| std::cmp::Reverse(f.severity));
    for x in sorted {
        let tag = match x.severity {
            Severity::Good => "[ ok ]",
            Severity::Warn => "[warn]",
            Severity::Bad => "[BAD ]",
        };
        println!("{} {}", tag, x.title);
        if x.severity != Severity::Good {
            if !x.detail.is_empty() {
                println!("       {}", x.detail);
            }
            if let Some(fix) = &x.fix {
                println!("       Fix: {}", fix);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINK: &str = "Connected to aa:bb:cc:dd:ee:ff (on wlan0)\n\tSSID: Home\n\tfreq: 2437.0\n\tRX: 1 bytes (1 packets)\n\tsignal: -74 dBm\n\trx bitrate: 72.2 MBit/s MCS 7 short GI\n\ttx bitrate: 39.0 MBit/s MCS 4\n";
    const STATION: &str = "Station aa:bb:cc:dd:ee:ff (on wlan0)\n\tinactive time:\t10 ms\n\ttx packets:\t1000\n\ttx retries:\t300\n\ttx failed:\t20\n\tbeacon loss:\t2\n\tsignal:  \t-74 [-75, -76] dBm\n";
    const SURVEY: &str = "Survey data from wlan0\n\tfrequency:\t\t\t2412 MHz\n\tchannel active time:\t\t100 ms\n\tchannel busy time:\t\t90 ms\nSurvey data from wlan0\n\tfrequency:\t\t\t2437 MHz [in use]\n\tnoise:\t\t\t\t-92 dBm\n\tchannel active time:\t\t10000 ms\n\tchannel busy time:\t\t7500 ms\nSurvey data from wlan0\n\tfrequency:\t\t\t5180 MHz\n\tchannel active time:\t\t50 ms\n\tchannel busy time:\t\t1 ms\n";
    const SCAN: &str = "BSS aa:bb:cc:dd:ee:ff(on wlan0) -- associated\n\tfreq: 2437\n\tsignal: -74.00 dBm\n\tSSID: Home\nBSS aa:bb:cc:dd:ee:00(on wlan0)\n\tfreq: 5180\n\tsignal: -66.00 dBm\n\tSSID: Home\nBSS 11:22:33:44:55:66(on wlan0)\n\tfreq: 5745\n\tsignal: -50.00 dBm\n\tSSID: Neighbor\n";

    #[test]
    fn parses_iw_output() {
        let l = parse_link(LINK).unwrap();
        assert_eq!(l.ssid.as_deref(), Some("Home"));
        assert_eq!(l.freq_mhz, Some(2437.0));
        assert_eq!(l.signal_dbm, Some(-74));
        assert_eq!(l.tx_mbit, Some(39.0));
        assert_eq!(parse_link("Not connected.\n"), None);

        let s = parse_station(STATION).unwrap();
        assert_eq!(
            (s.tx_packets, s.tx_retries, s.tx_failed, s.beacon_loss),
            (1000, 300, 20, 2)
        );

        let v = parse_survey(SURVEY).unwrap();
        assert_eq!((v.active_ms, v.busy_ms), (10000, 7500));

        let b = parse_scan(SCAN);
        assert_eq!(b.len(), 3);
        assert_eq!(
            b[1],
            Bss {
                ssid: "Home".into(),
                freq_mhz: 5180.0,
                signal_dbm: -66.0
            }
        );
    }

    #[test]
    fn bad_2g_link_produces_actionable_findings() {
        let s = parse_station(STATION).unwrap();
        let f = analyze(&Inputs {
            link: parse_link(LINK).unwrap(),
            station: Some((s, s)),
            survey: parse_survey(SURVEY),
            power_save_on: Some(true),
            scan: parse_scan(SCAN),
        });
        let titles: Vec<&str> = f.iter().map(|x| x.title.as_str()).collect();
        assert!(titles.iter().any(|t| t.contains("weak")), "{:?}", titles);
        assert!(
            titles
                .iter()
                .any(|t| t.contains("5 GHz is available (-66 dBm)")),
            "{:?}",
            titles
        );
        assert!(titles.iter().any(|t| t.starts_with("Low link rate")));
        assert!(titles
            .iter()
            .any(|t| t.starts_with("Retransmissions 30% since connecting")));
        assert!(titles
            .iter()
            .any(|t| t.contains("failed after all retries")));
        assert!(titles.iter().any(|t| t.starts_with("Channel busy 75%")));
        assert!(titles.iter().any(|t| t.contains("power save is on")));
        assert!(f
            .iter()
            .filter(|x| x.severity != Severity::Good)
            .all(|x| x.fix.is_some()));
    }

    #[test]
    fn live_window_used_when_traffic_flows() {
        let a = Station {
            tx_packets: 1000,
            tx_retries: 500,
            tx_failed: 0,
            beacon_loss: 0,
        };
        let b = Station {
            tx_packets: 2000,
            tx_retries: 550,
            tx_failed: 0,
            beacon_loss: 0,
        };
        let (r, _, live) = retry_ratio(&a, &b);
        assert!(live);
        assert!((r - 0.05).abs() < 1e-9);
    }

    #[test]
    fn healthy_link_is_all_good() {
        let s = Station {
            tx_packets: 5000,
            tx_retries: 100,
            tx_failed: 0,
            beacon_loss: 0,
        };
        let f = analyze(&Inputs {
            link: Link {
                ssid: Some("Home".into()),
                freq_mhz: Some(5180.0),
                signal_dbm: Some(-52),
                tx_mbit: Some(866.7),
                rx_mbit: Some(866.7),
            },
            station: Some((s, s)),
            survey: Some(Survey {
                active_ms: 1000,
                busy_ms: 150,
            }),
            power_save_on: Some(false),
            scan: vec![],
        });
        assert!(f.iter().all(|x| x.severity == Severity::Good), "{:?}", f);
    }
}
