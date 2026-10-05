//! Autorate: latency-driven bufferbloat control
//!
//! Keeps the shaper just below the real bottleneck (ISP line or Wi-Fi link), wherever it is.
//! Method follows cake-autorate (OpenWrt), the established approach:
//! - Ping a few reflectors and track each one's baseline RTT.
//! - When at least half of them see RTT rise above baseline by `delay_threshold_ms`
//!   while we are pushing traffic close to our recent peak, the queue is ours:
//!   cut that direction to 90% of what it actually achieved.
//! - When the shaper is saturated and latency is clean, raise it 5% at a time.
//!
//! Bloat caused by other devices (our traffic far below our own recent peak) never cuts our rate.

use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::config::structs::{AutorateConfig, AutorateMode};
use crate::network::pinger::{Pinger, Sample};
use crate::network::shaper::{self, Shaper};

pub const STATUS_PATH: &str = "/run/hifi-wifi/autorate.json";
const LEARNED_PATH: &str = "/var/lib/hifi-wifi/autorate-learned.json";

const TICK: Duration = Duration::from_millis(500);
const ALPHA_DELTA: f64 = 0.3;
const ALPHA_BASE_UP: f64 = 0.001;
const ALPHA_BASE_DOWN: f64 = 0.9;
const STALE_TICKS: u64 = 6;
const DECREASE_FACTOR: f64 = 0.9;
/// Below the throughput last seen at the bottleneck: recover quickly
const INCREASE_FACTOR: f64 = 1.05;
/// Above it: probe gently, since that is where the queue starts building
const PROBE_FACTOR: f64 = 1.015;
const SATURATED: f64 = 0.75;
const BUSY_VS_PEAK: f64 = 0.5;
const PEAK_DECAY: f64 = 0.995;
const DECREASE_REFRACTORY_TICKS: u64 = 4;
const INCREASE_REFRACTORY_TICKS: u64 = 2;
/// Traffic that counts as "busy" for `mode = "busy"` (kbit/s, either direction)
const BUSY_DL_KBIT: f64 = 2_000.0;
const BUSY_UL_KBIT: f64 = 1_000.0;
const BUSY_HOLD: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct Direction {
    pub rate_kbit: f64,
    pub min_kbit: f64,
    pub max_kbit: f64,
    peak_kbit: f64,
    /// Rate set by the last bloat cut: known to be just under the bottleneck
    bottleneck_kbit: Option<f64>,
    pub enabled: bool,
}

impl Direction {
    pub fn new(rate_kbit: f64, min_kbit: f64, max_kbit: f64) -> Self {
        let max_kbit = max_kbit.max(min_kbit);
        Self {
            rate_kbit: rate_kbit.clamp(min_kbit, max_kbit),
            min_kbit,
            max_kbit,
            peak_kbit: 0.0,
            bottleneck_kbit: None,
            enabled: true,
        }
    }

    fn clamp(&self, v: f64) -> f64 {
        v.clamp(self.min_kbit, self.max_kbit)
    }

    pub fn set_max(&mut self, max_kbit: f64) {
        self.max_kbit = max_kbit.max(self.min_kbit);
        self.rate_kbit = self.clamp(self.rate_kbit);
    }
}

#[derive(Debug, Clone, Default)]
struct Reflector {
    baseline_ms: Option<f64>,
    delta_ms: f64,
    last_tick: u64,
}

/// Pure rate controller. Feed it RTT samples and per-tick achieved throughput.
#[derive(Debug, Clone)]
pub struct Controller {
    threshold_ms: f64,
    pub dl: Direction,
    pub ul: Direction,
    reflectors: Vec<Reflector>,
    tick: u64,
    last_decrease_tick: Option<u64>,
    pub bloat_events: u64,
}

impl Controller {
    pub fn new(threshold_ms: f64, dl: Direction, ul: Direction) -> Self {
        Self {
            threshold_ms,
            dl,
            ul,
            reflectors: Vec::new(),
            tick: 0,
            last_decrease_tick: None,
            bloat_events: 0,
        }
    }

    pub fn on_sample(&mut self, reflector: usize, rtt_ms: f64) {
        if self.reflectors.len() <= reflector {
            self.reflectors.resize(reflector + 1, Reflector::default());
        }
        let threshold = self.threshold_ms;
        let r = &mut self.reflectors[reflector];
        r.last_tick = self.tick;
        let Some(base) = r.baseline_ms else {
            r.baseline_ms = Some(rtt_ms);
            return;
        };
        let base = if rtt_ms < base {
            ALPHA_BASE_DOWN * rtt_ms + (1.0 - ALPHA_BASE_DOWN) * base
        } else if rtt_ms - base < threshold {
            // Track slow path changes, but never learn bloat as the new normal
            ALPHA_BASE_UP * rtt_ms + (1.0 - ALPHA_BASE_UP) * base
        } else {
            base
        };
        r.baseline_ms = Some(base);
        r.delta_ms = ALPHA_DELTA * (rtt_ms - base) + (1.0 - ALPHA_DELTA) * r.delta_ms;
    }

    fn live(&self) -> impl Iterator<Item = &Reflector> {
        let tick = self.tick;
        self.reflectors
            .iter()
            .filter(move |r| r.baseline_ms.is_some() && tick.saturating_sub(r.last_tick) <= STALE_TICKS)
    }

    /// (bloated reflectors, responsive reflectors)
    pub fn bloat_votes(&self) -> (usize, usize) {
        let live: Vec<_> = self.live().collect();
        let bloated = live.iter().filter(|r| r.delta_ms > self.threshold_ms).count();
        (bloated, live.len())
    }

    pub fn is_bloated(&self) -> bool {
        let (bloated, live) = self.bloat_votes();
        bloated > 0 && bloated * 2 >= live
    }

    pub fn baselines(&self) -> Vec<Option<f64>> {
        self.reflectors.iter().map(|r| r.baseline_ms).collect()
    }

    /// Start from known-good baselines (they still drop instantly if lower RTTs show up)
    pub fn seed_baselines(&mut self, baselines: &[Option<f64>]) {
        self.reflectors = baselines
            .iter()
            .map(|b| Reflector {
                baseline_ms: *b,
                delta_ms: 0.0,
                last_tick: 0,
            })
            .collect();
    }

    pub fn baseline_ms(&self) -> Option<f64> {
        median(self.live().filter_map(|r| r.baseline_ms).collect())
    }

    pub fn delta_ms(&self) -> Option<f64> {
        median(self.live().map(|r| r.delta_ms).collect())
    }

    /// Advance one tick with the throughput achieved since the last tick.
    /// Returns true when a rate changed.
    pub fn tick(&mut self, dl_kbit: f64, ul_kbit: f64) -> bool {
        self.tick += 1;
        let before = (self.dl.rate_kbit, self.ul.rate_kbit);
        let since_decrease = self
            .last_decrease_tick
            .map(|t| self.tick - t)
            .unwrap_or(u64::MAX);
        let bloated = self.is_bloated();

        for (dir, achieved) in [(&mut self.dl, dl_kbit), (&mut self.ul, ul_kbit)] {
            dir.peak_kbit = achieved.max(dir.peak_kbit * PEAK_DECAY);
        }

        let mut cut = false;
        if bloated {
            if since_decrease >= DECREASE_REFRACTORY_TICKS {
                // Directions carrying real traffic near their own recent peak are suspects.
                // Cut those loading their shaper heavily, else only the most loaded one.
                let load = |d: &Direction, a: f64| {
                    let ours = d.enabled
                        && a >= 2.0 * d.min_kbit
                        && a / d.peak_kbit.max(d.min_kbit) > BUSY_VS_PEAK;
                    ours.then_some(a / d.rate_kbit)
                };
                let loads = [load(&self.dl, dl_kbit), load(&self.ul, ul_kbit)];
                let heavy = loads.iter().any(|l| l.is_some_and(|l| l >= BUSY_VS_PEAK));
                let top = loads.iter().flatten().cloned().fold(f64::MIN, f64::max);
                for ((dir, achieved), l) in [(&mut self.dl, dl_kbit), (&mut self.ul, ul_kbit)]
                    .into_iter()
                    .zip(loads)
                {
                    let Some(l) = l else { continue };
                    if (heavy && l >= BUSY_VS_PEAK) || (!heavy && l == top) {
                        dir.rate_kbit = dir.clamp(dir.rate_kbit.min(achieved) * DECREASE_FACTOR);
                        dir.bottleneck_kbit = Some(dir.rate_kbit);
                        cut = true;
                    }
                }
            }
        } else if since_decrease >= INCREASE_REFRACTORY_TICKS {
            for (dir, achieved) in [(&mut self.dl, dl_kbit), (&mut self.ul, ul_kbit)] {
                if dir.enabled && achieved / dir.rate_kbit > SATURATED {
                    let below_bottleneck = dir.bottleneck_kbit.is_none_or(|b| dir.rate_kbit < b);
                    let factor = if below_bottleneck { INCREASE_FACTOR } else { PROBE_FACTOR };
                    dir.rate_kbit = dir.clamp(dir.rate_kbit * factor);
                }
            }
        }
        if cut {
            self.last_decrease_tick = Some(self.tick);
            self.bloat_events += 1;
        }
        before != (self.dl.rate_kbit, self.ul.rate_kbit)
    }
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    Some(v[v.len() / 2])
}

/// What the governor wants shaped right now
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Target {
    /// Interface carrying the default route
    pub iface: Option<String>,
    /// SSID (Wi-Fi) or "wired:<iface>", used to remember learned rates
    pub network_key: Option<String>,
    /// Physical link rate, used as the upper bound when no limits are configured
    pub link_kbit: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Status {
    pub iface: String,
    pub network: Option<String>,
    pub shaping: bool,
    pub shaper: Option<String>,
    pub download_kbit: Option<u32>,
    pub upload_kbit: u32,
    pub baseline_ms: Option<f64>,
    pub delay_ms: Option<f64>,
    pub reflectors_responding: usize,
    pub bloat_events: u64,
    pub updated_unix: u64,
}

pub fn read_status() -> Option<Status> {
    serde_json::from_str(&std::fs::read_to_string(STATUS_PATH).ok()?).ok()
}

/// What we remember about one network between sessions
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Learned {
    /// Rates found after at least one bloat correction
    rates_kbit: Option<(u32, u32)>,
    /// Per-reflector baseline RTTs, so a session that starts under load has a clean reference
    baselines_ms: Vec<Option<f64>>,
}

fn load_learned() -> HashMap<String, Learned> {
    std::fs::read_to_string(LEARNED_PATH)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_learned(key: &str, entry: Learned) {
    let mut all = load_learned();
    all.insert(key.to_string(), entry);
    if let Ok(json) = serde_json::to_string_pretty(&all) {
        let _ = std::fs::write(LEARNED_PATH, json);
    }
}

fn read_counter(iface: &str, name: &str) -> Option<u64> {
    std::fs::read_to_string(format!("/sys/class/net/{}/statistics/{}", iface, name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct Session {
    iface: String,
    key: Option<String>,
    ctrl: Controller,
    shaper: Option<Shaper>,
    pinger: Option<Pinger>,
    last_counters: Option<(u64, u64, Instant)>,
    busy_until: Option<Instant>,
    /// Whether the running pinger uses the fast (shaping) interval
    pinger_fast: bool,
    ticks: u64,
    /// Rates changed after a bloat correction and are worth remembering
    rates_learned: bool,
}

/// Runs the controller loop; drop/shutdown tears everything down.
pub struct AutorateHandle {
    tx: watch::Sender<Target>,
    join: JoinHandle<()>,
}

impl AutorateHandle {
    pub fn set_target(&self, target: Target) {
        self.tx.send_if_modified(|t| {
            if *t != target {
                *t = target;
                true
            } else {
                false
            }
        });
    }

    /// Remove the shaper and stop probing, waiting until it is done.
    pub async fn shutdown(self) {
        drop(self.tx);
        let _ = self.join.await;
    }
}

pub struct Settings {
    pub cfg: AutorateConfig,
    pub internet_down_mbit: Option<u32>,
    pub internet_up_mbit: Option<u32>,
    pub shape_download: bool,
}

pub fn spawn(settings: Settings) -> AutorateHandle {
    let (tx, rx) = watch::channel(Target::default());
    let join = tokio::spawn(run(settings, rx));
    AutorateHandle { tx, join }
}

impl Settings {
    fn limits(&self, link_kbit: Option<u32>) -> ((f64, f64), (f64, f64)) {
        let link = link_kbit.map(|k| k as f64).unwrap_or(1_000_000.0);
        let max_dl = self
            .cfg
            .max_download_mbit
            .or(self.internet_down_mbit.map(|m| m as f64))
            .map(|m| m * 1000.0)
            .unwrap_or(link);
        let max_ul = self
            .cfg
            .max_upload_mbit
            .or(self.internet_up_mbit.map(|m| m as f64))
            .map(|m| m * 1000.0)
            .unwrap_or(link);
        (
            (self.cfg.min_download_mbit * 1000.0, max_dl),
            (self.cfg.min_upload_mbit * 1000.0, max_ul),
        )
    }

    fn new_session(&self, target: &Target) -> Option<Session> {
        let iface = target.iface.clone()?;
        let ((min_dl, max_dl), (min_ul, max_ul)) = self.limits(target.link_kbit);
        let learned = target
            .network_key
            .as_ref()
            .filter(|_| self.cfg.remember_rates)
            .and_then(|k| load_learned().remove(k))
            .unwrap_or_default();
        let (dl, ul) = match learned.rates_kbit {
            Some((d, u)) => {
                info!("Autorate: using remembered rates for this network ({} / {} kbit)", d, u);
                (d as f64, u as f64)
            }
            None => (max_dl, max_ul),
        };
        let mut ctrl = Controller::new(
            self.cfg.delay_threshold_ms,
            Direction::new(dl, min_dl, max_dl),
            Direction::new(ul, min_ul, max_ul),
        );
        ctrl.seed_baselines(&learned.baselines_ms);
        ctrl.dl.enabled = self.shape_download;
        Some(Session {
            iface,
            key: target.network_key.clone(),
            ctrl,
            shaper: None,
            pinger: None,
            last_counters: None,
            busy_until: None,
            pinger_fast: false,
            ticks: 0,
            rates_learned: false,
        })
    }
}

async fn run(settings: Settings, mut target_rx: watch::Receiver<Target>) {
    let (sample_tx, mut sample_rx) = mpsc::channel::<Sample>(512);
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut session: Option<Session> = None;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let target = target_rx.borrow().clone();
                let stale = match (&session, &target.iface) {
                    (Some(s), Some(i)) => &s.iface != i || s.key != target.network_key,
                    (Some(_), None) => true,
                    (None, _) => false,
                };
                if stale {
                    if let Some(s) = session.take() {
                        teardown(s, &settings);
                    }
                }
                if session.is_none() {
                    session = settings.new_session(&target);
                }
                if let Some(s) = session.as_mut() {
                    let ((_, max_dl), (_, max_ul)) = settings.limits(target.link_kbit);
                    s.ctrl.dl.set_max(max_dl);
                    s.ctrl.ul.set_max(max_ul);
                    step(s, &settings, &sample_tx);
                }
            }
            Some(sample) = sample_rx.recv() => {
                if let Some(s) = session.as_mut() {
                    s.ctrl.on_sample(sample.reflector, sample.rtt_ms);
                }
            }
            changed = target_rx.changed() => {
                if changed.is_err() {
                    break;
                }
            }
        }
    }

    if let Some(s) = session.take() {
        teardown(s, &settings);
    }
    let _ = std::fs::remove_file(STATUS_PATH);
    info!("Autorate stopped");
}

fn step(s: &mut Session, settings: &Settings, sample_tx: &mpsc::Sender<Sample>) {
    s.ticks += 1;
    let now = Instant::now();
    let (Some(rx), Some(tx)) = (read_counter(&s.iface, "rx_bytes"), read_counter(&s.iface, "tx_bytes")) else {
        return;
    };
    let (dl_kbit, ul_kbit) = match s.last_counters {
        Some((prx, ptx, t)) => {
            let secs = now.duration_since(t).as_secs_f64().max(0.05);
            (
                rx.saturating_sub(prx) as f64 * 8.0 / 1000.0 / secs,
                tx.saturating_sub(ptx) as f64 * 8.0 / 1000.0 / secs,
            )
        }
        None => (0.0, 0.0),
    };
    s.last_counters = Some((rx, tx, now));

    if dl_kbit >= BUSY_DL_KBIT || ul_kbit >= BUSY_UL_KBIT {
        s.busy_until = Some(now + BUSY_HOLD);
    }
    let active = match settings.cfg.mode {
        AutorateMode::Always => true,
        AutorateMode::Busy => s.busy_until.is_some_and(|u| now < u),
        AutorateMode::Off => false,
    };

    if active && s.shaper.is_none() {
        start_shaping(s);
    } else if !active && s.shaper.is_some() {
        info!("Autorate: link idle, removing shaper from {}", s.iface);
        stop_shaping(s, settings);
    }
    update_pinger(s, settings, sample_tx);

    if let Some(shaper) = s.shaper.as_mut() {
        if s.ctrl.tick(dl_kbit, ul_kbit) {
            let down = s.ctrl.dl.enabled.then_some(s.ctrl.dl.rate_kbit as u32);
            debug!(
                "Autorate {}: down {:?} kbit, up {} kbit (delay {:.1?} ms)",
                s.iface,
                down,
                s.ctrl.ul.rate_kbit as u32,
                s.ctrl.delta_ms()
            );
            if let Err(e) = shaper.set_rates(s.ctrl.ul.rate_kbit as u32, down) {
                warn!("Autorate: rate change failed ({}), reinstalling shaper", e);
                s.shaper = None;
            }
            s.rates_learned |= s.ctrl.bloat_events > 0;
        }
        // NetworkManager or a reconnect can wipe qdiscs; check every 10s
        if s.ticks % 20 == 0 && s.shaper.as_ref().is_some_and(|sh| !sh.is_present()) {
            info!("Autorate: shaper on {} disappeared, reinstalling", s.iface);
            s.shaper = None;
        }
        if s.ticks % 60 == 0 && s.rates_learned {
            persist(s, settings);
        }
    } else {
        // Keep the controller's view of time moving so idle samples refresh baselines
        s.ctrl.tick(0.0, 0.0);
    }
    // Baselines are cheap to keep current; save them every 10 minutes
    if s.ticks % 1200 == 0 {
        persist(s, settings);
    }

    if s.ticks % 4 == 0 {
        write_status(s);
    }
}

fn start_shaping(s: &mut Session) {
    let down = s.ctrl.dl.enabled.then_some(s.ctrl.dl.rate_kbit as u32);
    match Shaper::install(&s.iface, s.ctrl.ul.rate_kbit as u32, down) {
        Ok(sh) => {
            s.ctrl.dl.enabled = sh.ifb.is_some();
            s.shaper = Some(sh);
        }
        Err(e) => warn!("Autorate: cannot install shaper on {}: {}", s.iface, e),
    }
}

/// Fast probing while shaping; slow probing while idle keeps baselines clean
/// (a baseline first measured under load would hide the bloat it is meant to detect).
fn update_pinger(s: &mut Session, settings: &Settings, sample_tx: &mpsc::Sender<Sample>) {
    let fast = s.shaper.is_some();
    let interval = if fast {
        settings.cfg.ping_interval_ms
    } else {
        settings.cfg.idle_ping_interval_ms
    };
    if interval == 0 {
        s.pinger = None;
        return;
    }
    if s.pinger.is_some() && s.pinger_fast == fast {
        return;
    }
    let p = Pinger::start(&settings.cfg.reflectors, Some(&s.iface), interval, sample_tx.clone());
    if p.is_empty() {
        warn!("Autorate: no latency probes running; the shaper will hold its rate");
    }
    s.pinger = Some(p);
    s.pinger_fast = fast;
}

fn stop_shaping(s: &mut Session, settings: &Settings) {
    persist(s, settings);
    s.shaper = None;
    shaper::remove(&s.iface);
}

fn teardown(mut s: Session, settings: &Settings) {
    s.pinger = None;
    if s.shaper.is_some() {
        stop_shaping(&mut s, settings);
    } else {
        persist(&mut s, settings);
    }
}

fn persist(s: &mut Session, settings: &Settings) {
    if !settings.cfg.remember_rates {
        return;
    }
    let Some(key) = &s.key else {
        return;
    };
    let previous = load_learned().remove(key).unwrap_or_default();
    let rates_kbit = if s.rates_learned {
        Some((s.ctrl.dl.rate_kbit as u32, s.ctrl.ul.rate_kbit as u32))
    } else {
        previous.rates_kbit
    };
    let baselines = s.ctrl.baselines();
    let baselines_ms = if baselines.iter().any(Option::is_some) {
        baselines
    } else {
        previous.baselines_ms
    };
    save_learned(key, Learned { rates_kbit, baselines_ms });
}

fn write_status(s: &Session) {
    let (_, live) = s.ctrl.bloat_votes();
    let status = Status {
        iface: s.iface.clone(),
        network: s.key.clone(),
        shaping: s.shaper.is_some(),
        shaper: s.shaper.as_ref().map(|sh| format!("{:?}", sh.kind)),
        download_kbit: s.ctrl.dl.enabled.then_some(s.ctrl.dl.rate_kbit as u32),
        upload_kbit: s.ctrl.ul.rate_kbit as u32,
        baseline_ms: s.ctrl.baseline_ms(),
        delay_ms: s.ctrl.delta_ms(),
        reflectors_responding: live,
        bloat_events: s.ctrl.bloat_events,
        updated_unix: now_unix(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&status) {
        let dir = Path::new(STATUS_PATH).parent().unwrap_or(Path::new("/run"));
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(STATUS_PATH, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctrl(dl: f64, ul: f64) -> Controller {
        Controller::new(
            15.0,
            Direction::new(dl, 5_000.0, 1_000_000.0),
            Direction::new(ul, 1_000.0, 1_000_000.0),
        )
    }

    /// Feed one tick of pings from 3 reflectors with the given extra delay.
    fn pings(c: &mut Controller, base: f64, extra: f64) {
        for r in 0..3 {
            c.on_sample(r, base + r as f64 + extra);
        }
    }

    #[test]
    fn no_bloat_no_cut() {
        let mut c = ctrl(500_000.0, 50_000.0);
        for _ in 0..20 {
            pings(&mut c, 20.0, 1.0);
            c.tick(90_000.0, 1_000.0);
        }
        assert_eq!(c.bloat_events, 0);
        assert_eq!(c.dl.rate_kbit, 500_000.0);
    }

    #[test]
    fn cuts_to_achieved_when_we_saturate_a_slower_bottleneck() {
        // Shaper starts at the 500 Mbit link rate, ISP delivers 100 Mbit and bloats.
        let mut c = ctrl(500_000.0, 50_000.0);
        pings(&mut c, 20.0, 0.0);
        c.tick(100_000.0, 2_000.0);
        for _ in 0..5 {
            pings(&mut c, 20.0, 80.0);
            c.tick(100_000.0, 2_000.0);
        }
        assert!(c.bloat_events >= 1);
        assert!(c.dl.rate_kbit <= 90_000.0 + 1.0, "dl={}", c.dl.rate_kbit);
        // Upload carried little traffic relative to its rate: must not be cut
        assert_eq!(c.ul.rate_kbit, 50_000.0);
    }

    #[test]
    fn other_devices_bloat_does_not_cut_us() {
        let mut c = ctrl(100_000.0, 20_000.0);
        // We downloaded at 90 Mbit earlier, so our peak is high
        for _ in 0..4 {
            pings(&mut c, 20.0, 0.0);
            c.tick(90_000.0, 1_000.0);
        }
        let rate = c.dl.rate_kbit;
        // Now someone else saturates the line; we only use 3 Mbit
        for _ in 0..10 {
            pings(&mut c, 20.0, 100.0);
            c.tick(3_000.0, 200.0);
        }
        assert_eq!(c.dl.rate_kbit, rate);
    }

    #[test]
    fn increases_when_saturated_and_clean() {
        // Traffic fills whatever the shaper allows (bottleneck is elsewhere and faster)
        let mut c = ctrl(50_000.0, 10_000.0);
        for _ in 0..10 {
            pings(&mut c, 20.0, 1.0);
            let (dl, ul) = (c.dl.rate_kbit, c.ul.rate_kbit);
            c.tick(dl, ul);
        }
        assert!(c.dl.rate_kbit > 70_000.0);
        assert!(c.ul.rate_kbit > 14_000.0);
    }

    #[test]
    fn single_bad_reflector_is_outvoted() {
        let mut c = ctrl(100_000.0, 20_000.0);
        for _ in 0..10 {
            c.on_sample(0, 20.0);
            c.on_sample(1, 21.0);
            c.on_sample(2, 22.0);
            c.tick(95_000.0, 1_000.0);
        }
        for _ in 0..10 {
            c.on_sample(0, 20.0);
            c.on_sample(1, 21.0);
            c.on_sample(2, 150.0); // one reflector deprioritizes ICMP
            c.tick(95_000.0, 1_000.0);
        }
        assert_eq!(c.bloat_events, 0);
    }

    #[test]
    fn stale_reflectors_stop_voting() {
        let mut c = ctrl(100_000.0, 20_000.0);
        pings(&mut c, 20.0, 0.0);
        for _ in 0..3 {
            pings(&mut c, 20.0, 90.0);
            c.tick(1.0, 1.0);
        }
        assert!(c.is_bloated());
        for _ in 0..(STALE_TICKS + 1) {
            c.tick(1.0, 1.0);
        }
        assert_eq!(c.bloat_votes(), (0, 0));
        assert!(!c.is_bloated());
    }

    #[test]
    fn baseline_does_not_learn_bloat() {
        let mut c = ctrl(100_000.0, 20_000.0);
        c.on_sample(0, 20.0);
        for _ in 0..1000 {
            c.on_sample(0, 120.0);
        }
        assert!(c.reflectors[0].baseline_ms.unwrap() < 21.0);
    }

    #[test]
    fn never_below_minimum() {
        let mut c = ctrl(6_000.0, 1_200.0);
        for _ in 0..50 {
            pings(&mut c, 20.0, 0.0);
            pings(&mut c, 20.0, 200.0);
            c.tick(12_000.0, 2_400.0);
        }
        assert!(c.dl.rate_kbit >= 5_000.0);
        assert!(c.ul.rate_kbit >= 1_000.0);
    }

    #[test]
    fn traffic_that_does_not_fill_the_shaper_does_not_grow_it() {
        let mut c = ctrl(50_000.0, 10_000.0);
        for _ in 0..20 {
            pings(&mut c, 20.0, 1.0);
            c.tick(50_000.0, 1_000.0);
        }
        assert!(c.dl.rate_kbit < 50_000.0 / SATURATED * INCREASE_FACTOR);
        assert_eq!(c.ul.rate_kbit, 10_000.0);
    }

    #[test]
    fn converges_near_bottleneck() {
        // Simulated link: 100 Mbit bottleneck that bloats when offered more than it can carry.
        let bottleneck = 100_000.0;
        let mut c = ctrl(500_000.0, 20_000.0);
        // Idle probing before the download starts establishes the baseline
        for _ in 0..10 {
            pings(&mut c, 20.0, 0.5);
            c.tick(0.0, 0.0);
        }
        let mut rates = Vec::new();
        for _ in 0..400 {
            let offered = c.dl.rate_kbit;
            let achieved = offered.min(bottleneck);
            let extra = if offered > bottleneck { 60.0 } else { 0.5 };
            pings(&mut c, 20.0, extra);
            c.tick(achieved, 1_000.0);
            rates.push(c.dl.rate_kbit);
        }
        let tail = &rates[200..];
        let avg = tail.iter().sum::<f64>() / tail.len() as f64;
        assert!((75_000.0..=105_000.0).contains(&avg), "avg={}", avg);
        // Mostly below the bottleneck (latency controlled), not stuck at the floor
        let above = tail.iter().filter(|r| **r > bottleneck).count();
        assert!(above * 4 < tail.len(), "above={} of {}", above, tail.len());
    }

    #[test]
    fn seeded_baseline_detects_bloat_present_from_the_start() {
        let mut c = ctrl(500_000.0, 20_000.0);
        c.seed_baselines(&[Some(20.0), Some(21.0), Some(22.0)]);
        for _ in 0..6 {
            pings(&mut c, 20.0, 60.0);
            c.tick(100_000.0, 500.0);
        }
        assert!(c.bloat_events >= 1);
        assert!(c.dl.rate_kbit <= 90_000.0 + 1.0);
    }

    #[test]
    fn unseeded_baseline_under_load_misses_bloat() {
        // Documents why idle probing and remembered baselines exist
        let mut c = ctrl(500_000.0, 20_000.0);
        for _ in 0..6 {
            pings(&mut c, 20.0, 60.0);
            c.tick(100_000.0, 500.0);
        }
        assert_eq!(c.bloat_events, 0);
    }

    #[test]
    fn disabled_direction_is_untouched() {
        let mut c = ctrl(500_000.0, 50_000.0);
        c.dl.enabled = false;
        pings(&mut c, 20.0, 0.0);
        for _ in 0..6 {
            pings(&mut c, 20.0, 80.0);
            c.tick(100_000.0, 45_000.0);
        }
        assert_eq!(c.dl.rate_kbit, 500_000.0);
        assert!(c.ul.rate_kbit < 50_000.0);
    }
}
