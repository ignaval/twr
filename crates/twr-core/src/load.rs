//! How loaded the machine is: what the hub tells the orchestrator in `hub.info`, and what the TUI
//! shows at the start of its status bar while the machine is busy.
//!
//! On Linux it reads the kernel's pressure stall information (`/proc/pressure/{cpu,memory,io}`,
//! the share of the last 10 seconds that work waited for a CPU, for memory or for the disk) and
//! the load average (`/proc/loadavg`). Pressure says how much work waits, which the load average
//! alone can't: a load of 6 on 4 cores may wait little, and 2 tests swapping may wait a lot.
//! Without CPU pressure (a kernel built without it, `psi=0`, or another system) the load
//! average per online CPU speaks for the CPUs; on macOS it comes from `sysctl -n vm.loadavg
//! hw.logicalcpu`, given a second at most.
//!
//! The levels ([`Level`]) are deliberately coarse: [`Level::Saturated`] means more heavy work
//! (another build, another test run) makes everything slower, so the orchestrator holds it back.
//! Sampling never fails loudly: what can't be read is unknown, and nothing known is no load.

use serde::Serialize;
use serde_json::{json, Value};

/// How loaded the machine is, coarsely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Work rarely waits.
    Calm,
    /// Work waits noticeably: builds and tests run slower than alone.
    Busy,
    /// Work waits most of the time: more heavy work slows everything down.
    Saturated,
}

impl Level {
    /// Its name, as in JSON: `calm`, `busy` or `saturated`.
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Calm => "calm",
            Level::Busy => "busy",
            Level::Saturated => "saturated",
        }
    }
}

/// CPU pressure (share of time some work waited for a CPU, %) at which the machine is busy and
/// saturated.
pub const CPU_PRESSURE: (f64, f64) = (40.0, 80.0);
/// Memory pressure (some work waited for memory, %) at which the machine is busy and saturated.
pub const MEMORY_PRESSURE: (f64, f64) = (10.0, 30.0);
/// Disk pressure (all work waited for the disk, % of `full`) at which the machine is busy and
/// saturated.
pub const IO_PRESSURE: (f64, f64) = (10.0, 30.0);
/// Load average per core at which the machine counts as busy and saturated when there is no
/// pressure information.
pub const LOAD_PER_CORE: (f64, f64) = (1.5, 3.0);

/// One reading of the machine's load. Pressures are the 10-second averages, in percent.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Load {
    /// CPU pressure: share of time at least one task waited for a CPU (`some avg10`).
    pub cpu: Option<f64>,
    /// Memory pressure: share of time at least one task waited for memory (`some avg10`).
    pub memory: Option<f64>,
    /// Disk pressure: share of time every task waited for the disk (`full avg10`).
    pub io: Option<f64>,
    /// The one-minute load average.
    pub load1: Option<f64>,
    /// The cores the system has (at least 1).
    pub cores: usize,
}

impl Load {
    /// Reads the machine's load now; `None` when nothing could be read. On macOS this runs
    /// `sysctl`, given at most [`SYSCTL_DEADLINE`]: call it off an event loop.
    pub fn sample() -> Option<Load> {
        #[cfg(target_os = "linux")]
        {
            let read = |path: &str| std::fs::read_to_string(path).ok();
            let cores = read("/sys/devices/system/cpu/online")
                .as_deref()
                .and_then(parse_cpu_list)
                .unwrap_or_else(fallback_cores);
            Load::from_proc(
                read("/proc/pressure/cpu").as_deref(),
                read("/proc/pressure/memory").as_deref(),
                read("/proc/pressure/io").as_deref(),
                read("/proc/loadavg").as_deref(),
                cores,
            )
        }
        #[cfg(not(target_os = "linux"))]
        {
            // One run for both: the load average, then the logical CPUs.
            let out = run_with_deadline("sysctl", &["-n", "vm.loadavg", "hw.logicalcpu"])?;
            let mut lines = out.lines();
            let load1 = parse_sysctl_loadavg(lines.next()?)?;
            let cores = lines
                .next()
                .and_then(|l| l.trim().parse::<usize>().ok())
                .filter(|n| *n > 0)
                .unwrap_or_else(fallback_cores);
            Some(Load {
                load1: Some(load1),
                cores,
                ..Load::default()
            })
        }
    }

    /// The latest reading, for callers on every request (`hub.info`) that must not wait: on
    /// Linux [`Load::sample`] read again once the last reading is a second old (a few small
    /// files); elsewhere what a background thread reads every 2 seconds (`None` until its first
    /// reading), so a slow `sysctl` never holds up a request.
    pub fn sample_cached() -> Option<Load> {
        use std::sync::Mutex;
        use std::time::{Duration, Instant};
        static LAST: Mutex<Option<(Instant, Option<Load>)>> = Mutex::new(None);
        let lock = || LAST.lock().unwrap_or_else(|e| e.into_inner());
        if cfg!(target_os = "linux") {
            if let Some((at, load)) = &*lock() {
                if at.elapsed() < Duration::from_secs(1) {
                    return load.clone();
                }
            }
            let load = Load::sample();
            *lock() = Some((Instant::now(), load.clone()));
            load
        } else {
            static SAMPLER: std::sync::Once = std::sync::Once::new();
            SAMPLER.call_once(|| {
                let spawned =
                    std::thread::Builder::new()
                        .name("twr-load".into())
                        .spawn(move || loop {
                            let load = Load::sample();
                            *lock() = Some((Instant::now(), load));
                            std::thread::sleep(Duration::from_secs(2));
                        });
                if let Err(e) = spawned {
                    tracing::warn!(error = %e, "cannot start the load sampler");
                }
            });
            lock().as_ref().and_then(|(_, load)| load.clone())
        }
    }

    /// The load from the contents of `/proc/pressure/{cpu,memory,io}` and `/proc/loadavg` (each
    /// `None` when it couldn't be read), on a machine with `cores` cores; `None` when none of
    /// them says anything.
    pub fn from_proc(
        cpu: Option<&str>,
        memory: Option<&str>,
        io: Option<&str>,
        loadavg: Option<&str>,
        cores: usize,
    ) -> Option<Load> {
        let load = Load {
            cpu: cpu.and_then(|s| pressure(s, "some")),
            memory: memory.and_then(|s| pressure(s, "some")),
            io: io.and_then(|s| pressure(s, "full")),
            load1: loadavg.and_then(parse_loadavg),
            cores: cores.max(1),
        };
        (load.cpu.is_some() || load.memory.is_some() || load.io.is_some() || load.load1.is_some())
            .then_some(load)
    }

    /// The load average per core, when known.
    pub fn per_core(&self) -> Option<f64> {
        self.load1.map(|l| l / self.cores.max(1) as f64)
    }

    /// Each measure that decides the level, with its value and the level it alone gives:
    /// the pressures read, and the load average per core when CPU pressure is unknown.
    fn measures(&self) -> Vec<(&'static str, f64, Level)> {
        let level = |v: f64, (busy, saturated): (f64, f64)| {
            if v >= saturated {
                Level::Saturated
            } else if v >= busy {
                Level::Busy
            } else {
                Level::Calm
            }
        };
        let mut out: Vec<_> = [
            ("cpu", self.cpu, CPU_PRESSURE),
            ("mem", self.memory, MEMORY_PRESSURE),
            ("io", self.io, IO_PRESSURE),
        ]
        .into_iter()
        .filter_map(|(name, v, t)| v.map(|v| (name, v, level(v, t))))
        .collect();
        // CPU pressure says whether work waits for a CPU; without it the load average per
        // core is the best word on the CPUs, beside what the other pressures say.
        if self.cpu.is_none() {
            if let Some(v) = self.per_core() {
                out.push(("load", v, level(v, LOAD_PER_CORE)));
            }
        }
        out
    }

    /// How loaded the machine is: the highest level any measure gives.
    pub fn level(&self) -> Level {
        self.measures()
            .into_iter()
            .map(|(_, _, l)| l)
            .max()
            .unwrap_or(Level::Calm)
    }

    /// The measures at [`Level::Busy`] or above, the highest first, as short text with their
    /// level: `[("cpu 92%", Saturated), ("mem 14%", Busy)]`, or `[("load 2.1/core", Busy)]`.
    /// Empty while the machine is calm.
    pub fn pressing(&self) -> Vec<(String, Level)> {
        let mut out: Vec<_> = self
            .measures()
            .into_iter()
            .filter(|(_, _, l)| *l >= Level::Busy)
            .collect();
        out.sort_by_key(|m| std::cmp::Reverse(m.2));
        out.into_iter()
            .map(|(name, v, l)| {
                let text = if name == "load" {
                    format!("load {v:.1}/core")
                } else {
                    format!("{name} {v:.0}%")
                };
                (text, l)
            })
            .collect()
    }

    /// One line for people and the orchestrator: `saturated: CPU pressure 92%, memory pressure
    /// 3%, disk pressure 0%, load 61.2 on 4 cores`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(v) = self.cpu {
            parts.push(format!("CPU pressure {v:.0}%"));
        }
        if let Some(v) = self.memory {
            parts.push(format!("memory pressure {v:.0}%"));
        }
        if let Some(v) = self.io {
            parts.push(format!("disk pressure {v:.0}%"));
        }
        if let Some(v) = self.load1 {
            parts.push(format!("load {v:.1} on {} cores", self.cores));
        }
        format!("{}: {}", self.level().as_str(), parts.join(", "))
    }

    /// `hub.info`'s `load`: `{level, summary, cpu_pressure, memory_pressure, io_pressure, load1,
    /// cores}`, an unknown measure `null`.
    pub fn to_json(&self) -> Value {
        json!({
            "level": self.level(),
            "summary": self.summary(),
            "cpu_pressure": self.cpu,
            "memory_pressure": self.memory,
            "io_pressure": self.io,
            "load1": self.load1,
            "cores": self.cores,
        })
    }
}

/// How long `sysctl` may take on macOS before the reading is given up as unknown.
pub const SYSCTL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(1);

/// The machine's CPUs: the online ones on Linux, else those this process may use.
pub fn machine_cpus() -> usize {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/sys/devices/system/cpu/online")
            .ok()
            .as_deref()
            .and_then(parse_cpu_list)
            .unwrap_or_else(fallback_cores)
    }
    #[cfg(not(target_os = "linux"))]
    {
        fallback_cores()
    }
}

/// The CPUs this process may use, when the system's count can't be read.
fn fallback_cores() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// How many CPUs a Linux CPU list names (`/sys/devices/system/cpu/online`: `0-3,8,10-11`).
/// The machine's online CPUs, not the ones this process may use (an affinity or a cgroup
/// quota), since the load average is the machine's.
pub fn parse_cpu_list(text: &str) -> Option<usize> {
    let mut n = 0usize;
    for part in text.trim().split(',') {
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => (a.parse::<usize>().ok()?, b.parse::<usize>().ok()?),
            None => {
                let a = part.parse::<usize>().ok()?;
                (a, a)
            }
        };
        n += b.checked_sub(a)? + 1;
    }
    (n > 0).then_some(n)
}

/// `program args`'s standard output, when it exits successfully within [`SYSCTL_DEADLINE`];
/// killed and reaped past it. It runs in a session of its own (`setsid`), without the hub's
/// controlling terminal: a `sysctl` (or a wrapper on `PATH`) that touches one gets an error
/// instead of stopping the hub's process group.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn run_with_deadline(program: &str, args: &[&str]) -> Option<String> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe, and nothing else runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().ok()?;
    let deadline = std::time::Instant::now() + SYSCTL_DEADLINE;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    status.success().then_some(out)
}

/// The `avg10` of the `kind` line (`some` or `full`) of a pressure file, in percent.
fn pressure(text: &str, kind: &str) -> Option<f64> {
    text.lines()
        .find_map(|l| l.strip_prefix(kind)?.strip_prefix(' '))?
        .split_whitespace()
        .find_map(|f| f.strip_prefix("avg10="))?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

/// The one-minute load average of `/proc/loadavg` (`2.53 0.94 0.51 4/402 203454`).
fn parse_loadavg(text: &str) -> Option<f64> {
    text.split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

/// The one-minute load average of macOS's `sysctl -n vm.loadavg` (`{ 1.85 1.79 1.73 }`).
pub fn parse_sysctl_loadavg(text: &str) -> Option<f64> {
    text.trim()
        .trim_start_matches('{')
        .split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn psi(some: f64, full: f64) -> String {
        format!(
            "some avg10={some:.2} avg60=3.89 avg300=1.32 total=7964765742\n\
             full avg10={full:.2} avg60=0.00 avg300=0.00 total=0\n"
        )
    }

    fn load(cpu: f64, mem: f64, io_full: f64, load1: &str, cores: usize) -> Load {
        Load::from_proc(
            Some(&psi(cpu, 0.0)),
            Some(&psi(mem, 0.0)),
            Some(&psi(0.5, io_full)),
            Some(load1),
            cores,
        )
        .unwrap()
    }

    #[test]
    fn reads_the_ten_second_averages() {
        let l = load(3.29, 0.0, 0.03, "2.53 0.94 0.51 4/402 203454", 4);
        assert_eq!(l.cpu, Some(3.29));
        assert_eq!(l.memory, Some(0.0));
        // Disk pressure is the `full` line: every task waiting, not one.
        assert_eq!(l.io, Some(0.03));
        assert_eq!(l.load1, Some(2.53));
        assert_eq!(l.level(), Level::Calm);
        assert!(l.pressing().is_empty());
        assert_eq!(
            l.summary(),
            "calm: CPU pressure 3%, memory pressure 0%, disk pressure 0%, load 2.5 on 4 cores"
        );
    }

    #[test]
    fn the_highest_measure_decides() {
        let busy = load(45.0, 2.0, 0.0, "6.0 1 1 1/1 1", 4);
        assert_eq!(busy.level(), Level::Busy);
        assert_eq!(busy.pressing(), vec![("cpu 45%".to_string(), Level::Busy)]);

        let swapping = load(20.0, 35.0, 12.0, "3.0 1 1 1/1 1", 4);
        assert_eq!(swapping.level(), Level::Saturated);
        assert_eq!(
            swapping.pressing(),
            vec![
                ("mem 35%".to_string(), Level::Saturated),
                ("io 12%".to_string(), Level::Busy)
            ]
        );

        let builds = load(97.0, 0.0, 0.0, "61.2 40 20 70/900 1", 4);
        assert_eq!(builds.level(), Level::Saturated);
        assert!(builds.summary().starts_with("saturated: CPU pressure 97%"));
        assert!(builds.summary().ends_with("load 61.2 on 4 cores"));
    }

    #[test]
    fn thresholds_are_inclusive() {
        assert_eq!(load(40.0, 0.0, 0.0, "1 1 1 1/1 1", 4).level(), Level::Busy);
        assert_eq!(
            load(80.0, 0.0, 0.0, "1 1 1 1/1 1", 4).level(),
            Level::Saturated
        );
        assert_eq!(
            load(39.99, 9.99, 9.99, "1 1 1 1/1 1", 4).level(),
            Level::Calm
        );
    }

    /// With pressure known, the load average alone never raises the level: pressure says
    /// whether work waits.
    #[test]
    fn pressure_wins_over_the_load_average() {
        assert_eq!(
            load(5.0, 0.0, 0.0, "40.0 1 1 1/1 1", 4).level(),
            Level::Calm
        );
    }

    /// Without CPU pressure, the load average per core speaks for the CPUs beside the other
    /// pressures.
    #[test]
    fn without_cpu_pressure_the_load_average_joins_the_others() {
        let l = Load::from_proc(
            None,
            Some(&psi(0.0, 0.0)),
            Some(&psi(0.0, 0.0)),
            Some("24.0 1 1 1/1 1"),
            4,
        )
        .unwrap();
        assert_eq!(l.level(), Level::Saturated);
        assert_eq!(
            l.pressing(),
            vec![("load 6.0/core".to_string(), Level::Saturated)]
        );
    }

    #[test]
    fn counts_the_online_cpus() {
        assert_eq!(parse_cpu_list("0-3\n"), Some(4));
        assert_eq!(parse_cpu_list("0-3,8,10-11"), Some(7));
        assert_eq!(parse_cpu_list("0"), Some(1));
        assert_eq!(parse_cpu_list(""), None);
        assert_eq!(parse_cpu_list("3-1"), None);
        assert_eq!(parse_cpu_list("a-b"), None);
    }

    #[test]
    fn a_command_past_its_deadline_is_unknown() {
        assert_eq!(
            run_with_deadline("sh", &["-c", "echo hi"]).as_deref(),
            Some("hi\n")
        );
        assert_eq!(run_with_deadline("sh", &["-c", "exit 3"]), None);
        let t = std::time::Instant::now();
        assert_eq!(run_with_deadline("sleep", &["30"]), None);
        assert!(t.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!(run_with_deadline("/nonexistent/twr-sysctl", &[]), None);
    }

    #[test]
    fn without_pressure_the_load_average_per_core_decides() {
        let at = |text: &str| Load::from_proc(None, None, None, Some(text), 4).unwrap();
        assert_eq!(at("5.9 1 1 1/1 1").level(), Level::Calm);
        assert_eq!(at("6.0 1 1 1/1 1").level(), Level::Busy);
        let l = at("12.4 1 1 1/1 1");
        assert_eq!(l.level(), Level::Saturated);
        assert_eq!(
            l.pressing(),
            vec![("load 3.1/core".to_string(), Level::Saturated)]
        );
        assert_eq!(l.summary(), "saturated: load 12.4 on 4 cores");
    }

    #[test]
    fn unreadable_or_garbled_input_is_unknown() {
        assert_eq!(Load::from_proc(None, None, None, None, 4), None);
        assert_eq!(
            Load::from_proc(Some("garbage"), Some(""), None, Some("x y z"), 4),
            None
        );
        // A pressure file without its `full` line (older kernels' cpu file) still gives `some`.
        let l = Load::from_proc(
            Some("some avg10=50.00 avg60=1 avg300=1 total=1\n"),
            None,
            Some("some avg10=1.00 avg60=1 avg300=1 total=1\n"),
            None,
            0,
        )
        .unwrap();
        assert_eq!((l.cpu, l.io, l.cores), (Some(50.0), None, 1));
        assert_eq!(l.level(), Level::Busy);
        // Negative or non-finite numbers are no reading.
        assert_eq!(pressure("some avg10=-1.00 avg60=1", "some"), None);
        assert_eq!(pressure("some avg10=NaN avg60=1", "some"), None);
        assert_eq!(parse_loadavg("inf 1 1"), None);
    }

    #[test]
    fn reads_macos_sysctl_loadavg() {
        assert_eq!(parse_sysctl_loadavg("{ 1.85 1.79 1.73 }\n"), Some(1.85));
        assert_eq!(parse_sysctl_loadavg("{1.85 1.79 1.73}"), Some(1.85));
        assert_eq!(parse_sysctl_loadavg(""), None);
    }

    #[test]
    fn json_names_the_level_and_unknowns_are_null() {
        let l = Load::from_proc(None, None, None, Some("13 1 1 1/1 1"), 4).unwrap();
        let v = l.to_json();
        assert_eq!(v["level"], "saturated");
        assert_eq!(v["cpu_pressure"], Value::Null);
        assert_eq!(v["load1"], 13.0);
        assert_eq!(v["cores"], 4);
        assert_eq!(v["summary"], "saturated: load 13.0 on 4 cores");
    }

    /// This machine's own reading parses (whatever its level).
    #[cfg(target_os = "linux")]
    #[test]
    fn samples_this_machine() {
        if std::path::Path::new("/proc/loadavg").exists() {
            assert!(Load::sample().is_some());
        }
    }
}
