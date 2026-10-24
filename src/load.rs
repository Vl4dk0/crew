#![allow(clippy::collapsible_if)]

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

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
    pub gpu_percent: Option<u32>,
    pub swap_used_bytes: Option<u64>,
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

/// The `ps` view `crew load` counts from. Update uses the same snapshot so
/// "running" means the same processes the load line would list.
fn process_snapshot() -> (HashMap<u32, ProcessRecord>, f64) {
    let ps_output = run_cmd("ps", &["-axww", "-o", "pid=,ppid=,%cpu=,command="]);
    parse_ps_snapshot(&ps_output)
}

/// Agents `crew load` would list. Replacing a binary under one of these
/// processes can break that run, so update refuses the provider.
pub fn running_agents() -> HashMap<&'static str, Vec<u32>> {
    let (processes, _) = process_snapshot();
    find_agent_pids(&processes)
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
            metrics.gpu_percent = Some(max_gpu);
        }
    } else {
        // Fallback for Linux / other OS
        if let Ok(loadavg) = std::fs::read_to_string("/proc/loadavg") {
            if let Some(first) = loadavg.split_whitespace().next() {
                metrics.load_1m = first.parse().ok();
            }
        }
        if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
            let mut total = 0u64;
            let mut available = 0u64;
            for line in meminfo.lines() {
                if line.starts_with("MemTotal:") {
                    total = extract_kb(line) * 1024;
                } else if line.starts_with("MemAvailable:") {
                    available = extract_kb(line) * 1024;
                }
            }
            if total > 0 {
                metrics.ram_total_bytes = Some(total);
                metrics.ram_used_bytes = Some(total.saturating_sub(available));
            }
        }
        metrics.cores = std::thread::available_parallelism().ok().map(|n| n.get());
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

fn extract_kb(line: &str) -> u64 {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() >= 2 {
        parts[1].parse().unwrap_or(0)
    } else {
        0
    }
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

    let gpu_text = metrics
        .gpu_percent
        .map(|g| format!("{}%", g))
        .unwrap_or_else(|| "?%".to_string());

    format!(
        "CPU {} (load {} / {} cores) | RAM {} | GPU {}",
        cpu_text, load_text, cores_text, ram_text, gpu_text
    )
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
    let (processes, cpu_sum) = process_snapshot();
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
            gpu_percent: Some(4),
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
            gpu_percent: Some(0),
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
}
