//! Resource safety for the local model: decide whether a naming run may start (free memory, memory pressure,
//! battery) and whether a running one must be killed. The decisions are pure functions over the text of
//! /proc/meminfo, /proc/pressure/memory and /sys/class/power_supply, so they can be tested.

use std::path::{Path, PathBuf};
use std::time::Duration;

const MIB: u64 = 1 << 20;
/// Memory pressure (`some avg10`, percent of time stalled) above which no run starts.
pub const PRESSURE_MAX: f64 = 5.0;
/// Extra free memory demanded on top of the estimate.
pub const MARGIN: u64 = 1024 * MIB;
/// A namer batch running longer than this is killed.
pub const WALL_MAX: Duration = Duration::from_secs(180);

/// `(MemTotal, MemAvailable)` in bytes.
pub fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let field = |name: &str| -> Option<u64> {
        let line = text.lines().find(|l| l.starts_with(name))?;
        let kb: u64 = line[name.len()..].trim_start_matches(':').split_whitespace().next()?.parse().ok()?;
        Some(kb * 1024)
    };
    Some((field("MemTotal")?, field("MemAvailable")?))
}

/// `some avg10` of /proc/pressure/memory.
pub fn parse_pressure(text: &str) -> Option<f64> {
    let line = text.lines().find(|l| l.starts_with("some "))?;
    line.split_whitespace().find_map(|w| w.strip_prefix("avg10="))?.parse().ok()
}

/// Memory the local model needs: weights (mmap'd, but resident while in use) plus context, compute buffers
/// and the process itself.
pub fn estimate(model_bytes: u64) -> u64 {
    model_bytes + model_bytes * 3 / 10 + 300 * MIB
}

/// `(type, online)` file contents of every power supply -> true if on battery: there is a mains supply
/// and none of them is online. Machines without a mains supply entry (desktops, VMs) are never "on battery".
pub fn on_battery(supplies: &[(String, String)]) -> bool {
    let mains: Vec<&(String, String)> = supplies.iter().filter(|(t, _)| t.trim() == "Mains").collect();
    !mains.is_empty() && mains.iter().all(|(_, online)| online.trim() == "0")
}

/// May a run start? `Err` has the reason (for the log).
pub fn may_start(
    meminfo: &str,
    pressure: Option<&str>,
    model_bytes: u64,
    battery: bool,
    allow_battery: bool,
) -> Result<(), String> {
    if battery && !allow_battery {
        return Err("on battery (llm_on_battery = false)".into());
    }
    let (_, avail) = parse_meminfo(meminfo).ok_or("cannot read /proc/meminfo")?;
    let need = estimate(model_bytes) + MARGIN;
    if avail < need {
        return Err(format!("not enough free memory ({} MiB available, {} MiB needed)", avail / MIB, need / MIB));
    }
    if let Some(p) = pressure.and_then(parse_pressure)
        && p >= PRESSURE_MAX
    {
        return Err(format!("memory pressure (avg10 {p:.1})"));
    }
    Ok(())
}

/// Watchdog decision for a running namer: `Some(reason)` = kill it now.
pub fn must_kill(meminfo: &str, elapsed: Duration) -> Option<String> {
    if elapsed > WALL_MAX {
        return Some(format!("ran longer than {} s", WALL_MAX.as_secs()));
    }
    let (total, avail) = parse_meminfo(meminfo)?;
    let floor = (total / 20).max(512 * MIB);
    (avail < floor).then(|| format!("free memory fell to {} MiB", avail / MIB))
}

// ---------------------------------------------------------------- reading the system

pub fn read_meminfo() -> String {
    std::fs::read_to_string("/proc/meminfo").unwrap_or_default()
}

fn read_supplies() -> Vec<(String, String)> {
    let Ok(rd) = std::fs::read_dir("/sys/class/power_supply") else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| {
            let t = std::fs::read_to_string(e.path().join("type")).ok()?;
            Some((t, std::fs::read_to_string(e.path().join("online")).unwrap_or_default()))
        })
        .collect()
}

/// Size of the model file for `spec` (path or URL cached in the models dir); a guess if not downloaded yet.
pub fn model_size(spec: &str) -> u64 {
    let path: PathBuf = if spec.starts_with("http://") || spec.starts_with("https://") {
        let name = spec.split(['?', '#']).next().unwrap_or(spec).rsplit('/').next().unwrap_or("");
        let base = match std::env::var_os("XDG_DATA_HOME") {
            Some(d) if !d.is_empty() => PathBuf::from(d),
            _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"),
        };
        base.join("nt/models").join(name)
    } else {
        Path::new(spec).to_path_buf()
    };
    std::fs::metadata(path).map_or(1200 * MIB, |m| m.len())
}

/// One or two words for a `may_start` error, for the switcher header.
pub fn short_reason(why: &str) -> &'static str {
    if why.contains("battery") {
        "battery"
    } else if why.contains("pressure") {
        "memory pressure"
    } else {
        "low memory"
    }
}

/// The live check before starting a local-model run.
pub fn check_system(model_spec: &str, allow_battery: bool) -> Result<(), String> {
    let pressure = std::fs::read_to_string("/proc/pressure/memory").ok();
    may_start(&read_meminfo(), pressure.as_deref(), model_size(model_spec), on_battery(&read_supplies()), allow_battery)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEM: &str = "MemTotal:       16000000 kB\nMemFree:         100000 kB\nMemAvailable:    4000000 kB\nBuffers: 1 kB\n";
    const PSI: &str = "some avg10=0.31 avg60=0.10 avg300=0.02 total=12345\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";

    #[test]
    fn parsing() {
        assert_eq!(parse_meminfo(MEM), Some((16_000_000 * 1024, 4_000_000 * 1024)));
        assert_eq!(parse_meminfo("MemTotal: 5 kB\n"), None);
        assert_eq!(parse_pressure(PSI), Some(0.31));
        assert_eq!(parse_pressure("full avg10=1.0\n"), None);
    }

    #[test]
    fn start_gating() {
        let gb = 1000 * MIB; // a 1000 MiB model needs 1600 MiB + 1024 MiB margin = 2624 MiB
        assert_eq!(estimate(gb), 1000 * MIB + 300 * MIB + 300 * MIB);
        assert!(may_start(MEM, Some(PSI), gb, false, false).is_ok());
        // too little available: 2 GB available
        let low = "MemTotal: 16000000 kB\nMemAvailable: 2000000 kB\n";
        assert!(may_start(low, Some(PSI), gb, false, false).unwrap_err().contains("free memory"));
        // pressure
        let busy = "some avg10=5.00 avg60=1 avg300=1 total=1\n";
        assert!(may_start(MEM, Some(busy), gb, false, false).unwrap_err().contains("pressure"));
        assert!(may_start(MEM, Some("some avg10=4.99 avg60=1 avg300=1 total=1\n"), gb, false, false).is_ok());
        assert!(may_start(MEM, None, gb, false, false).is_ok()); // no PSI support
        // battery
        assert!(may_start(MEM, Some(PSI), gb, true, false).unwrap_err().contains("battery"));
        assert!(may_start(MEM, Some(PSI), gb, true, true).is_ok());
        assert_eq!(short_reason(&may_start(MEM, Some(PSI), gb, true, false).unwrap_err()), "battery");
        assert_eq!(short_reason(&may_start(low, Some(PSI), gb, false, false).unwrap_err()), "low memory");
        assert_eq!(short_reason(&may_start(MEM, Some(busy), gb, false, false).unwrap_err()), "memory pressure");
        assert!(may_start("junk", None, gb, false, true).is_err());
    }

    #[test]
    fn power_supplies() {
        let s = |t: &str, o: &str| (t.to_string(), o.to_string());
        assert!(on_battery(&[s("Battery\n", ""), s("Mains\n", "0\n")]));
        assert!(!on_battery(&[s("Battery\n", ""), s("Mains\n", "1\n")]));
        assert!(!on_battery(&[s("Battery\n", "")])); // no mains entry: unknown, run
        assert!(!on_battery(&[]));
        assert!(!on_battery(&[s("Mains\n", "0\n"), s("Mains\n", "1\n")])); // one of several chargers online
    }

    #[test]
    fn watchdog() {
        let s = Duration::from_secs;
        assert_eq!(must_kill(MEM, s(10)), None);
        assert!(must_kill(MEM, s(181)).unwrap().contains("longer"));
        // 16 GB box: floor is 5% = 800 MB
        assert!(must_kill("MemTotal: 16000000 kB\nMemAvailable: 700000 kB\n", s(1)).is_some());
        assert_eq!(must_kill("MemTotal: 16000000 kB\nMemAvailable: 900000 kB\n", s(1)), None);
        // small box: the 512 MiB floor wins over 5%
        assert!(must_kill("MemTotal: 4000000 kB\nMemAvailable: 500000 kB\n", s(1)).is_some());
        assert_eq!(must_kill("MemTotal: 4000000 kB\nMemAvailable: 600000 kB\n", s(1)), None);
    }
}
