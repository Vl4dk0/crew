#![allow(clippy::collapsible_if)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::error::Error;

/// Providers in print order, with the subcommand names that select each.
const PROVIDERS: [(&str, &[&str]); 4] = [
    ("claude", &["claude"]),
    ("gemini", &["gemini", "agy", "antigravity"]),
    ("grok", &["grok"]),
    ("codex", &["codex", "gpt", "chatgpt"]),
];

#[derive(Debug, Clone, Default)]
pub struct SystemMetrics {
    pub cpu_percent: Option<f64>,
    pub load_1m: Option<f64>,
    pub cores: Option<usize>,
    pub ram_used_bytes: Option<u64>,
    pub ram_total_bytes: Option<u64>,
    pub gpu: GpuReport,
    pub swap_used_bytes: Option<u64>,
}

/// macOS has one accelerator reading. Linux lists every DRM card.
#[derive(Debug, Clone)]
pub enum GpuReport {
    Mac(Option<u32>),
    Linux(Vec<GpuCard>),
}

impl Default for GpuReport {
    fn default() -> Self {
        Self::Mac(None)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuCard {
    pub vendor: String,
    pub percent: Option<u32>,
}

/// One DRM card, already read from sysfs. Parsing stays free of I/O so the
/// fixture tests can feed file contents directly. `idle_path` is only used
/// by the sampler; both residency reads have to hit the same file.
struct CardSnap {
    name: String,
    device_key: String,
    vendor_text: Option<String>,
    driver: Option<String>,
    busy_text: Option<String>,
    idle_path: Option<PathBuf>,
    idle_before_ms: Option<u64>,
    idle_after_ms: Option<u64>,
    wall_ms: u64,
}

#[derive(Debug, Clone)]
pub struct ProcessRecord {
    pub pid: u32,
    pub ppid: u32,
    pub cpu: f64,
    pub executable: String,
    pub arg1: String,
}

/// Execute a system command and return stdout as string, or empty string on error.
fn run_cmd(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).to_string())
        .unwrap_or_default()
}

/// Parse process snapshot from `ps -axww -o pid=,ppid=,%cpu=,command=`.
pub fn parse_ps_snapshot(output: &str) -> (HashMap<u32, ProcessRecord>, f64) {
    let mut processes = HashMap::new();
    let mut cpu_sum = 0.0;

    for line in output.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() < 4 {
            continue;
        }

        let pid: u32 = match parts[0].parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let ppid: u32 = match parts[1].parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let cpu: f64 = match parts[2].parse() {
            Ok(c) => c,
            Err(_) => continue,
        };

        let executable = parts[3].to_string();
        let arg1 = if parts.len() > 4 {
            parts[4].to_string()
        } else {
            String::new()
        };

        cpu_sum += cpu;
        processes.insert(
            pid,
            ProcessRecord {
                pid,
                ppid,
                cpu,
                executable,
                arg1,
            },
        );
    }

    (processes, cpu_sum)
}

/// Collect hardware telemetry on macOS (sysctl, vm_stat, ioreg).
pub fn collect_metrics(cpu_sum: f64, has_processes: bool) -> SystemMetrics {
    let mut metrics = SystemMetrics::default();

    if cfg!(target_os = "macos") {
        let sysctl_out = run_cmd(
            "sysctl",
            &["-n", "hw.ncpu", "vm.loadavg", "hw.memsize", "vm.swapusage"],
        );
        let lines: Vec<&str> = sysctl_out.lines().collect();

        if let Some(l) = lines.first() {
            if let Ok(c) = l.trim().parse::<usize>() {
                metrics.cores = Some(c);
            }
        }
        if lines.len() > 1 {
            // "{ 2.45 2.10 1.95 }"
            if let Some(first_num) = lines[1]
                .split_whitespace()
                .find(|s| !s.starts_with('{'))
                .and_then(|s| s.parse::<f64>().ok())
            {
                metrics.load_1m = Some(first_num);
            }
        }
        if lines.len() > 2 {
            if let Ok(total) = lines[2].trim().parse::<u64>() {
                metrics.ram_total_bytes = Some(total);
            }
        }
        if lines.len() > 3 {
            // "total = 3072.00M  used = 512.00M  free = 2560.00M  (encrypted)"
            if let Some(used_idx) = lines[3].find("used = ") {
                let rest = &lines[3][used_idx + 7..];
                if let Some(token) = rest.split_whitespace().next() {
                    if let Some(last_char) = token.chars().last() {
                        let num_part = &token[..token.len() - 1];
                        if let Ok(val) = num_part.parse::<f64>() {
                            let scale: u64 = match last_char {
                                'K' => 1024,
                                'M' => 1024 * 1024,
                                'G' => 1024 * 1024 * 1024,
                                _ => 1,
                            };
                            metrics.swap_used_bytes = Some((val * scale as f64) as u64);
                        }
                    }
                }
            }
        }

        // vm_stat for used RAM
        let vm_out = run_cmd("vm_stat", &[]);
        let mut page_size: u64 = 4096;
        for line in vm_out.lines() {
            if line.contains("page size of ") {
                if let Some(start) = line.find("page size of ") {
                    let rest = &line[start + 13..];
                    if let Some(end) = rest.find(" bytes") {
                        if let Ok(ps) = rest[..end].trim().parse::<u64>() {
                            page_size = ps;
                        }
                    }
                }
            }
        }

        let mut anonymous: Option<u64> = None;
        let mut purgeable: Option<u64> = None;
        let mut wired: Option<u64> = None;
        let mut compressor: Option<u64> = None;

        for line in vm_out.lines() {
            if line.starts_with("Anonymous pages:") {
                anonymous = extract_vm_stat_count(line);
            } else if line.starts_with("Pages purgeable:") {
                purgeable = extract_vm_stat_count(line);
            } else if line.starts_with("Pages wired down:") {
                wired = extract_vm_stat_count(line);
            } else if line.starts_with("Pages occupied by compressor:") {
                compressor = extract_vm_stat_count(line);
            }
        }

        if let (Some(anon), Some(purge), Some(wire), Some(comp)) =
            (anonymous, purgeable, wired, compressor)
        {
            let app_pages = anon.saturating_sub(purge);
            let used_pages = app_pages + wire + comp;
            metrics.ram_used_bytes = Some(used_pages * page_size);
        }

        // GPU via ioreg
        let ioreg_out = run_cmd(
            "ioreg",
            &["-r", "-d", "1", "-w", "0", "-c", "IOAccelerator"],
        );
        let mut max_gpu = 0;
        let mut found_gpu = false;
        let target = "\"Device Utilization %\"";
        let mut idx = 0;
        while let Some(pos) = ioreg_out[idx..].find(target) {
            let start = idx + pos + target.len();
            if let Some(eq_rel) = ioreg_out[start..].find('=') {
                let num_start = start + eq_rel + 1;
                let num_digits: String = ioreg_out[num_start..]
                    .chars()
                    .skip_while(|c| c.is_whitespace())
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(val) = num_digits.parse::<u32>() {
                    max_gpu = max_gpu.max(val);
                    found_gpu = true;
                }
            }
            idx = start;
        }
        if found_gpu {
            metrics.gpu = GpuReport::Mac(Some(max_gpu));
        }
    } else if cfg!(target_os = "linux") {
        fill_linux_metrics(&mut metrics);
    }

    if let Some(cores) = metrics.cores {
        if cores > 0 && has_processes {
            metrics.cpu_percent = Some(cpu_sum / cores as f64);
        }
    }

    metrics
}

fn extract_vm_stat_count(line: &str) -> Option<u64> {
    let colon_pos = line.find(':')?;
    let val_str = line[colon_pos + 1..].trim().trim_end_matches('.');
    val_str.parse::<u64>().ok()
}

fn fill_linux_metrics(metrics: &mut SystemMetrics) {
    if let Ok(text) = std::fs::read_to_string("/proc/loadavg") {
        metrics.load_1m = load_1m_from_loadavg(&text);
    }
    if let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") {
        metrics.cores = cores_from_cpuinfo(&text);
    }
    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        if let Some(mem) = parse_meminfo(&text) {
            metrics.ram_total_bytes = Some(mem.total_bytes);
            metrics.ram_used_bytes = Some(mem.used_bytes);
            metrics.swap_used_bytes = mem.swap_used_bytes;
        }
    }
    metrics.gpu = GpuReport::Linux(linux_gpus());
}

struct MemSnap {
    total_bytes: u64,
    used_bytes: u64,
    swap_used_bytes: Option<u64>,
}

fn load_1m_from_loadavg(text: &str) -> Option<f64> {
    text.split_whitespace().next()?.parse().ok()
}

/// Logical CPUs, matching macOS `hw.ncpu`. A `processor` key is one logical CPU.
fn cores_from_cpuinfo(text: &str) -> Option<usize> {
    let n = text
        .lines()
        .filter(|line| {
            line.split_once(':')
                .is_some_and(|(key, _)| key.trim() == "processor")
        })
        .count();
    if n == 0 { None } else { Some(n) }
}

/// `MemAvailable` is the kernel's own estimate of memory that can be used
/// without swapping. Used is what is not in that pool.
fn parse_meminfo(text: &str) -> Option<MemSnap> {
    let mut total_kb = None;
    let mut available_kb = None;
    let mut swap_total_kb = None;
    let mut swap_free_kb = None;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else { continue };
        let Some(value) = parts.next().and_then(|token| token.parse::<u64>().ok()) else {
            continue;
        };
        match key {
            "MemTotal:" => total_kb = Some(value),
            "MemAvailable:" => available_kb = Some(value),
            "SwapTotal:" => swap_total_kb = Some(value),
            "SwapFree:" => swap_free_kb = Some(value),
            _ => {}
        }
    }
    let total_kb = total_kb?;
    let available_kb = available_kb?;
    let total_bytes = total_kb.checked_mul(1024)?;
    let available_bytes = available_kb.checked_mul(1024)?;
    // A broken swap figure must not throw away the RAM reading.
    let swap_used_bytes = match (swap_total_kb, swap_free_kb) {
        (Some(total), Some(free)) => total.saturating_sub(free).checked_mul(1024),
        _ => None,
    };
    Some(MemSnap {
        total_bytes,
        used_bytes: total_bytes.saturating_sub(available_bytes),
        swap_used_bytes,
    })
}

fn linux_gpus() -> Vec<GpuCard> {
    let mut cards = read_drm_cards();
    if cards
        .iter()
        .any(|card| card_needs_idle_sample(card.driver.as_deref()))
    {
        let started = Instant::now();
        let befores: Vec<Option<u64>> = cards.iter().map(read_idle_counter).collect();
        std::thread::sleep(Duration::from_millis(200));
        let wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(0);
        for (card, before) in cards.iter_mut().zip(befores) {
            card.idle_before_ms = before;
            card.idle_after_ms = read_idle_counter(card);
            card.wall_ms = wall_ms;
        }
    }
    let nvidia_smi = if cards
        .iter()
        .any(|card| is_nvidia_smi_driver(card.driver.as_deref()))
    {
        run_cmd(
            "nvidia-smi",
            &[
                "--query-gpu=index,utilization.gpu",
                "--format=csv,noheader,nounits",
            ],
        )
    } else {
        String::new()
    };
    gpus_from_cards(cards, &nvidia_smi)
}

fn card_needs_idle_sample(driver: Option<&str>) -> bool {
    matches!(driver, Some("i915") | Some("xe"))
}

fn is_nvidia_smi_driver(driver: Option<&str>) -> bool {
    matches!(driver, Some("nvidia") | Some("nvidia-drm"))
}

fn read_drm_cards() -> Vec<CardSnap> {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut cards = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if card_index(&name).is_none() {
            continue;
        }
        let base = entry.path();
        let device_key = std::fs::canonicalize(base.join("device"))
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| format!("unresolved:{name}"));
        let driver = std::fs::read_link(base.join("device/driver"))
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            });
        let busy_text = if driver.as_deref() == Some("amdgpu") {
            std::fs::read_to_string(base.join("device/gpu_busy_percent")).ok()
        } else {
            None
        };
        let idle_path = driver
            .as_deref()
            .and_then(|driver| first_existing_idle(&base, driver));
        cards.push(CardSnap {
            name,
            device_key,
            vendor_text: std::fs::read_to_string(base.join("device/vendor")).ok(),
            driver,
            busy_text,
            idle_before_ms: None,
            idle_after_ms: None,
            wall_ms: 0,
            idle_path,
        });
    }
    cards
}

fn card_index(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("card")?;
    if rest.is_empty() || !rest.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

/// gt0 is the render engine. `power/rc6_residency_ms` is the older single-GT
/// i915 ABI, used only when the per-GT file is absent.
fn idle_counter_candidates(driver: &str) -> &'static [&'static str] {
    match driver {
        "i915" => &["gt/gt0/rc6_residency_ms", "power/rc6_residency_ms"],
        "xe" => &["device/tile0/gt0/gtidle/idle_residency_ms"],
        _ => &[],
    }
}

fn first_existing_idle(card: &Path, driver: &str) -> Option<PathBuf> {
    idle_counter_candidates(driver)
        .iter()
        .map(|relative| card.join(relative))
        .find(|path| path.is_file())
}

fn read_idle_counter(card: &CardSnap) -> Option<u64> {
    let path = card.idle_path.as_ref()?;
    let text = std::fs::read_to_string(path).ok()?;
    parse_u64_token(&text)
}

fn parse_u64_token(text: &str) -> Option<u64> {
    text.split_whitespace().next()?.parse().ok()
}

fn parse_percent(text: &str) -> Option<u32> {
    let value = parse_u64_token(text)?;
    u32::try_from(value).ok().filter(|value| *value <= 100)
}

fn parse_vendor_id(text: &str) -> Option<u32> {
    let token = text.split_whitespace().next()?;
    let hex = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))?;
    u32::from_str_radix(hex, 16).ok()
}

fn vendor_label(id: u32) -> String {
    match id {
        0x1002 => "amd".to_string(),
        0x8086 => "intel".to_string(),
        0x10de => "nvidia".to_string(),
        other => format!("0x{other:04x}"),
    }
}

fn parse_nvidia_smi(text: &str) -> Vec<Option<u32>> {
    let mut values = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((_, util)) = line.split_once(',') else {
            continue;
        };
        let util = util.trim().trim_matches(|ch| ch == '[' || ch == ']');
        if util.eq_ignore_ascii_case("n/a") {
            values.push(None);
            continue;
        }
        values.push(parse_percent(util));
    }
    values
}

/// RC6 and Xe GT-idle are cumulative idle time. Busy is the part of the
/// sample window the counter did not claim. A counter that goes backwards
/// is unreadable, not zero.
fn busy_from_idle_ms(before: u64, after: u64, wall_ms: u64) -> Option<u32> {
    if wall_ms == 0 {
        return None;
    }
    let idle = after.checked_sub(before)?;
    if idle >= wall_ms {
        return Some(0);
    }
    let busy = (wall_ms - idle) as f64 * 100.0 / wall_ms as f64;
    if !busy.is_finite() {
        return None;
    }
    let rounded = busy.round();
    if !(0.0..=100.0).contains(&rounded) {
        return None;
    }
    Some(rounded as u32)
}

fn gpus_from_cards(mut cards: Vec<CardSnap>, nvidia_smi: &str) -> Vec<GpuCard> {
    cards.sort_by_key(|card| card_index(&card.name).unwrap_or(u32::MAX));
    let nvidia = parse_nvidia_smi(nvidia_smi);
    let mut nvidia_index = 0usize;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for card in cards {
        if card_index(&card.name).is_none() {
            continue;
        }
        if !seen.insert(card.device_key.clone()) {
            continue;
        }
        let vendor = card
            .vendor_text
            .as_deref()
            .and_then(parse_vendor_id)
            .map_or_else(|| "n/a".to_string(), vendor_label);
        let percent = match card.driver.as_deref() {
            Some("amdgpu") => card.busy_text.as_deref().and_then(parse_percent),
            Some("i915") | Some("xe") => match (card.idle_before_ms, card.idle_after_ms) {
                (Some(before), Some(after)) => busy_from_idle_ms(before, after, card.wall_ms),
                _ => None,
            },
            Some(driver) if is_nvidia_smi_driver(Some(driver)) => {
                let percent = nvidia.get(nvidia_index).copied().flatten();
                nvidia_index += 1;
                percent
            }
            _ => None,
        };
        out.push(GpuCard { vendor, percent });
    }
    out
}

/// Format the first line matching subagentcounter exactly.
pub fn format_load_line(metrics: &SystemMetrics) -> String {
    let cpu_text = metrics
        .cpu_percent
        .map(|c| format!("{:.0}%", c))
        .unwrap_or_else(|| "?%".to_string());

    let load_text = metrics
        .load_1m
        .map(|l| format!("{:.1}", l))
        .unwrap_or_else(|| "?".to_string());

    let cores_text = metrics
        .cores
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".to_string());

    let ram_text = match (metrics.ram_used_bytes, metrics.ram_total_bytes) {
        (Some(used), Some(total)) if total > 0 => {
            let used_gb = used as f64 / (1024.0 * 1024.0 * 1024.0);
            let total_gb = total as f64 / (1024.0 * 1024.0 * 1024.0);
            let pct = ((used as f64 * 100.0) / total as f64).round() as u64;
            let mut s = format!("{:.1} / {:.1} GB ({}%)", used_gb, total_gb, pct);
            if let Some(swap) = metrics.swap_used_bytes {
                if swap > 0 {
                    let swap_gb = swap as f64 / (1024.0 * 1024.0 * 1024.0);
                    s.push_str(&format!(", swap {:.1} GB", swap_gb));
                }
            }
            s
        }
        _ => "?".to_string(),
    };

    let mut line = format!(
        "CPU {} (load {} / {} cores) | RAM {}",
        cpu_text, load_text, cores_text, ram_text
    );
    if let Some(gpu) = format_gpu(&metrics.gpu) {
        line.push_str(" | ");
        line.push_str(&gpu);
    }
    line
}

fn format_gpu(report: &GpuReport) -> Option<String> {
    match report {
        GpuReport::Mac(percent) => {
            let text = match percent {
                Some(value) => format!("{value}%"),
                None => "?%".to_string(),
            };
            Some(format!("GPU {text}"))
        }
        GpuReport::Linux(cards) => {
            if cards.is_empty() {
                return None;
            }
            let parts: Vec<String> = cards
                .iter()
                .map(|card| match card.percent {
                    Some(value) => format!("{} {value}%", card.vendor),
                    None => format!("{} n/a", card.vendor),
                })
                .collect();
            Some(format!("GPU {}", parts.join(" · ")))
        }
    }
}

/// Running crew agents per provider. A run is exactly one `crew <provider>`
/// process: detached jobs are supervised by `crew --supervise <name>`, which
/// spawns that same process, so supervisors and the provider CLI's own
/// children never count a run twice.
pub fn find_agent_pids(processes: &HashMap<u32, ProcessRecord>) -> HashMap<&'static str, Vec<u32>> {
    let mut matches: HashMap<&'static str, Vec<u32>> = PROVIDERS
        .iter()
        .map(|&(name, _)| (name, Vec::new()))
        .collect();

    for record in processes.values() {
        let is_crew = Path::new(&record.executable)
            .file_name()
            .is_some_and(|n| n == "crew");
        if !is_crew {
            continue;
        }
        if let Some(&(name, _)) = PROVIDERS
            .iter()
            .find(|(_, names)| names.contains(&record.arg1.as_str()))
        {
            matches.get_mut(name).unwrap().push(record.pid);
        }
    }

    for pids in matches.values_mut() {
        pids.sort_unstable();
    }

    matches
}

/// One line per provider that has running crew agents, in print order. Idle
/// providers are left out so the output lists only work that is in flight.
pub fn format_provider_lines(agent_pids: &HashMap<&'static str, Vec<u32>>) -> Vec<String> {
    PROVIDERS
        .iter()
        .filter_map(|&(provider, _)| {
            let pids = &agent_pids[provider];
            if pids.is_empty() {
                return None;
            }
            let pid_strs: Vec<String> = pids.iter().map(|p| p.to_string()).collect();
            Some(format!(
                "{}: {} | {}",
                provider,
                pids.len(),
                pid_strs.join("; ")
            ))
        })
        .collect()
}

/// Print the machine line, then the provider lines for agents that are running.
pub fn print_load() -> Result<i32, Error> {
    let ps_output = run_cmd("ps", &["-axww", "-o", "pid=,ppid=,%cpu=,command="]);
    let (processes, cpu_sum) = parse_ps_snapshot(&ps_output);
    let metrics = collect_metrics(cpu_sum, !processes.is_empty());

    println!("{}", format_load_line(&metrics));

    for line in format_provider_lines(&find_agent_pids(&processes)) {
        println!("{}", line);
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_load_line_with_swap() {
        let metrics = SystemMetrics {
            cpu_percent: Some(12.4),
            load_1m: Some(2.4),
            cores: Some(10),
            // 17_440_000_000 bytes = 16.242 GiB (formats as 16.2 GB, 67.7% -> 68%)
            ram_used_bytes: Some(17_440_000_000),
            ram_total_bytes: Some((24.0 * 1024.0 * 1024.0 * 1024.0) as u64),
            gpu: GpuReport::Mac(Some(4)),
            swap_used_bytes: Some((0.5 * 1024.0 * 1024.0 * 1024.0) as u64),
        };

        let line = format_load_line(&metrics);
        assert_eq!(
            line,
            "CPU 12% (load 2.4 / 10 cores) | RAM 16.2 / 24.0 GB (68%), swap 0.5 GB | GPU 4%"
        );
    }

    #[test]
    fn format_load_line_without_swap() {
        let metrics = SystemMetrics {
            cpu_percent: Some(5.0),
            load_1m: Some(1.5),
            cores: Some(8),
            ram_used_bytes: Some((8.0 * 1024.0 * 1024.0 * 1024.0) as u64),
            ram_total_bytes: Some((16.0 * 1024.0 * 1024.0 * 1024.0) as u64),
            gpu: GpuReport::Mac(Some(0)),
            swap_used_bytes: Some(0),
        };

        let line = format_load_line(&metrics);
        assert_eq!(
            line,
            "CPU 5% (load 1.5 / 8 cores) | RAM 8.0 / 16.0 GB (50%) | GPU 0%"
        );
    }

    #[test]
    fn ps_parsing_and_agent_detection() {
        let sample = "\
100 1 2.0 /bin/zsh -c crew claude task\n\
101 100 5.0 /Users/vladko/.cargo/bin/crew claude -m sonnet -\n\
102 101 1.0 claude --output-format json\n\
103 1 0.5 crew --supervise demo\n\
104 103 0.5 /repo/target/debug/crew agy -\n\
105 1 0.5 crew gpt -\n\
200 1 10.0 crew load\n\
";
        let (procs, cpu_sum) = parse_ps_snapshot(sample);
        assert_eq!(procs.len(), 7);
        assert!((cpu_sum - 19.5).abs() < 1e-6);

        let agents = find_agent_pids(&procs);
        assert_eq!(agents["claude"], vec![101]);
        assert_eq!(agents["gemini"], vec![104]);
        assert_eq!(agents["grok"], Vec::<u32>::new());
        assert_eq!(agents["codex"], vec![105]);
    }

    #[test]
    fn provider_lines_skip_idle_providers() {
        let mut agents = find_agent_pids(&HashMap::new());
        assert!(format_provider_lines(&agents).is_empty());

        agents.get_mut("gemini").unwrap().push(104);
        assert_eq!(format_provider_lines(&agents), vec!["gemini: 1 | 104"]);
    }

    #[test]
    fn linux_proc_fixtures() {
        assert_eq!(load_1m_from_loadavg("1.25 0.50 0.40 2/100 9\n"), Some(1.25));
        assert_eq!(load_1m_from_loadavg("\n"), None);

        let cpuinfo = "processor\t: 0\nprocessor\t: 1\nvendor_id\t: GenuineIntel\n";
        assert_eq!(cores_from_cpuinfo(cpuinfo), Some(2));
        assert_eq!(cores_from_cpuinfo("vendor_id\t: none\n"), None);

        let meminfo = "\
MemTotal:       1000000 kB
MemFree:          10000 kB
MemAvailable:    250000 kB
SwapTotal:       200000 kB
SwapFree:         50000 kB
";
        let parsed = parse_meminfo(meminfo).expect("meminfo");
        assert_eq!(parsed.total_bytes, 1_000_000 * 1024);
        assert_eq!(parsed.used_bytes, 750_000 * 1024);
        assert_eq!(parsed.swap_used_bytes, Some(150_000 * 1024));

        let no_swap = "MemTotal: 1000 kB\nMemAvailable: 400 kB\n";
        assert_eq!(parse_meminfo(no_swap).expect("ram").swap_used_bytes, None);
        assert!(parse_meminfo("MemTotal: 1000 kB\n").is_none());
    }

    fn card(name: &str, device: &str, vendor: &str, driver: Option<&str>) -> CardSnap {
        CardSnap {
            name: name.to_string(),
            device_key: device.to_string(),
            vendor_text: Some(vendor.to_string()),
            driver: driver.map(str::to_string),
            busy_text: None,
            idle_path: None,
            idle_before_ms: None,
            idle_after_ms: None,
            wall_ms: 0,
        }
    }

    fn with_idle(mut card: CardSnap, before: u64, after: u64, wall: u64) -> CardSnap {
        card.idle_before_ms = Some(before);
        card.idle_after_ms = Some(after);
        card.wall_ms = wall;
        card
    }

    fn with_busy(mut card: CardSnap, text: &str) -> CardSnap {
        card.busy_text = Some(text.to_string());
        card
    }

    #[test]
    fn linux_gpu_fixtures() {
        assert_eq!(
            idle_counter_candidates("i915"),
            ["gt/gt0/rc6_residency_ms", "power/rc6_residency_ms"]
        );
        assert_eq!(
            idle_counter_candidates("xe"),
            ["device/tile0/gt0/gtidle/idle_residency_ms"]
        );
        assert!(idle_counter_candidates("nouveau").is_empty());
        assert_eq!(busy_from_idle_ms(1_000, 1_100, 200), Some(50));
        assert_eq!(busy_from_idle_ms(1_100, 1_000, 200), None);
        assert_eq!(busy_from_idle_ms(0, 0, 0), None);
        assert_eq!(busy_from_idle_ms(0, 500, 200), Some(0));

        let cards = vec![
            with_idle(
                card("card2", "pci-intel", "0x8086\n", Some("i915")),
                9,
                9,
                200,
            ),
            with_idle(
                card("card0", "pci-intel", "0x8086", Some("i915")),
                1_000,
                1_100,
                200,
            ),
            card("card0-HDMI-A-1", "pci-intel", "0x8086", Some("i915")),
            card("card3", "pci-nv0", "0x10de", Some("nvidia")),
            card("card4", "pci-nv1", "0x10DE", Some("nvidia-drm")),
            card("card5", "pci-nv2", "0x10de", Some("nouveau")),
            with_busy(card("card6", "pci-amd", "0x1002", Some("amdgpu")), "7\n"),
            card("card7", "pci-other", "0x1234", None),
            with_busy(
                card("card8", "pci-amd-bad", "0x1002", Some("amdgpu")),
                "nope",
            ),
            with_idle(card("card9", "pci-xe", "0x8086", Some("xe")), 0, 50, 200),
        ];
        let smi = "0, 81\n1, [N/A]\n";
        let gpus = gpus_from_cards(cards, smi);
        assert_eq!(
            gpus,
            vec![
                GpuCard {
                    vendor: "intel".to_string(),
                    percent: Some(50)
                },
                GpuCard {
                    vendor: "nvidia".to_string(),
                    percent: Some(81)
                },
                GpuCard {
                    vendor: "nvidia".to_string(),
                    percent: None
                },
                GpuCard {
                    vendor: "nvidia".to_string(),
                    percent: None
                },
                GpuCard {
                    vendor: "amd".to_string(),
                    percent: Some(7)
                },
                GpuCard {
                    vendor: "0x1234".to_string(),
                    percent: None
                },
                GpuCard {
                    vendor: "amd".to_string(),
                    percent: None
                },
                GpuCard {
                    vendor: "intel".to_string(),
                    percent: Some(75)
                },
            ]
        );

        let metrics = SystemMetrics {
            cpu_percent: Some(1.2),
            load_1m: Some(0.4),
            cores: Some(4),
            ram_used_bytes: Some(1024 * 1024 * 1024),
            ram_total_bytes: Some(2 * 1024 * 1024 * 1024),
            gpu: GpuReport::Linux(gpus),
            swap_used_bytes: None,
        };
        assert_eq!(
            format_load_line(&metrics),
            "CPU 1% (load 0.4 / 4 cores) | RAM 1.0 / 2.0 GB (50%) | GPU intel 50% · nvidia 81% · nvidia n/a · nvidia n/a · amd 7% · 0x1234 n/a · amd n/a · intel 75%"
        );

        let mut absent = metrics;
        absent.gpu = GpuReport::Linux(Vec::new());
        assert_eq!(
            format_load_line(&absent),
            "CPU 1% (load 0.4 / 4 cores) | RAM 1.0 / 2.0 GB (50%)"
        );
    }
}
