use std::collections::{HashMap, HashSet};
use std::fs::{self, DirEntry};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::error::Error;

/// Providers in print order, with the subcommand names that select each.
const PROVIDERS: [(&str, &[&str]); 5] = [
    ("claude", &["claude"]),
    ("gemini", &["gemini", "agy", "antigravity"]),
    ("grok", &["grok"]),
    ("codex", &["codex", "gpt", "chatgpt"]),
    ("pi", &["pi"]),
];

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

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
/// fixture tests can feed file contents directly. `idle_path` is fixed before
/// the sample so both residency reads hit the same file.
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
    pub cpu: f64,
    pub executable: String,
    pub arg1: String,
}

/// A missing tool or a failed probe leaves that reading blank. `crew load`
/// still prints the rest.
fn run_cmd(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default()
}

/// Rows from `ps -axww -o pid=,ppid=,%cpu=,command=`. A row that does not
/// parse is ignored so one bad line cannot shift the columns of the next.
pub fn parse_ps_snapshot(output: &str) -> (HashMap<u32, ProcessRecord>, f64) {
    let mut processes = HashMap::new();
    let mut cpu_sum = 0.0;
    for line in output.lines() {
        let Some(record) = parse_ps_line(line) else {
            continue;
        };
        cpu_sum += record.cpu;
        processes.insert(record.pid, record);
    }
    (processes, cpu_sum)
}

fn parse_ps_line(line: &str) -> Option<ProcessRecord> {
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse().ok()?;
    // `ppid` is only here to keep the columns aligned. Parentage is not how a
    // run is counted; see `provider_of`.
    parts.next().filter(|token| token.parse::<u32>().is_ok())?;
    let cpu = parts.next()?.parse().ok()?;
    let executable = parts.next()?.to_string();
    Some(ProcessRecord {
        pid,
        cpu,
        executable,
        arg1: parts.next().unwrap_or("").to_string(),
    })
}

/// CPU percent is the sum of per-process `%cpu` divided by logical cores, so
/// the headline matches the `ps` snapshot. Anything a probe could not read
/// stays empty and the line shows `?`.
pub fn collect_metrics(cpu_sum: f64, has_processes: bool) -> SystemMetrics {
    let mut metrics = SystemMetrics::default();
    if cfg!(target_os = "macos") {
        fill_mac_metrics(&mut metrics);
    } else if cfg!(target_os = "linux") {
        fill_linux_metrics(&mut metrics);
    }
    metrics.cpu_percent = cpu_percent(cpu_sum, metrics.cores, has_processes);
    metrics
}

fn cpu_percent(cpu_sum: f64, cores: Option<usize>, has_processes: bool) -> Option<f64> {
    if !has_processes {
        return None;
    }
    let cores = cores.filter(|count| *count > 0)?;
    Some(cpu_sum / cores as f64)
}

fn fill_mac_metrics(metrics: &mut SystemMetrics) {
    apply_sysctl(
        metrics,
        &run_cmd(
            "sysctl",
            &["-n", "hw.ncpu", "vm.loadavg", "hw.memsize", "vm.swapusage"],
        ),
    );
    apply_vm_stat(metrics, &run_cmd("vm_stat", &[]));
    if let Some(percent) = max_accelerator_util(&run_cmd(
        "ioreg",
        &["-r", "-d", "1", "-w", "0", "-c", "IOAccelerator"],
    )) {
        metrics.gpu = GpuReport::Mac(Some(percent));
    }
}

fn apply_sysctl(metrics: &mut SystemMetrics, text: &str) {
    let mut lines = text.lines();
    metrics.cores = lines.next().and_then(|line| line.trim().parse().ok());
    metrics.load_1m = lines.next().and_then(|line| {
        line.split_whitespace()
            .find(|token| !token.starts_with('{'))?
            .parse()
            .ok()
    });
    metrics.ram_total_bytes = lines.next().and_then(|line| line.trim().parse().ok());
    metrics.swap_used_bytes = lines.next().and_then(swap_used_from_sysctl);
}

/// `vm.swapusage` looks like `used = 512.00M`. The suffix is the unit.
fn swap_used_from_sysctl(line: &str) -> Option<u64> {
    let token = line.split_once("used = ")?.1.split_whitespace().next()?;
    let mut chars = token.chars();
    let suffix = chars.next_back()?;
    let scale = match suffix {
        'K' => 1024.0,
        'M' => 1024.0 * 1024.0,
        'G' => GIB,
        _ => 1.0,
    };
    let value: f64 = chars.as_str().parse().ok()?;
    Some((value * scale) as u64)
}

fn apply_vm_stat(metrics: &mut SystemMetrics, text: &str) {
    let mut page_size = 4096u64;
    let mut anonymous = None;
    let mut purgeable = None;
    let mut wired = None;
    let mut compressor = None;
    for line in text.lines() {
        if let Some(size) = page_size_of(line) {
            page_size = size;
        }
        if line.starts_with("Anonymous pages:") {
            anonymous = vm_count(line);
        } else if line.starts_with("Pages purgeable:") {
            purgeable = vm_count(line);
        } else if line.starts_with("Pages wired down:") {
            wired = vm_count(line);
        } else if line.starts_with("Pages occupied by compressor:") {
            compressor = vm_count(line);
        }
    }
    if let (Some(anon), Some(purge), Some(wire), Some(comp)) =
        (anonymous, purgeable, wired, compressor)
    {
        // App memory is anonymous pages that are not also purgeable, plus
        // wired and compressor pages. That matches Activity Monitor's "Memory Used".
        let used_pages = anon.saturating_sub(purge) + wire + comp;
        metrics.ram_used_bytes = Some(used_pages * page_size);
    }
}

fn page_size_of(line: &str) -> Option<u64> {
    let rest = line.split_once("page size of ")?.1;
    rest.split_once(" bytes")?.0.trim().parse().ok()
}

fn vm_count(line: &str) -> Option<u64> {
    line.split_once(':')?
        .1
        .trim()
        .trim_end_matches('.')
        .parse()
        .ok()
}

/// Several accelerators can report at once. The line shows the busiest one.
fn max_accelerator_util(ioreg: &str) -> Option<u32> {
    let target = "\"Device Utilization %\"";
    let mut max_gpu = 0u32;
    let mut found = false;
    let mut rest = ioreg;
    while let Some(pos) = rest.find(target) {
        rest = &rest[pos + target.len()..];
        let Some(after_eq) = rest.split_once('=').map(|(_, value)| value) else {
            continue;
        };
        let digits: String = after_eq
            .chars()
            .skip_while(|ch| ch.is_whitespace())
            .take_while(|ch| ch.is_ascii_digit())
            .collect();
        if let Ok(value) = digits.parse() {
            max_gpu = max_gpu.max(value);
            found = true;
        }
    }
    found.then_some(max_gpu)
}

fn fill_linux_metrics(metrics: &mut SystemMetrics) {
    if let Ok(text) = fs::read_to_string("/proc/loadavg") {
        metrics.load_1m = load_1m_from_loadavg(&text);
    }
    if let Ok(text) = fs::read_to_string("/proc/cpuinfo") {
        metrics.cores = cores_from_cpuinfo(&text);
    }
    if let Ok(text) = fs::read_to_string("/proc/meminfo")
        && let Some(mem) = parse_meminfo(&text)
    {
        metrics.ram_total_bytes = Some(mem.total_bytes);
        metrics.ram_used_bytes = Some(mem.used_bytes);
        metrics.swap_used_bytes = mem.swap_used_bytes;
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
    let count = text
        .lines()
        .filter(|line| {
            line.split_once(':')
                .is_some_and(|(key, _)| key.trim() == "processor")
        })
        .count();
    if count == 0 { None } else { Some(count) }
}

/// `MemAvailable` is the kernel's own estimate of memory that can be used
/// without swapping. Used is what is not in that pool. A broken swap figure
/// must not throw away the RAM reading.
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
    let total_bytes = total_kb?.checked_mul(1024)?;
    let available_bytes = available_kb?.checked_mul(1024)?;
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
    sample_idle(&mut cards);
    let nvidia_smi = if cards
        .iter()
        .any(|card| card.driver.as_deref().is_some_and(is_nvidia_smi_driver))
    {
        // Sysfs has no utilization for the proprietary driver.
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

fn is_nvidia_smi_driver(driver: &str) -> bool {
    matches!(driver, "nvidia" | "nvidia-drm")
}

fn read_drm_cards() -> Vec<CardSnap> {
    let Ok(entries) = fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| card_snap(&entry))
        .collect()
}

fn card_snap(entry: &DirEntry) -> Option<CardSnap> {
    let name = entry.file_name().to_string_lossy().into_owned();
    // Connector nodes (`card0-HDMI-A-1`) are not separate GPUs.
    card_index(&name)?;
    let base = entry.path();
    let device_key = fs::canonicalize(base.join("device"))
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| format!("unresolved:{name}"));
    let driver = fs::read_link(base.join("device/driver"))
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });
    let busy_text = (driver.as_deref() == Some("amdgpu"))
        .then(|| fs::read_to_string(base.join("device/gpu_busy_percent")).ok())
        .flatten();
    let idle_path = driver
        .as_deref()
        .and_then(|driver| first_existing_idle(&base, driver));
    Some(CardSnap {
        name,
        device_key,
        vendor_text: fs::read_to_string(base.join("device/vendor")).ok(),
        driver,
        busy_text,
        idle_path,
        idle_before_ms: None,
        idle_after_ms: None,
        wall_ms: 0,
    })
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

/// RC6 and Xe GT-idle tick in milliseconds. The window has to be long enough
/// for the counter to move and short enough that `crew load` stays quick.
fn sample_idle(cards: &mut [CardSnap]) {
    let needs_sample = cards.iter().any(|card| {
        card.driver
            .as_deref()
            .is_some_and(|driver| !idle_counter_candidates(driver).is_empty())
    });
    if !needs_sample {
        return;
    }
    let started = Instant::now();
    let befores: Vec<Option<u64>> = cards.iter().map(read_idle_counter).collect();
    thread::sleep(Duration::from_millis(200));
    let wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(0);
    for (card, before) in cards.iter_mut().zip(befores) {
        card.idle_before_ms = before;
        card.idle_after_ms = read_idle_counter(card);
        card.wall_ms = wall_ms;
    }
}

fn read_idle_counter(card: &CardSnap) -> Option<u64> {
    let text = fs::read_to_string(card.idle_path.as_ref()?).ok()?;
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
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let util = line.split_once(',')?.1;
            let util = util.trim().trim_matches(|ch| ch == '[' || ch == ']');
            Some(if util.eq_ignore_ascii_case("n/a") {
                None
            } else {
                parse_percent(util)
            })
        })
        .collect()
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
    let rounded = ((wall_ms - idle) as f64 * 100.0 / wall_ms as f64).round();
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
        // Several DRM nodes can name one GPU. The lowest card index wins.
        if !seen.insert(card.device_key.clone()) {
            continue;
        }
        let vendor = card
            .vendor_text
            .as_deref()
            .and_then(parse_vendor_id)
            .map(vendor_label)
            .unwrap_or_else(|| "n/a".to_string());
        out.push(GpuCard {
            vendor,
            percent: card_percent(&card, &nvidia, &mut nvidia_index),
        });
    }
    out
}

fn card_percent(card: &CardSnap, nvidia: &[Option<u32>], nvidia_index: &mut usize) -> Option<u32> {
    match card.driver.as_deref() {
        Some("amdgpu") => card.busy_text.as_deref().and_then(parse_percent),
        Some(driver) if !idle_counter_candidates(driver).is_empty() => {
            busy_from_idle_ms(card.idle_before_ms?, card.idle_after_ms?, card.wall_ms)
        }
        Some(driver) if is_nvidia_smi_driver(driver) => {
            let percent = nvidia.get(*nvidia_index).copied().flatten();
            *nvidia_index += 1;
            percent
        }
        _ => None,
    }
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / GIB
}

/// The first printed line. The shape stays fixed so two machines can be
/// compared by eye. Missing readings are `?`, not a dropped field.
pub fn format_load_line(metrics: &SystemMetrics) -> String {
    let cpu = match metrics.cpu_percent {
        Some(value) => format!("{value:.0}%"),
        None => "?%".to_string(),
    };
    let load = match metrics.load_1m {
        Some(value) => format!("{value:.1}"),
        None => "?".to_string(),
    };
    let cores = match metrics.cores {
        Some(value) => value.to_string(),
        None => "?".to_string(),
    };
    let mut line = format!(
        "CPU {cpu} (load {load} / {cores} cores) | RAM {}",
        format_ram(metrics)
    );
    if let Some(gpu) = format_gpu(&metrics.gpu) {
        line.push_str(" | ");
        line.push_str(&gpu);
    }
    line
}

fn format_ram(metrics: &SystemMetrics) -> String {
    let (Some(used), Some(total)) = (metrics.ram_used_bytes, metrics.ram_total_bytes) else {
        return "?".to_string();
    };
    if total == 0 {
        return "?".to_string();
    }
    let pct = ((used as f64 * 100.0) / total as f64).round() as u64;
    let mut text = format!("{:.1} / {:.1} GB ({pct}%)", gib(used), gib(total));
    if let Some(swap) = metrics.swap_used_bytes.filter(|swap| *swap > 0) {
        text.push_str(&format!(", swap {:.1} GB", gib(swap)));
    }
    text
}

fn format_gpu(report: &GpuReport) -> Option<String> {
    match report {
        GpuReport::Mac(Some(value)) => Some(format!("GPU {value}%")),
        GpuReport::Mac(None) => Some("GPU ?%".to_string()),
        GpuReport::Linux(cards) if cards.is_empty() => None,
        GpuReport::Linux(cards) => {
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

/// A run is the `crew <provider>` process itself. Detached jobs are supervised
/// by `crew --supervise`, which spawns that same process, so the supervisor
/// and the provider CLI's own children must not count a second time.
pub fn find_agent_pids(processes: &HashMap<u32, ProcessRecord>) -> HashMap<String, Vec<u32>> {
    find_agent_pids_with(processes, &[])
}

fn find_agent_pids_with(
    processes: &HashMap<u32, ProcessRecord>,
    agents: &[String],
) -> HashMap<String, Vec<u32>> {
    let mut matches: HashMap<String, Vec<u32>> = PROVIDERS
        .iter()
        .map(|&(name, _)| (name.to_string(), Vec::new()))
        .collect();
    for name in agents {
        matches.entry(name.clone()).or_default();
    }
    for record in processes.values() {
        let Some(name) = provider_of(record, agents) else {
            continue;
        };
        if let Some(pids) = matches.get_mut(&name) {
            pids.push(record.pid);
        }
    }
    for pids in matches.values_mut() {
        pids.sort_unstable();
    }
    matches
}

fn provider_of(record: &ProcessRecord, agents: &[String]) -> Option<String> {
    let is_crew = Path::new(&record.executable)
        .file_name()
        .is_some_and(|name| name == "crew");
    if !is_crew {
        return None;
    }
    if let Some((name, _)) = PROVIDERS
        .iter()
        .find(|(_, names)| names.contains(&record.arg1.as_str()))
    {
        return Some((*name).to_string());
    }
    agents.iter().find(|name| *name == &record.arg1).cloned()
}

/// One line per provider that has running crew agents, in print order. Idle
/// providers are left out so the output lists only work that is in flight.
pub fn format_provider_lines(agent_pids: &HashMap<String, Vec<u32>>) -> Vec<String> {
    let mut names: Vec<String> = PROVIDERS
        .iter()
        .map(|&(provider, _)| provider.to_string())
        .collect();
    let mut extras: Vec<String> = agent_pids
        .keys()
        .filter(|name| !names.iter().any(|builtin| builtin == *name))
        .cloned()
        .collect();
    extras.sort();
    names.extend(extras);
    names
        .into_iter()
        .filter_map(|provider| {
            let pids = agent_pids.get(&provider)?;
            if pids.is_empty() {
                return None;
            }
            let joined = pids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join("; ");
            Some(format!("{provider}: {} | {joined}", pids.len()))
        })
        .collect()
}

/// Print the machine line, then the provider lines for agents that are running.
pub fn print_load() -> Result<i32, Error> {
    let ps_output = run_cmd("ps", &["-axww", "-o", "pid=,ppid=,%cpu=,command="]);
    let (processes, cpu_sum) = parse_ps_snapshot(&ps_output);
    let metrics = collect_metrics(cpu_sum, !processes.is_empty());
    println!("{}", format_load_line(&metrics));
    let agents = crate::tier::agent_names()?;
    for line in format_provider_lines(&find_agent_pids_with(&processes, &agents)) {
        println!("{line}");
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
            ram_total_bytes: Some((24.0 * GIB) as u64),
            gpu: GpuReport::Mac(Some(4)),
            swap_used_bytes: Some((0.5 * GIB) as u64),
        };
        assert_eq!(
            format_load_line(&metrics),
            "CPU 12% (load 2.4 / 10 cores) | RAM 16.2 / 24.0 GB (68%), swap 0.5 GB | GPU 4%"
        );
    }

    #[test]
    fn format_load_line_without_swap() {
        let metrics = SystemMetrics {
            cpu_percent: Some(5.0),
            load_1m: Some(1.5),
            cores: Some(8),
            ram_used_bytes: Some((8.0 * GIB) as u64),
            ram_total_bytes: Some((16.0 * GIB) as u64),
            gpu: GpuReport::Mac(Some(0)),
            swap_used_bytes: Some(0),
        };
        assert_eq!(
            format_load_line(&metrics),
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

    fn gpu(vendor: &str, percent: Option<u32>) -> GpuCard {
        GpuCard {
            vendor: vendor.to_string(),
            percent,
        }
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
        let gpus = gpus_from_cards(cards, "0, 81\n1, [N/A]\n");
        assert_eq!(
            gpus,
            vec![
                gpu("intel", Some(50)),
                gpu("nvidia", Some(81)),
                gpu("nvidia", None),
                gpu("nvidia", None),
                gpu("amd", Some(7)),
                gpu("0x1234", None),
                gpu("amd", None),
                gpu("intel", Some(75)),
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
