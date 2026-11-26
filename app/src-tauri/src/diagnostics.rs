use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub fn log_path(home: &Path) -> PathBuf {
    home.join("logs").join("accio.log")
}

pub fn log(home: &Path, message: &str) {
    let path = log_path(home);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let _ = writeln!(file, "{now} {message}");
    }
}

pub fn helper_memory(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-ax", "-o", "pid=,ppid=,rss="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let processes: Vec<_> = std::str::from_utf8(&output.stdout)
        .ok()?
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((
                fields.next()?.parse::<u32>().ok()?,
                fields.next()?.parse::<u32>().ok()?,
                fields.next()?.parse::<u64>().ok()?,
            ))
        })
        .collect();
    let mut pending = vec![pid];
    let mut rss = None;
    while let Some(pid) = pending.pop() {
        for &(process, parent, size) in &processes {
            if process == pid {
                rss = Some(rss.unwrap_or(0) + size * 1024);
            } else if parent == pid {
                pending.push(process);
            }
        }
    }
    rss
}
