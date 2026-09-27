//! Latency counters. Each named metric collects samples (ms, or KB for sizes);
//! every 2 s the non-empty ones are summarised to the log as
//! `n, avg, p50, p95, max` under a `[lat]` prefix.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(2);

struct State {
    series: BTreeMap<&'static str, Vec<f64>>,
    since: Instant,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State { series: BTreeMap::new(), since: Instant::now() }))
}

pub fn record(name: &'static str, value: f64) {
    let mut st = state().lock().unwrap();
    st.series.entry(name).or_default().push(value);
    if st.since.elapsed() < WINDOW {
        return;
    }
    let secs = st.since.elapsed().as_secs_f64();
    st.since = Instant::now();
    let mut out = format!("[lat] --- last {secs:.1}s");
    for (name, v) in st.series.iter_mut() {
        if v.is_empty() {
            continue;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = v.len();
        let avg = v.iter().sum::<f64>() / n as f64;
        out.push_str(&format!(
            "\n[lat] {name:<20} n={n:<4} ({:.1}/s) avg={avg:<7.1} p50={:<7.1} p95={:<7.1} max={:.1}",
            n as f64 / secs,
            v[n / 2],
            v[((n as f64 * 0.95) as usize).min(n - 1)],
            v[n - 1]
        ));
        v.clear();
    }
    drop(st);
    crate::plog!("{out}");
}

pub fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
