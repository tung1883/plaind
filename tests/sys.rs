use plaind::procs::Procs;
use rmpv::Value;
use std::{thread, time::Duration};

fn cpu(v: &Value) -> f64 {
    if let Value::Map(p) = v {
        for (k, val) in p {
            if k.as_str() == Some("cpu") { return val.as_f64().unwrap_or(-1.0); }
        }
    }
    -1.0
}

#[test]
fn sys_cpu_is_sane() {
    let mut procs = Procs::new();
    let mut last = -1.0;
    for _ in 0..4 {
        thread::sleep(Duration::from_millis(400));
        let (_list, sys) = procs.snapshot();
        last = cpu(&sys);
        assert!((0.0..=100.0).contains(&last), "cpu out of range: {last}");
    }
    // a normally-idle-to-moderate CI box should not be pinned at exactly 100
    assert!(last < 99.9, "cpu stuck at {last}% (bad refresh interval?)");
}

#[test]
fn processes_carry_a_user() {
    let mut procs = Procs::new();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let (list, _sys) = procs.snapshot();
    let mut sample = vec![];
    let mut with_user = 0;
    for v in &list {
        if let Value::Map(p) = v {
            let mut name = ""; let mut user = "";
            for (k, val) in p {
                match k.as_str() {
                    Some("name") => name = val.as_str().unwrap_or(""),
                    Some("user") => user = val.as_str().unwrap_or(""),
                    _ => {}
                }
            }
            if !user.is_empty() { with_user += 1; }
            if sample.len() < 6 { sample.push(format!("{name} -> [{user}]")); }
        }
    }
    eprintln!("sample: {sample:#?}");
    assert!(with_user > 0, "no process had a resolved user");
}
