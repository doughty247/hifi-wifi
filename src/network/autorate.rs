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
/// v2: baselines learned only while idle. Rates learned by v1 could be far too low (a
/// baseline taken under load on lines that are faster when busy), so they are not reused.
const LEARNED_PATH: &str = "/var/lib/hifi-wifi/autorate-learned-v2.json";

const TICK: Duration = Duration::from_millis(500);
const ALPHA_DELTA: f64 = 0.3;
/// Idle samples are trustworthy, so the idle baseline can follow slow path changes
const ALPHA_BASE_UP: f64 = 0.01;
const ALPHA_BASE_DOWN: f64 = 0.9;
const STALE_TICKS: u64 = 6;
const DECREASE_FACTOR: f64 = 0.9;
/// Below the rate the last bloat episode settled on: recover quickly
const INCREASE_FACTOR: f64 = 1.08;
/// Above it: probe gently, since that is where the queue starts building
const PROBE_FACTOR: f64 = 1.03;
const SATURATED: f64 = 0.75;
const BUSY_VS_PEAK: f64 = 0.5;
const PEAK_DECAY: f64 = 0.995;
const DECREASE_REFRACTORY_TICKS: u64 = 4;
/// A deep upstream buffer takes seconds to drain after a cut. While delay is falling the cut
/// is working, so only cut again if it is not, or after this long.
const DRAIN_WAIT_TICKS: u64 = 12;
/// Cuts closer together than this belong to the same bloat episode
const EPISODE_TICKS: u64 = 20;
/// A cut "helped" if smoothed delay later fell below this fraction of its value at the cut
const HELPED_RATIO: f64 = 0.7;
/// After a cut that did not help, ignore bloat for this long (60 s) and restore the rate:
/// the delay is not coming from a queue we control (e.g. Wi-Fi contention or retries)
const FUTILE_HOLD_TICKS: u64 = 120;
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
    /// Rate set by the first cut of the last bloat episode: just under the bottleneck
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
    /// Smoothed delay over baseline (bloat vote)
    delta_ms: f64,
    /// Latest raw delay over baseline (queue trend, no smoothing lag)
    raw_delta_ms: f64,
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
    /// Whether the last tick saw an idle link (baseline samples are taken only then)
    link_idle: bool,
    last_decrease_tick: Option<u64>,
    /// Highest median delay since the last cut, to tell a draining queue from a growing one
    delay_peak_since_cut_ms: f64,
    /// Smoothed delay at the last cut, and the lowest since: did the cut help?
    delay_at_cut_ms: f64,
    delay_min_since_cut_ms: f64,
    /// Rates before the first cut of the current episode, restored if cutting proves futile
    episode_start_kbit: Option<(f64, f64)>,
    /// Bloat is ignored until this tick (cutting did not reduce it)
    futile_until_tick: u64,
    pub futile_episodes: u64,
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
            link_idle: false,
            last_decrease_tick: None,
            delay_peak_since_cut_ms: 0.0,
            delay_at_cut_ms: 0.0,
            delay_min_since_cut_ms: 0.0,
            episode_start_kbit: None,
            futile_until_tick: 0,
            futile_episodes: 0,
            bloat_events: 0,
        }
    }

    /// Bloat is latency under load compared with latency at idle, so the baseline is learned
    /// only while the link is idle. Some lines (DOCSIS cable, for one) answer *faster* while
    /// busy; a baseline taken then makes every normal idle-like RTT look like bloat.
    pub fn on_sample(&mut self, reflector: usize, rtt_ms: f64) {
        if self.reflectors.len() <= reflector {
            self.reflectors.resize(reflector + 1, Reflector::default());
        }
        let threshold = self.threshold_ms;
        let idle = self.link_idle;
        let r = &mut self.reflectors[reflector];
        r.last_tick = self.tick;
        let Some(base) = r.baseline_ms else {
            if idle {
                r.baseline_ms = Some(rtt_ms);
            }
            return;
        };
        let base = if !idle {
            base
        } else if rtt_ms < base {
            ALPHA_BASE_DOWN * rtt_ms + (1.0 - ALPHA_BASE_DOWN) * base
        } else if rtt_ms - base < threshold {
            // Follow slow path changes, but never learn a spike as the new normal
            ALPHA_BASE_UP * rtt_ms + (1.0 - ALPHA_BASE_UP) * base
        } else {
            base
        };
        r.baseline_ms = Some(base);
        r.raw_delta_ms = rtt_ms - base;
        r.delta_ms = ALPHA_DELTA * r.raw_delta_ms + (1.0 - ALPHA_DELTA) * r.delta_ms;
    }

    fn live(&self) -> impl Iterator<Item = &Reflector> {
        let tick = self.tick;
        self.reflectors.iter().filter(move |r| {
            r.baseline_ms.is_some() && tick.saturating_sub(r.last_tick) <= STALE_TICKS
        })
    }

    /// (bloated reflectors, responsive reflectors)
    pub fn bloat_votes(&self) -> (usize, usize) {
        let live: Vec<_> = self.live().collect();
        let bloated = live
            .iter()
            .filter(|r| r.delta_ms > self.threshold_ms)
            .count();
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
                ..Default::default()
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
        // Applies to the RTT samples that arrive until the next tick
        self.link_idle = dl_kbit < BUSY_DL_KBIT && ul_kbit < BUSY_UL_KBIT;
        let before = (self.dl.rate_kbit, self.ul.rate_kbit);
        let since_decrease = self
            .last_decrease_tick
            .map(|t| self.tick - t)
            .unwrap_or(u64::MAX);
        let smooth = self.delta_ms().unwrap_or(0.0);
        self.delay_min_since_cut_ms = self.delay_min_since_cut_ms.min(smooth);
        let in_episode = self.last_decrease_tick.is_some() && since_decrease <= EPISODE_TICKS;
        let helped = self.delay_min_since_cut_ms < self.delay_at_cut_ms * HELPED_RATIO;

        // A cut that had time to act but left the delay where it was: the delay is not in a
        // queue we control. Stop cutting for a while and give the bandwidth back.
        if in_episode
            && since_decrease >= DRAIN_WAIT_TICKS
            && !helped
            && self.tick >= self.futile_until_tick
            && self.is_bloated()
        {
            self.futile_until_tick = self.tick + FUTILE_HOLD_TICKS;
            self.futile_episodes += 1;
            if let Some((dl, ul)) = self.episode_start_kbit.take() {
                self.dl.rate_kbit = self.dl.clamp(self.dl.rate_kbit.max(dl));
                self.ul.rate_kbit = self.ul.clamp(self.ul.rate_kbit.max(ul));
            }
        }
        let bloated = self.is_bloated() && self.tick >= self.futile_until_tick;

        for (dir, achieved) in [(&mut self.dl, dl_kbit), (&mut self.ul, ul_kbit)] {
            dir.peak_kbit = achieved.max(dir.peak_kbit * PEAK_DECAY);
        }

        let delay = median(self.live().map(|r| r.raw_delta_ms).collect()).unwrap_or(0.0);
        self.delay_peak_since_cut_ms = self.delay_peak_since_cut_ms.max(delay);
        let draining = delay < self.delay_peak_since_cut_ms * 0.9;
        let new_episode = since_decrease > EPISODE_TICKS;

        let mut cut = false;
        if bloated {
            // Within an episode, cut again only if the last cut demonstrably reduced delay
            // (the queue is ours) and the delay is not already falling.
            if since_decrease >= DECREASE_REFRACTORY_TICKS
                && (new_episode || (helped && !draining))
            {
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
                        if new_episode || dir.bottleneck_kbit.is_none() {
                            dir.bottleneck_kbit = Some(dir.rate_kbit);
                        }
                        cut = true;
                    }
                }
            }
        } else if since_decrease >= INCREASE_REFRACTORY_TICKS {
            for (dir, achieved) in [(&mut self.dl, dl_kbit), (&mut self.ul, ul_kbit)] {
                if dir.enabled && achieved / dir.rate_kbit > SATURATED {
                    let below_bottleneck = dir.bottleneck_kbit.is_none_or(|b| dir.rate_kbit < b);
                    let factor = if below_bottleneck {
                        INCREASE_FACTOR
                    } else {
                        PROBE_FACTOR
                    };
                    dir.rate_kbit = dir.clamp(dir.rate_kbit * factor);
                }
            }
        }
        if cut {
            if new_episode {
                self.episode_start_kbit = Some(before);
            }
            self.last_decrease_tick = Some(self.tick);
            self.delay_peak_since_cut_ms = delay;
            self.delay_at_cut_ms = smooth;
            self.delay_min_since_cut_ms = smooth;
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

/// Current status, if a live autorate wrote it recently (a crashed daemon leaves a stale file)
pub fn read_status() -> Option<Status> {
    let st: Status = serde_json::from_str(&std::fs::read_to_string(STATUS_PATH).ok()?).ok()?;
    (now_unix().saturating_sub(st.updated_unix) <= 30).then_some(st)
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
    pinger_started_at: Option<Instant>,
    last_sample_at: Option<Instant>,
    /// Back off after a failed shaper install instead of retrying every tick
    retry_install_at: Option<Instant>,
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
                info!(
                    "Autorate: using remembered rates for this network ({} / {} kbit)",
                    d, u
                );
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
            pinger_started_at: None,
            last_sample_at: None,
            retry_install_at: None,
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
                    s.last_sample_at = Some(Instant::now());
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
    let (Some(rx), Some(tx)) = (
        read_counter(&s.iface, "rx_bytes"),
        read_counter(&s.iface, "tx_bytes"),
    ) else {
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
        start_shaping(s, settings);
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
        if s.ticks.is_multiple_of(20) && s.shaper.as_ref().is_some_and(|sh| !sh.is_present()) {
            info!("Autorate: shaper on {} disappeared, reinstalling", s.iface);
            s.shaper = None;
        }
        if s.ticks.is_multiple_of(60) && s.rates_learned {
            persist(s, settings);
        }
    } else {
        // Keep time and the idle/busy state current so idle samples refresh baselines
        s.ctrl.tick(dl_kbit, ul_kbit);
    }
    // Baselines are cheap to keep current; save them every 10 minutes
    if s.ticks.is_multiple_of(1200) {
        persist(s, settings);
    }

    if s.ticks.is_multiple_of(4) {
        write_status(s);
    }
}

const INSTALL_RETRY: Duration = Duration::from_secs(30);
const PROBE_WATCHDOG: Duration = Duration::from_secs(20);

fn start_shaping(s: &mut Session, settings: &Settings) {
    let now = Instant::now();
    if s.retry_install_at.is_some_and(|t| now < t) {
        return;
    }
    // Retry download shaping on every install; IFB may have failed only transiently
    let down = settings
        .shape_download
        .then_some(s.ctrl.dl.rate_kbit as u32);
    match Shaper::install(&s.iface, s.ctrl.ul.rate_kbit as u32, down) {
        Ok(sh) => {
            s.ctrl.dl.enabled = sh.ifb.is_some();
            s.shaper = Some(sh);
            s.retry_install_at = None;
        }
        Err(e) => {
            warn!(
                "Autorate: cannot install shaper on {} ({}); retrying in {} s",
                s.iface,
                e,
                INSTALL_RETRY.as_secs()
            );
            s.retry_install_at = Some(now + INSTALL_RETRY);
        }
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
    let now = Instant::now();
    // ping exits on its own if the network was unreachable when it started; restart it
    let silent = s
        .pinger_started_at
        .is_some_and(|t| now.duration_since(t) > PROBE_WATCHDOG)
        && s.last_sample_at
            .is_none_or(|t| now.duration_since(t) > PROBE_WATCHDOG);
    if s.pinger.is_some() && s.pinger_fast == fast && !silent {
        return;
    }
    if silent {
        debug!(
            "Autorate: no latency samples for {} s, restarting probes",
            PROBE_WATCHDOG.as_secs()
        );
    }
    let p = Pinger::start(
        &settings.cfg.reflectors,
        Some(&s.iface),
        interval,
        sample_tx.clone(),
    );
    if p.is_empty() {
        warn!("Autorate: no latency probes running; the shaper will hold its rate");
    }
    s.pinger = Some(p);
    s.pinger_fast = fast;
    s.pinger_started_at = Some(now);
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
    save_learned(
        key,
        Learned {
            rates_kbit,
            baselines_ms,
        },
    );
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

    /// An idle period with clean pings: how a real session learns its baseline
    fn idle_baseline(c: &mut Controller, base: f64) {
        for _ in 0..3 {
            c.tick(0.0, 0.0);
            pings(c, base, 0.0);
        }
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
        idle_baseline(&mut c, 20.0);
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
        idle_baseline(&mut c, 20.0);
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
        c.tick(0.0, 0.0);
        c.on_sample(0, 20.0);
        for _ in 0..1000 {
            c.on_sample(0, 120.0); // idle spikes are not learned
        }
        assert!(c.reflectors[0].baseline_ms.unwrap() < 21.0);
        c.tick(90_000.0, 0.0);
        for _ in 0..1000 {
            c.on_sample(0, 5.0); // nor is anything seen under load, even lower RTTs
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
    fn draining_queue_is_not_cut_again() {
        let mut c = ctrl(500_000.0, 20_000.0);
        c.seed_baselines(&[Some(20.0), Some(21.0), Some(22.0)]);
        // Deep buffer: 300 ms of extra delay, draining 40 ms per tick after the cut
        let mut extra = 300.0;
        for _ in 0..4 {
            pings(&mut c, 20.0, extra);
            c.tick(20_000.0, 500.0);
        }
        assert_eq!(c.bloat_events, 1);
        let after_first = c.dl.rate_kbit;
        for _ in 0..8 {
            extra = (extra - 40.0f64).max(0.5);
            pings(&mut c, 20.0, extra);
            c.tick(after_first, 500.0);
        }
        assert_eq!(c.bloat_events, 1, "cut again while the queue was draining");
    }

    /// Real-hardware regression (Bazzite desktop, cable-like line): idle RTT ~18 ms, *lower*
    /// (~11 ms) while uploading, ~24 ms with jitter while downloading 245 Mbit. v1 learned the
    /// 11 ms baseline during upload and cut the download from 245 to 39 Mbit.
    #[test]
    fn line_faster_when_busy_is_not_cut() {
        let mut c = ctrl(1_000_000.0, 1_000_000.0);
        idle_baseline(&mut c, 18.0);
        // Upload saturated: RTT drops to 11 ms
        for _ in 0..30 {
            c.tick(4_500.0, 191_000.0);
            pings(&mut c, 11.0, 0.0);
        }
        // Download saturated: ~24 ms with jitter spikes to ~40
        for i in 0..60 {
            c.tick(245_000.0, 1_500.0);
            let jitter = if i % 5 == 0 { 18.0 } else { 4.0 };
            pings(&mut c, 18.0, jitter);
        }
        assert_eq!(c.bloat_events, 0, "dl={} ul={}", c.dl.rate_kbit, c.ul.rate_kbit);
        assert!(c.dl.rate_kbit >= 245_000.0);
    }

    /// Real-hardware regression (same desktop on Wi-Fi): download latency jitters 25-150 ms
    /// whatever the rate, so cutting cannot help. v2 kept cutting every ~2 s, 151 -> 38 Mbit.
    #[test]
    fn delay_that_cutting_does_not_reduce_is_left_alone() {
        let mut c = ctrl(1_300_000.0, 1_300_000.0);
        idle_baseline(&mut c, 18.0);
        let mut min_rate = f64::MAX;
        for i in 0..120 {
            // Wi-Fi: up to ~151 Mbit gets through; delay +10..+40 ms regardless of our rate
            let achieved = c.dl.rate_kbit.min(151_000.0);
            c.tick(achieved, 1_500.0);
            let extra = 10.0 + (i % 4) as f64 * 10.0;
            pings(&mut c, 18.0, extra);
            min_rate = min_rate.min(c.dl.rate_kbit);
        }
        assert!(c.futile_episodes >= 1);
        assert!(min_rate > 120_000.0, "cut too deep: {}", min_rate);
        assert!(c.dl.rate_kbit >= 135_000.0, "not restored: {}", c.dl.rate_kbit);
    }

    #[test]
    fn disabled_direction_is_untouched() {
        let mut c = ctrl(500_000.0, 50_000.0);
        c.dl.enabled = false;
        idle_baseline(&mut c, 20.0);
        for _ in 0..6 {
            pings(&mut c, 20.0, 80.0);
            c.tick(100_000.0, 45_000.0);
        }
        assert_eq!(c.dl.rate_kbit, 500_000.0);
        assert!(c.ul.rate_kbit < 50_000.0);
    }
}
