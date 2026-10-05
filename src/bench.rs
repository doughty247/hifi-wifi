//! `hifi-wifi bench`: latency under load (bufferbloat) measurement
//!
//! Pings a reflector while idle, then while saturating download, then upload (parallel curl
//! streams). Throughput comes from interface counters, so it counts everything on the link.
//! The result is the latency *increase* under load, which is what makes games and calls stutter.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::network::pinger::{Pinger, Sample};

pub const RESULTS_DIR: &str = "/var/lib/hifi-wifi/bench";
const PING_INTERVAL_MS: u64 = 200;

pub struct Options {
    pub iface: String,
    pub reflector: String,
    pub secs: u64,
    pub warmup_secs: u64,
    pub streams: usize,
    pub download_url: String,
    pub upload_url: String,
    pub label: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Phase {
    pub name: String,
    pub samples: usize,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
    pub jitter_ms: f64,
    /// Approximate: replies missing relative to pings sent at the fixed interval
    pub loss_pct: f64,
    pub down_mbit: f64,
    pub up_mbit: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub label: String,
    pub iface: String,
    pub reflector: String,
    pub version: String,
    pub unix_time: u64,
    pub phases: Vec<Phase>,
}

impl Report {
    fn phase(&self, name: &str) -> Option<&Phase> {
        self.phases.iter().find(|p| p.name == name)
    }

    /// Median latency increase under load, worst of download and upload
    pub fn bloat_ms(&self) -> Option<f64> {
        let idle = self.phase("idle")?.p50_ms;
        ["download", "upload"]
            .iter()
            .filter_map(|n| self.phase(n))
            .filter(|p| p.samples > 0)
            .map(|p| (p.p50_ms - idle).max(0.0))
            .reduce(f64::max)
    }
}

/// Grade on median latency increase, same bands as the Waveform bufferbloat test
pub fn grade(bloat_ms: f64) -> &'static str {
    match bloat_ms {
        b if b < 5.0 => "A+",
        b if b < 30.0 => "A",
        b if b < 60.0 => "B",
        b if b < 200.0 => "C",
        b if b < 400.0 => "D",
        _ => "F",
    }
}

pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Mean absolute difference between consecutive samples (RFC 3550 style, unsmoothed)
pub fn jitter(in_order: &[f64]) -> f64 {
    if in_order.len() < 2 {
        return 0.0;
    }
    in_order.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f64>() / (in_order.len() - 1) as f64
}

fn summarize(name: &str, rtts: &[f64], secs: f64, down_mbit: f64, up_mbit: f64) -> Phase {
    let mut sorted = rtts.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let expected = (secs * 1000.0 / PING_INTERVAL_MS as f64).floor().max(1.0);
    Phase {
        name: name.into(),
        samples: rtts.len(),
        p50_ms: percentile(&sorted, 0.50),
        p90_ms: percentile(&sorted, 0.90),
        p99_ms: percentile(&sorted, 0.99),
        jitter_ms: jitter(rtts),
        loss_pct: ((1.0 - rtts.len() as f64 / expected) * 100.0).clamp(0.0, 100.0),
        down_mbit,
        up_mbit,
    }
}

fn counters(iface: &str) -> (u64, u64) {
    let read = |n: &str| {
        std::fs::read_to_string(format!("/sys/class/net/{}/statistics/{}", iface, n))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    };
    (read("rx_bytes"), read("tx_bytes"))
}

#[derive(Clone, Copy, PartialEq)]
enum Load {
    None,
    Download,
    Upload,
    Both,
}

/// Keep `streams` curl transfers running until `deadline`.
async fn run_load(o: &Options, load: Load, deadline: Instant) {
    let mut tasks = Vec::new();
    for _ in 0..o.streams {
        for upload in [false, true] {
            let wanted = match load {
                Load::None => false,
                Load::Download => !upload,
                Load::Upload => upload,
                Load::Both => true,
            };
            if !wanted {
                continue;
            }
            let url = if upload { o.upload_url.clone() } else { o.download_url.clone() };
            let iface = o.iface.clone();
            tasks.push(tokio::spawn(async move {
                while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                    if left < Duration::from_millis(300) {
                        break;
                    }
                    let mut cmd = Command::new("curl");
                    cmd.args(["-s", "-o", "/dev/null", "--interface", &iface, "--max-time"])
                        .arg(format!("{:.1}", left.as_secs_f64()))
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .kill_on_drop(true);
                    if upload {
                        // Stream zeros from stdin: unbounded upload without buffering in memory
                        let Ok(zero) = std::fs::File::open("/dev/zero") else { break };
                        cmd.args(["-X", "POST", "-H", "Content-Type: application/octet-stream", "-T", "-"])
                            .stdin(Stdio::from(zero));
                    } else {
                        cmd.stdin(Stdio::null());
                    }
                    cmd.arg(&url);
                    match cmd.status().await {
                        Ok(_) => {}
                        Err(_) => break,
                    }
                    // Avoid a hot loop if the server refuses immediately
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }));
        }
    }
    tokio::time::sleep_until(deadline.into()).await;
    for t in tasks {
        t.abort();
    }
}

async fn phase(o: &Options, name: &str, load: Load, secs: u64, rx: &mut mpsc::Receiver<Sample>) -> Phase {
    let start = Instant::now();
    let deadline = start + Duration::from_secs(secs);
    while rx.try_recv().is_ok() {}
    let (rx0, tx0) = counters(&o.iface);

    let mut rtts = Vec::new();
    let load_fut = run_load(o, load, deadline);
    tokio::pin!(load_fut);
    loop {
        tokio::select! {
            _ = &mut load_fut => break,
            Some(s) = rx.recv() => rtts.push(s.rtt_ms),
        }
    }

    let secs_f = start.elapsed().as_secs_f64();
    let (rx1, tx1) = counters(&o.iface);
    let mbit = |a: u64, b: u64| b.saturating_sub(a) as f64 * 8.0 / 1e6 / secs_f;
    summarize(name, &rtts, secs_f, mbit(rx0, rx1), mbit(tx0, tx1))
}

pub async fn run(o: &Options) -> Result<Report> {
    if o.streams == 0 || o.secs < 5 {
        bail!("need at least 1 stream and 5 s per phase");
    }
    let (tx, mut rx) = mpsc::channel(4096);
    let pinger = Pinger::start(std::slice::from_ref(&o.reflector), Some(&o.iface), PING_INTERVAL_MS, tx);
    if pinger.is_empty() {
        bail!("could not start ping to {}", o.reflector);
    }

    // Let the first replies arrive (ARP, route lookup) before measuring idle
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut phases = vec![phase(o, "idle", Load::None, o.secs.min(10), &mut rx).await];
    if phases[0].samples == 0 {
        bail!("no ping replies from {} via {}", o.reflector, o.iface);
    }
    if o.warmup_secs > 0 {
        eprintln!("  warm-up ({} s, not counted)...", o.warmup_secs);
        phase(o, "warmup", Load::Both, o.warmup_secs, &mut rx).await;
        // Let queues drain
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    for (name, load) in [("download", Load::Download), ("upload", Load::Upload)] {
        eprintln!("  {} ({} s)...", name, o.secs);
        phases.push(phase(o, name, load, o.secs, &mut rx).await);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    drop(pinger);

    Ok(Report {
        label: o.label.clone(),
        iface: o.iface.clone(),
        reflector: o.reflector.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        unix_time: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        phases,
    })
}

pub fn print(r: &Report) {
    println!(
        "\n{} ({} via {}):",
        if r.label.is_empty() { "Result" } else { &r.label },
        r.reflector,
        r.iface
    );
    println!(
        "  {:<9} {:>8} {:>8} {:>8} {:>8} {:>6} {:>10} {:>10}",
        "phase", "p50 ms", "p90 ms", "p99 ms", "jitter", "loss", "down Mbit", "up Mbit"
    );
    for p in &r.phases {
        println!(
            "  {:<9} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>5.0}% {:>10.1} {:>10.1}",
            p.name, p.p50_ms, p.p90_ms, p.p99_ms, p.jitter_ms, p.loss_pct, p.down_mbit, p.up_mbit
        );
    }
    if let Some(b) = r.bloat_ms() {
        println!("  Latency increase under load: +{:.1} ms (grade {})", b, grade(b));
    }
}

pub fn print_comparison(off: &Report, on: &Report) {
    println!("\nA/B comparison (same network, back to back):");
    println!(
        "  {:<9} {:>16} {:>16} {:>18} {:>18}",
        "phase", "p50 off -> on", "p99 off -> on", "down Mbit", "up Mbit"
    );
    for name in ["idle", "download", "upload"] {
        if let (Some(a), Some(b)) = (off.phase(name), on.phase(name)) {
            println!(
                "  {:<9} {:>7.1} -> {:<6.1} {:>7.1} -> {:<6.1} {:>8.1} -> {:<7.1} {:>8.1} -> {:<7.1}",
                name, a.p50_ms, b.p50_ms, a.p99_ms, b.p99_ms, a.down_mbit, b.down_mbit, a.up_mbit, b.up_mbit
            );
        }
    }
    if let (Some(a), Some(b)) = (off.bloat_ms(), on.bloat_ms()) {
        println!(
            "  Latency increase under load: +{:.1} ms ({}) -> +{:.1} ms ({})",
            a,
            grade(a),
            b,
            grade(b)
        );
    }
    println!("  Network conditions change between runs; repeat a few times before drawing conclusions.");
}

pub fn save(r: &Report) -> Option<String> {
    std::fs::create_dir_all(RESULTS_DIR).ok()?;
    let safe: String = r
        .label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    let path = Path::new(RESULTS_DIR).join(format!("{}-{}.json", r.unix_time, safe));
    std::fs::write(&path, serde_json::to_string_pretty(r).ok()?).ok()?;
    Some(path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grades_match_waveform_bands() {
        assert_eq!(grade(2.0), "A+");
        assert_eq!(grade(25.0), "A");
        assert_eq!(grade(45.0), "B");
        assert_eq!(grade(150.0), "C");
        assert_eq!(grade(300.0), "D");
        assert_eq!(grade(900.0), "F");
    }

    #[test]
    fn percentiles_and_jitter() {
        let v: Vec<f64> = (1..=100).map(|x| x as f64).collect();
        assert_eq!(percentile(&v, 0.5), 51.0);
        assert_eq!(percentile(&v, 0.99), 99.0);
        assert_eq!(percentile(&[], 0.5).is_nan(), true);
        assert_eq!(jitter(&[10.0, 12.0, 10.0, 10.0]), 4.0 / 3.0);
        assert_eq!(jitter(&[5.0]), 0.0);
    }

    #[test]
    fn bloat_is_worst_direction_median_increase() {
        let p = |n: &str, p50: f64| Phase { name: n.into(), samples: 10, p50_ms: p50, ..Default::default() };
        let r = Report {
            label: String::new(),
            iface: "wlan0".into(),
            reflector: "1.1.1.1".into(),
            version: String::new(),
            unix_time: 0,
            phases: vec![p("idle", 20.0), p("download", 85.0), p("upload", 40.0)],
        };
        assert_eq!(r.bloat_ms(), Some(65.0));
    }

    #[test]
    fn loss_is_estimated_from_interval() {
        let rtts = vec![10.0; 40];
        let ph = summarize("x", &rtts, 10.0, 0.0, 0.0);
        assert!((ph.loss_pct - 20.0).abs() < 0.01);
    }
}
