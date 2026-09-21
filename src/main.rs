use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use std::fs;
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::SystemTime;

#[derive(Parser)]
#[command(
    name = "nix-reaper",
    version,
    about = "Deep-clean a NixOS system: generations, boot entries, GC roots, and non-Nix bloat that plain `nix-collect-garbage` never touches."
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Report what's currently taking up space. Read-only, changes nothing.
    Status,
    /// Deep-clean everything: generations, gc, store optimise, journal, docker/podman,
    /// and stray gcroots. Runs for real by default — pass --dry-run to preview instead.
    Clean(CleanArgs),
}

#[derive(Args)]
struct CleanArgs {
    /// Only print what would happen — don't actually delete/run anything.
    #[arg(long)]
    dry_run: bool,

    /// Keep this many NixOS system profile generations
    #[arg(long, default_value_t = 3)]
    keep_system: u32,

    /// Keep this many per-user (nix-env) profile generations
    #[arg(long, default_value_t = 3)]
    keep_user: u32,

    /// Expire home-manager generations older than this many days (skipped if home-manager isn't found)
    #[arg(long, default_value_t = 30)]
    keep_home_days: u32,

    /// Vacuum the systemd journal down to this size, e.g. 200M. Pass "off" to skip journal cleanup.
    #[arg(long, value_name = "SIZE", default_value = "200M")]
    journal: String,

    /// Only remove root symlinks older than this many days
    #[arg(long, default_value_t = 30)]
    roots_older_than_days: u64,

    /// No longer needed — `clean` already does everything --all used to. Kept as a harmless no-op
    /// so old scripts/aliases that still pass it don't break.
    #[arg(long, hide = true)]
    all: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Status => status(),
        Cmd::Clean(args) => clean(args),
    }
}

// ---------- status ----------

fn status() -> Result<()> {
    let home = dirs_home()?;

    section("Nix store");
    if let Some(out) = run_captured("du", &["-sh", "/nix/store"]) {
        println!("  size on disk: {out}");
    }
    if let Some(out) = run_captured("df", &["-h", "/nix/store"]) {
        println!("{}", indent(&out));
    }

    section("Generations");
    print_generation_count("system profile", "/nix/var/nix/profiles/system");
    let user_profile = format!("{}/.nix-profile", home.display());
    print_generation_count("your user profile", &user_profile);
    if command_exists("home-manager") {
        if let Some(out) = run_captured("home-manager", &["generations"]) {
            let n = out.lines().filter(|l| !l.trim().is_empty()).count();
            println!("  home-manager: {n} generation(s)");
        }
    } else {
        println!("  home-manager: not installed, skipping");
    }

    section("Boot entries");
    match fs::read_dir("/boot/loader/entries") {
        Ok(entries) => {
            let n = entries.filter_map(|e| e.ok()).count();
            println!("  systemd-boot entries: {n} (each pins a kernel+initrd generation alive)");
            println!("  set `boot.loader.systemd-boot.configurationLimit = 5;` (or similar) to stop these piling up");
        }
        Err(_) => {
            println!("  no /boot/loader/entries — probably GRUB");
            println!("  set `boot.loader.grub.configurationLimit = 5;` in your config");
        }
    }

    section("Biggest paths in your current system closure");
    if let Some(out) = run_captured("nix", &["path-info", "-rS", "/run/current-system"]) {
        let mut lines: Vec<&str> = out.lines().collect();
        lines.sort_by_key(|l| {
            l.split_whitespace()
                .last()
                .and_then(|n| n.parse::<i64>().ok())
                .unwrap_or(0)
        });
        lines.reverse();
        for l in lines.into_iter().take(15) {
            println!("  {l}");
        }
    }

    section("Stray build results / dev-shell roots pinning old store paths");
    let roots = find_home_roots(&home);
    if roots.is_empty() {
        println!("  none found under {}", home.display());
    } else {
        for r in &roots {
            println!("  {} ({})", r.display(), describe_age(r));
        }
        println!("  -> `nix-reaper clean` removes the stale ones");
    }

    section("Non-Nix disk hogs");
    if command_exists("docker") {
        if let Some(out) = run_captured("docker", &["system", "df"]) {
            println!("{}", indent(&out));
        }
    }
    if command_exists("podman") {
        if let Some(out) = run_captured("podman", &["system", "df"]) {
            println!("{}", indent(&out));
        }
    }
    if let Some(out) = run_captured("journalctl", &["--disk-usage"]) {
        println!("  {out}");
    }
    let cache_dir = format!("{}/.cache", home.display());
    if let Some(out) = run_captured("du", &["-sh", &cache_dir]) {
        println!("  ~/.cache: {out}");
    }

    println!();
    println!(
        "Run `nix-reaper clean --dry-run` to preview a full cleanup, or `nix-reaper clean` to just run it."
    );
    Ok(())
}

// ---------- clean ----------

fn clean(args: CleanArgs) -> Result<()> {
    let home = dirs_home()?;
    let dry_run = args.dry_run;

    if dry_run {
        println!(
            "{y}DRY RUN{r} — nothing will actually change. Drop --dry-run once this plan looks right.\n",
            y = yellow(),
            r = reset()
        );
    }

    let before_size = run_captured("du", &["-sh", "/nix/store"]);

    // 1. system profile generations (root-owned, needs sudo)
    let keep_system_flag = format!("+{}", args.keep_system);
    let why_system = format!("keep the {} newest system generations", args.keep_system);
    step(
        dry_run,
        true,
        "nix-env",
        &[
            "-p",
            "/nix/var/nix/profiles/system",
            "--delete-generations",
            keep_system_flag.as_str(),
        ],
        &why_system,
        false,
    );

    // 2. your own user profile generations
    let keep_user_flag = format!("+{}", args.keep_user);
    let why_user = format!("keep the {} newest generations of your own user profile", args.keep_user);
    step(
        dry_run,
        false,
        "nix-env",
        &["--delete-generations", keep_user_flag.as_str()],
        &why_user,
        false,
    );

    // 3. home-manager generations, if present
    if command_exists("home-manager") {
        let hm_flag = format!("-{} days", args.keep_home_days);
        let why_hm = format!("expire home-manager generations older than {} days", args.keep_home_days);
        step(dry_run, false, "home-manager", &["expire-generations", hm_flag.as_str()], &why_hm, false);
    }

    // 4. actual gc — nix logs "finding garbage collector roots...", "deleting garbage...",
    // and every "deleting '/nix/store/...'" line to STDERR, not stdout, so we have to
    // capture and filter both streams or the deleting lines sail straight through.
    step(
        dry_run,
        false,
        "nix-collect-garbage",
        &["-d"],
        "delete everything no longer reachable from a live generation (add sudo yourself if this errors on permissions)",
        true,
    );

    // 5. optimise
    step(
        dry_run,
        false,
        "nix",
        &["store", "optimise"],
        "hardlink duplicate files inside the store (saves space, deletes nothing reachable)",
        false,
    );

    // 6. journal
    let journal_off = matches!(args.journal.to_lowercase().as_str(), "off" | "none" | "skip");
    if !journal_off {
        let flag = format!("--vacuum-size={}", args.journal);
        let why_journal = format!("shrink the systemd journal down to {}", args.journal);
        step(dry_run, true, "journalctl", &[flag.as_str()], &why_journal, false);
    }

    // 7. docker / podman
    if command_exists("docker") {
        step(
            dry_run,
            false,
            "docker",
            &["system", "prune", "-af", "--volumes"],
            "remove unused docker images, stopped containers, and unused volumes",
            false,
        );
    }
    if command_exists("podman") {
        step(
            dry_run,
            false,
            "podman",
            &["system", "prune", "-af", "--volumes"],
            "remove unused podman images, stopped containers, and unused volumes",
            false,
        );
    }

    // 8. stray build result / dev-shell gcroots under $HOME
    clean_roots(&home, dry_run, args.roots_older_than_days);

    if !dry_run {
        if let Some(before) = before_size {
            if let Some(after) = run_captured("du", &["-sh", "/nix/store"]) {
                println!(
                    "\n{b}nix store size:{r} {before} -> {g}{after}{r}",
                    b = bold(),
                    g = green(),
                    r = reset()
                );
            }
        }
    } else {
        println!(
            "\n{d}Looks right? Re-run the same command without --dry-run to actually do it.{r}",
            d = dim(),
            r = reset()
        );
    }

    Ok(())
}

/// Finds symlinks under `home` that show up as (or point to) live Nix GC roots —
/// this catches `nix build` `result*` links and nix-direnv `.direnv/*` profiles,
/// which otherwise keep entire closures alive forever even after you stop caring
/// about the project.
fn find_home_roots(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Some(text) = run_captured("nix-store", &["--gc", "--print-roots"]) else {
        return out;
    };
    let home_str = home.to_string_lossy().to_string();
    for line in text.lines() {
        let Some((a, b)) = line.split_once(" -> ") else {
            continue;
        };
        for side in [a.trim(), b.trim()] {
            if side.starts_with("/proc/") {
                continue;
            }
            if side.starts_with("/nix/var/nix/gcroots") {
                continue;
            }
            if side.starts_with("/nix/store") {
                continue;
            }
            if side.starts_with(&home_str) {
                out.push(PathBuf::from(side));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn clean_roots(home: &Path, dry_run: bool, cutoff_days: u64) {
    let roots = find_home_roots(home);
    if roots.is_empty() {
        println!("[roots] none found under {}", home.display());
        return;
    }
    for r in &roots {
        let age_days = fs::symlink_metadata(r)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .map(|d| d.as_secs() / 86400)
            .unwrap_or(u64::MAX);

        if age_days < cutoff_days {
            continue;
        }

        if dry_run {
            println!(
                "{y}[dry-run]{r} rm {}   ({age_days}d old — project directory stays, only the pinning symlink goes)",
                r.display(),
                y = yellow(),
                r = reset()
            );
            continue;
        }

        match fs::symlink_metadata(r) {
            Ok(meta) if meta.file_type().is_symlink() => match fs::remove_file(r) {
                Ok(_) => println!("{g}removed{r} {}", r.display(), g = green(), r = reset()),
                Err(e) => println!("{red}could not remove {}: {e}{r}", r.display(), red = red(), r = reset()),
            },
            Ok(_) => println!("skipping {} (not a symlink, leaving it alone)", r.display()),
            Err(_) => println!("skipping {} (already gone)", r.display()),
        }
    }
}

// ---------- small helpers ----------

fn step(dry_run: bool, sudo: bool, program: &str, args: &[&str], why: &str, filter_deleting: bool) {
    let mut parts: Vec<&str> = Vec::new();
    if sudo {
        parts.push("sudo");
    }
    parts.push(program);
    parts.extend_from_slice(args);
    let rendered = parts.join(" ");

    if dry_run {
        println!("{y}[dry-run]{r} {rendered}", y = yellow(), r = reset());
        println!("          {d}({why}){r}", d = dim(), r = reset());
        return;
    }

    println!(
        "\n{c}->{r} {b}{rendered}{r}  {d}({why}){r}",
        c = cyan(),
        b = bold(),
        d = dim(),
        r = reset()
    );

    match run_streamed(sudo, program, args, filter_deleting) {
        Ok(status) if status.success() => println!("   {g}ok{r}", g = green(), r = reset()),
        Ok(status) => println!("   {red}exited with {status}{r}", red = red(), r = reset()),
        Err(e) => println!("   {red}failed to run: {e}{r}", red = red(), r = reset()),
    }
}

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Shared between the stdout- and stderr-reading threads so the spinner/counter
/// stays consistent (and writes don't interleave) no matter which stream a given
/// line of Nix's output actually arrives on.
struct Progress {
    deleted: u64,
    frame: usize,
    shown: bool,
}

/// Runs `program` (optionally under sudo) and streams BOTH its stdout and stderr,
/// line by line, so we can quiet down chatty commands. Nix's own progress logging
/// (`finding garbage collector roots...`, `deleting '...'`, etc.) goes to stderr,
/// not stdout — so both streams need the same filtering or the noisy lines just
/// slip through on the one we're not watching.
///
/// When `filter_deleting` is set, individual `deleting '/nix/store/...'` lines are
/// never printed — they collapse into one live-updating spinner line instead.
/// Everything else (in particular Nix's own final "N store paths deleted, X freed"
/// summary) still prints, highlighted green so it stands out.
fn run_streamed(
    sudo: bool,
    program: &str,
    args: &[&str],
    filter_deleting: bool,
) -> std::io::Result<std::process::ExitStatus> {
    let mut cmd = if sudo {
        let mut c = Command::new("sudo");
        c.arg(program);
        c.args(args);
        c
    } else {
        let mut c = Command::new(program);
        c.args(args);
        c
    };
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let progress = Arc::new(Mutex::new(Progress {
        deleted: 0,
        frame: 0,
        shown: false,
    }));

    let progress_err = Arc::clone(&progress);
    let err_thread = thread::spawn(move || {
        stream_filtered(BufReader::new(stderr), filter_deleting, &progress_err);
    });

    stream_filtered(BufReader::new(stdout), filter_deleting, &progress);
    let _ = err_thread.join();

    // Clear a still-live spinner line before letting the final "ok"/exit status print.
    let mut st = progress.lock().unwrap();
    if st.shown {
        clear_progress_line();
        println!("   {g}deleted {} store paths{r}", st.deleted, g = green(), r = reset());
        st.shown = false;
    }
    drop(st);

    child.wait()
}

fn stream_filtered<R: BufRead>(reader: R, filter_deleting: bool, progress: &Mutex<Progress>) {
    for line in reader.lines() {
        let Ok(line) = line else { break };

        if filter_deleting && line.starts_with("deleting '") {
            let mut st = progress.lock().unwrap();
            st.deleted += 1;
            st.frame = (st.frame + 1) % SPINNER.len();
            let mut out = std::io::stdout();
            let _ = write!(
                out,
                "\r   {c}{spin}{r} {d}clearing store paths...{r} {b}{n}{r} removed",
                c = cyan(),
                spin = SPINNER[st.frame],
                r = reset(),
                d = dim(),
                b = bold(),
                n = st.deleted,
            );
            let _ = out.flush();
            st.shown = true;
            continue;
        }

        let mut st = progress.lock().unwrap();
        if st.shown {
            clear_progress_line();
            st.shown = false;
        }
        drop(st);

        if line.trim().is_empty() {
            continue;
        }

        if filter_deleting && line.contains("store paths deleted") {
            println!("   {g}{line}{r}", g = green(), r = reset());
        } else if filter_deleting
            && (line.starts_with("finding garbage collector roots")
                || line.starts_with("deleting garbage")
                || line.starts_with("removing stale temporary roots file"))
        {
            // Routine gc chatter — same category as the deleting lines, just skip it.
            continue;
        } else {
            println!("   {line}");
        }
    }
}

fn clear_progress_line() {
    let mut out = std::io::stdout();
    let _ = write!(out, "\r{:width$}\r", "", width = 64);
    let _ = out.flush();
}

fn run_captured(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn command_exists(program: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {program} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn dirs_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME environment variable is not set")
}

fn section(title: &str) {
    println!("\n{b}{c}── {title} ──{r}", b = bold(), c = cyan(), r = reset());
}

fn indent(s: &str) -> String {
    s.lines().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n")
}

fn print_generation_count(label: &str, profile: &str) {
    match run_captured("nix-env", &["-p", profile, "--list-generations"]) {
        Some(out) if !out.is_empty() => {
            let n = out.lines().filter(|l| !l.trim().is_empty()).count();
            println!("  {label}: {n} generation(s)  (profile: {profile})");
        }
        _ => println!("  {label}: none found (profile: {profile})"),
    }
}

fn describe_age(path: &Path) -> String {
    match fs::symlink_metadata(path).and_then(|m| m.modified()) {
        Ok(modified) => {
            let secs = SystemTime::now()
                .duration_since(modified)
                .unwrap_or_default()
                .as_secs();
            let days = secs / 86400;
            format!("{days}d old")
        }
        Err(_) => "target gone, will be pruned on next gc".to_string(),
    }
}

// ---------- color ----------
// Hand-rolled ANSI helpers (no extra crate) — auto-disabled when stdout isn't
// a terminal, so piping to a file or log doesn't fill up with escape codes.

fn use_color() -> bool {
    std::io::stdout().is_terminal()
}

fn col(code: &str) -> String {
    if use_color() {
        format!("\x1b[{code}m")
    } else {
        String::new()
    }
}

fn reset() -> String {
    col("0")
}
fn bold() -> String {
    col("1")
}
fn dim() -> String {
    col("2")
}
fn red() -> String {
    col("31")
}
fn green() -> String {
    col("32")
}
fn yellow() -> String {
    col("33")
}
fn cyan() -> String {
    col("36")
}
