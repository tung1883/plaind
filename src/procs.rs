//! The `proc` channel — a process snapshot and signalling, via `sysinfo`.

use std::time::{Duration, Instant};

use rmpv::Value;
use sysinfo::{
    Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, Signal, System, UpdateKind, Users,
};

use crate::proto::{map, s};

/// `global_cpu_usage()` measures the interval since the last CPU refresh; if two
/// `proc.list`s land within this window we keep the previous reading rather than
/// refresh over a few-ms interval and get a garbage 0%/100% spike.
const CPU_MIN_INTERVAL: Duration = Duration::from_millis(400);

pub struct Procs {
    system: System,
    users: Users,
    ncpu: f64,
    last_cpu_at: Instant,
    last_cpu: f64,
}

impl Procs {
    pub fn new() -> Self {
        let mut system = System::new();
        system.refresh_cpu_usage();
        let ncpu = (system.cpus().len().max(1)) as f64;
        Procs {
            system,
            users: Users::new_with_refreshed_list(),
            ncpu,
            last_cpu_at: Instant::now(),
            last_cpu: 0.0,
        }
    }

    /// The process list (sorted by CPU desc, capped at 300) plus a `sys` summary.
    pub fn snapshot(&mut self) -> (Vec<Value>, Value) {
        // Only sample CPU when enough time has passed since the last refresh.
        if self.last_cpu_at.elapsed() >= CPU_MIN_INTERVAL {
            self.last_cpu = f64::from(self.system.global_cpu_usage());
            self.system.refresh_cpu_usage();
            self.last_cpu_at = Instant::now();
        }
        let cpu = self.last_cpu;
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .with_user(UpdateKind::OnlyIfNotSet),
        );
        self.system.refresh_memory();

        let (mut run, mut sleeping, mut stopped, mut zombie, mut other) = (0i64, 0i64, 0i64, 0i64, 0i64);
        let mut out: Vec<Value> = Vec::new();
        for p in self.system.processes().values() {
            let st = p.status();
            match st {
                ProcessStatus::Run => run += 1,
                ProcessStatus::Sleep | ProcessStatus::Idle => sleeping += 1,
                ProcessStatus::Stop => stopped += 1,
                ProcessStatus::Zombie => zombie += 1,
                _ => other += 1,
            }
            out.push(map(vec![
                ("pid", Value::from(p.pid().as_u32() as i64)),
                ("name", s(&p.name().to_string_lossy())),
                // sysinfo gives 100% == one core; divide by core count so it
                // shares the 0..100 scale with the global figure (Task Manager style).
                ("cpu", Value::from(f64::from(p.cpu_usage()) / self.ncpu)),
                ("mem_kb", Value::from((p.memory() / 1024) as i64)),
                ("state", s(status_char(st))),
                (
                    "user",
                    s(p.user_id()
                        .and_then(|uid| self.users.get_user_by_id(uid))
                        .map(|u| u.name().to_string())
                        .or_else(|| p.user_id().map(|u| u.to_string()))
                        .unwrap_or_default()
                        .as_str()),
                ),
            ]));
        }
        out.sort_by(|a, b| {
            field_f64(b, "cpu")
                .partial_cmp(&field_f64(a, "cpu"))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let total = out.len() as i64;
        out.truncate(300);

        let la = System::load_average();
        let sys = map(vec![
            ("cpu", Value::from(cpu)),
            ("cpu_count", Value::from(self.ncpu as i64)),
            ("mem_used_kb", Value::from((self.system.used_memory() / 1024) as i64)),
            ("mem_total_kb", Value::from((self.system.total_memory() / 1024) as i64)),
            ("swap_used_kb", Value::from((self.system.used_swap() / 1024) as i64)),
            ("swap_total_kb", Value::from((self.system.total_swap() / 1024) as i64)),
            (
                "load",
                Value::Array(vec![
                    Value::from(la.one),
                    Value::from(la.five),
                    Value::from(la.fifteen),
                ]),
            ),
            ("uptime_s", Value::from(System::uptime() as i64)),
            (
                "tasks",
                map(vec![
                    ("total", Value::from(total)),
                    ("running", Value::from(run)),
                    ("sleeping", Value::from(sleeping)),
                    ("stopped", Value::from(stopped)),
                    ("zombie", Value::from(zombie)),
                    ("other", Value::from(other)),
                ]),
            ),
        ]);
        (out, sys)
    }

    pub fn kill(&mut self, pid: i64, sig: &str) -> bool {
        let signal = match sig {
            "KILL" => Signal::Kill,
            _ => Signal::Term,
        };
        match self.system.process(Pid::from_u32(pid as u32)) {
            Some(p) => p.kill_with(signal).unwrap_or_else(|| p.kill()),
            None => false,
        }
    }
}

fn status_char(st: ProcessStatus) -> &'static str {
    match st {
        ProcessStatus::Run => "R",
        ProcessStatus::Sleep | ProcessStatus::Idle => "S",
        ProcessStatus::Stop => "T",
        ProcessStatus::Zombie => "Z",
        _ => "?",
    }
}

fn field_f64(v: &Value, key: &str) -> f64 {
    if let Value::Map(pairs) = v {
        for (k, val) in pairs {
            if k.as_str() == Some(key) {
                return val.as_f64().unwrap_or(0.0);
            }
        }
    }
    0.0
}
