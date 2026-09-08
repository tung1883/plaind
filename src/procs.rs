//! The `proc` channel — a process snapshot and signalling, via `sysinfo`.

use rmpv::Value;
use sysinfo::{Pid, ProcessesToUpdate, Signal, System};

use crate::proto::{map, s};

pub struct Procs {
    system: System,
}

impl Procs {
    pub fn new() -> Self {
        Procs {
            system: System::new(),
        }
    }

    pub fn snapshot(&mut self) -> Vec<Value> {
        self.system
            .refresh_processes(ProcessesToUpdate::All, true);
        let mut out: Vec<Value> = self
            .system
            .processes()
            .values()
            .map(|p| {
                map(vec![
                    ("pid", Value::from(p.pid().as_u32() as i64)),
                    ("name", s(&p.name().to_string_lossy())),
                    ("cpu", Value::from(f64::from(p.cpu_usage()))),
                    ("mem_kb", Value::from((p.memory() / 1024) as i64)),
                    (
                        "user",
                        s(p.user_id()
                            .map(|u| u.to_string())
                            .unwrap_or_default()
                            .as_str()),
                    ),
                ])
            })
            .collect();
        out.sort_by(|a, b| {
            let ca = field_f64(a, "cpu");
            let cb = field_f64(b, "cpu");
            cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal)
        });
        out.truncate(300);
        out
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
