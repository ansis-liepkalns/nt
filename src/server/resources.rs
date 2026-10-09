//! Per-session CPU and memory: one scan of /proc, processes grouped by their session id (the session's
//! shell is a session leader, so its sid is the shell pid). Unreadable processes are skipped.

use std::collections::HashMap;
use std::time::Instant;

/// `(session id, utime+stime ticks, resident pages)` from the contents of /proc/<pid>/stat.
pub fn parse_stat(s: &str) -> Option<(u32, u64, u64)> {
    // The command name may contain spaces and parentheses: fields start after the last ')'.
    let rest = &s[s.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let num = |i: usize| f.get(i)?.parse::<u64>().ok();
    // f[0] = state, f[3] = session, f[11] = utime, f[12] = stime, f[21] = rss (pages)
    Some((num(3)? as u32, num(11)? + num(12)?, num(21)?))
}

struct Scan {
    /// pid -> (sid, ticks, pages)
    procs: HashMap<u32, (u32, u64, u64)>,
}

fn scan() -> Scan {
    let mut procs = HashMap::new();
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
            if let Some(v) = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok().as_deref().and_then(parse_stat) {
                procs.insert(pid, v);
            }
        }
    }
    Scan { procs }
}

#[derive(Default)]
pub struct Sampler {
    prev: HashMap<u32, u64>,
    at: Option<Instant>,
}

impl Sampler {
    /// sid -> (cpu percent of one core since the previous sample, resident bytes). The first sample reports 0% CPU.
    pub fn sample(&mut self, now: Instant) -> HashMap<u32, (f32, u64)> {
        let scan = scan();
        // SAFETY: plain sysconf calls.
        let (page, hz) = unsafe { (nix::libc::sysconf(nix::libc::_SC_PAGESIZE).max(4096) as u64, nix::libc::sysconf(nix::libc::_SC_CLK_TCK).max(1) as f64) };
        let dt = self.at.map_or(0.0, |t| now.duration_since(t).as_secs_f64());
        let mut out: HashMap<u32, (f32, u64)> = HashMap::new();
        for (pid, &(sid, ticks, pages)) in &scan.procs {
            let e = out.entry(sid).or_insert((0.0, 0));
            e.1 += pages * page;
            if dt > 0.0 {
                // A process new since the last sample counts from zero.
                let d = ticks.saturating_sub(self.prev.get(pid).copied().unwrap_or(0));
                e.0 += (d as f64 / hz / dt * 100.0) as f32;
            }
        }
        self.prev = scan.procs.iter().map(|(p, v)| (*p, v.1)).collect();
        self.at = Some(now);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    #[test]
    fn stat_parsing() {
        let s = "42 (we ird) name) S 1 42 42 0 -1 4194560 100 0 0 0 7 5 0 0 20 0 1 0 100 1000 33 18446744073709551615";
        assert_eq!(parse_stat(s), Some((42, 12, 33)));
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn aggregates_a_process_tree() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("yes >/dev/null & sleep 5");
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                nix::libc::setsid();
                Ok(())
            });
        }
        let mut child = cmd.spawn().unwrap();
        let sid = child.id();
        let mut s = Sampler::default();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let first = s.sample(Instant::now());
        assert_eq!(first[&sid].0, 0.0);
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let second = s.sample(Instant::now());
        let (cpu, rss) = second[&sid];
        nix::sys::signal::killpg(nix::unistd::Pid::from_raw(sid as i32), nix::sys::signal::Signal::SIGKILL).unwrap();
        let _ = child.wait();
        assert!(cpu > 50.0, "cpu = {cpu}");
        assert!(rss > 100_000, "rss = {rss}");
    }
}
