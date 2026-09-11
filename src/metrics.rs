//! The `stats` / `net` / `disk` channels — device metrics via `sysinfo`, plus
//! listening ports via `netstat2`. All poll-response (like `proc.list`): the
//! phone asks on a timer and pauses when its panel is hidden.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use rmpv::Value;
use sysinfo::{DiskKind, Disks, Networks, System};

use crate::proto::{map, s};

/// Samples kept in the CPU / memory history ring buffers (sent in full on every
/// `stats.get` so the phone can draw its chart straight after a reconnect).
const HIST_LEN: usize = 120;
const CPU_MIN_INTERVAL: Duration = Duration::from_millis(400);

pub struct Metrics {
    system: System,
    networks: Networks,
    disks: Disks,
    ncpu: f64,
    last_cpu_at: Instant,
    last_cpu: f64,
    last_net_at: Instant,
    cpu_hist: VecDeque<f32>,
    mem_hist: VecDeque<f32>,
}

impl Metrics {
    pub fn new() -> Self {
        let mut system = System::new();
        system.refresh_cpu_usage();
        system.refresh_memory();
        let ncpu = system.cpus().len().max(1) as f64;
        Metrics {
            system,
            networks: Networks::new_with_refreshed_list(),
            disks: Disks::new_with_refreshed_list(),
            ncpu,
            last_cpu_at: Instant::now(),
            last_cpu: 0.0,
            last_net_at: Instant::now(),
            cpu_hist: VecDeque::with_capacity(HIST_LEN),
            mem_hist: VecDeque::with_capacity(HIST_LEN),
        }
    }

    pub fn stats_snapshot(&mut self) -> Value {
        if self.last_cpu_at.elapsed() >= CPU_MIN_INTERVAL {
            self.last_cpu = f64::from(self.system.global_cpu_usage());
            self.system.refresh_cpu_usage();
            self.last_cpu_at = Instant::now();
        }
        self.system.refresh_memory();

        let cpu = self.last_cpu;
        let mem_used = self.system.used_memory();
        let mem_total = self.system.total_memory().max(1);
        let mem_pct = (mem_used as f64 / mem_total as f64) * 100.0;
        push_cap(&mut self.cpu_hist, cpu as f32);
        push_cap(&mut self.mem_hist, mem_pct as f32);

        let per_cpu: Vec<Value> = self
            .system
            .cpus()
            .iter()
            .map(|c| Value::from(f64::from(c.cpu_usage())))
            .collect();

        let la = System::load_average();

        map(vec![
            ("cpu", Value::from(cpu)),
            ("cpu_count", Value::from(self.ncpu as i64)),
            ("per_cpu", Value::Array(per_cpu)),
            ("mem_used_kb", Value::from((mem_used / 1024) as i64)),
            ("mem_total_kb", Value::from((mem_total / 1024) as i64)),
            ("swap_used_kb", Value::from((self.system.used_swap() / 1024) as i64)),
            ("swap_total_kb", Value::from((self.system.total_swap() / 1024) as i64)),
            ("cpu_hist", f32_array(&self.cpu_hist)),
            ("mem_hist", f32_array(&self.mem_hist)),
            (
                "load",
                Value::Array(vec![
                    Value::from(la.one),
                    Value::from(la.five),
                    Value::from(la.fifteen),
                ]),
            ),
            ("uptime_s", Value::from(System::uptime() as i64)),
            ("boot_s", Value::from(System::boot_time() as i64)),
        ])
    }

    pub fn net_snapshot(&mut self) -> Value {
        self.networks.refresh(true);
        let secs = self.last_net_at.elapsed().as_secs_f64().max(0.001);
        self.last_net_at = Instant::now();

        let mut total_rx = 0u64;
        let mut total_tx = 0u64;
        let ifaces: Vec<Value> = self
            .networks
            .iter()
            .map(|(name, d)| {
                total_rx += d.total_received();
                total_tx += d.total_transmitted();
                let addrs: Vec<Value> = d
                    .ip_networks()
                    .iter()
                    .map(|n| s(&format!("{}/{}", n.addr, n.prefix)))
                    .collect();
                map(vec![
                    ("name", s(name)),
                    ("rx_bps", Value::from(d.received() as f64 / secs)),
                    ("tx_bps", Value::from(d.transmitted() as f64 / secs)),
                    ("rx_total", Value::from(d.total_received() as i64)),
                    ("tx_total", Value::from(d.total_transmitted() as i64)),
                    ("mac", s(&d.mac_address().to_string())),
                    ("mtu", Value::from(d.mtu() as i64)),
                    ("addrs", Value::Array(addrs)),
                ])
            })
            .collect();

        map(vec![
            ("ifaces", Value::Array(ifaces)),
            ("rx_total", Value::from(total_rx as i64)),
            ("tx_total", Value::from(total_tx as i64)),
            ("ports", listening_ports()),
        ])
    }

    pub fn disk_snapshot(&mut self) -> Value {
        self.disks.refresh(true);
        let disks: Vec<Value> = self
            .disks
            .iter()
            .map(|d| {
                let total = d.total_space();
                let avail = d.available_space();
                let u = d.usage();
                map(vec![
                    ("mount", s(&d.mount_point().to_string_lossy())),
                    ("name", s(&d.name().to_string_lossy())),
                    ("fs", s(&d.file_system().to_string_lossy())),
                    ("kind", s(match d.kind() {
                        DiskKind::HDD => "HDD",
                        DiskKind::SSD => "SSD",
                        // sysinfo couldn't classify it; {:?} would print the
                        // raw "Unknown(-1)" payload, which isn't an error.
                        DiskKind::Unknown(_) => "Unknown",
                    })),
                    ("total", Value::from(total as i64)),
                    ("avail", Value::from(avail as i64)),
                    ("used", Value::from(total.saturating_sub(avail) as i64)),
                    ("read_bps", Value::from(u.read_bytes as i64)),
                    ("write_bps", Value::from(u.written_bytes as i64)),
                    ("read_total", Value::from(u.total_read_bytes as i64)),
                    ("write_total", Value::from(u.total_written_bytes as i64)),
                ])
            })
            .collect();
        map(vec![("disks", Value::Array(disks))])
    }
}

fn listening_ports() -> Value {
    use netstat2::{
        get_sockets_info, AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo, TcpState,
    };
    let mut out: Vec<Value> = Vec::new();
    let flags = AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6;
    let proto = ProtocolFlags::TCP | ProtocolFlags::UDP;
    if let Ok(socks) = get_sockets_info(flags, proto) {
        for si in socks {
            let pids: Vec<Value> = si
                .associated_pids
                .iter()
                .map(|p| Value::from(*p as i64))
                .collect();
            match si.protocol_socket_info {
                ProtocolSocketInfo::Tcp(t) if t.state == TcpState::Listen => {
                    out.push(map(vec![
                        ("proto", s("tcp")),
                        ("addr", s(&t.local_addr.to_string())),
                        ("port", Value::from(t.local_port as i64)),
                        ("pids", Value::Array(pids)),
                    ]));
                }
                ProtocolSocketInfo::Udp(u) => {
                    out.push(map(vec![
                        ("proto", s("udp")),
                        ("addr", s(&u.local_addr.to_string())),
                        ("port", Value::from(u.local_port as i64)),
                        ("pids", Value::Array(pids)),
                    ]));
                }
                _ => {}
            }
        }
    }
    Value::Array(out)
}

fn push_cap(q: &mut VecDeque<f32>, v: f32) {
    if q.len() == HIST_LEN {
        q.pop_front();
    }
    q.push_back(v);
}

fn f32_array(q: &VecDeque<f32>) -> Value {
    Value::Array(q.iter().map(|v| Value::from(*v as f64)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(v: &Value) -> Vec<String> {
        match v {
            Value::Map(m) => m
                .iter()
                .filter_map(|(k, _)| k.as_str().map(String::from))
                .collect(),
            _ => vec![],
        }
    }

    #[test]
    fn snapshots_have_the_documented_shape() {
        let mut m = Metrics::new();

        let stats = m.stats_snapshot();
        for k in ["cpu", "cpu_count", "per_cpu", "mem_total_kb", "cpu_hist", "load", "uptime_s"] {
            assert!(keys(&stats).contains(&k.to_string()), "stats missing {k}");
        }

        let net = m.net_snapshot();
        for k in ["ifaces", "rx_total", "tx_total", "ports"] {
            assert!(keys(&net).contains(&k.to_string()), "net missing {k}");
        }

        let disk = m.disk_snapshot();
        assert!(keys(&disk).contains(&"disks".to_string()));
    }
}
