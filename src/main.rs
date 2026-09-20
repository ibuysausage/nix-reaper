use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
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
    /// Clean things up. Prints a plan by default; pass --yes to actually run it.
    Clean(CleanArgs),
}

#[derive(Args)]
struct CleanArgs {
    /// Actually execute the plan. Without this flag, nix-reaper only prints what it would do.
    #[arg(short, long)]
    yes: bool,

    /// Keep this many NixOS system profile generations
    #[arg(long, default_value_t = 3)]
    keep_system: u32,

    /// Keep this many per-user (nix-env) profile generations
    #[arg(long, default_value_t = 3)]
    keep_user: u32,

    /// Expire home-manager generations older than this many days (skipped if home-manager isn't found)
    #[arg(long, default_value_t = 30)]
    keep_home_days: u32,

    /// Run `nix-collect-garbage -d` after trimming generations
    #[arg(long)]
    gc: bool,

    /// Run the store optimiser (hardlinks duplicate files) after gc
    #[arg(long)]
    optimise: bool,

    /// Prune unused docker/podman images, containers and volumes, if either is installed
    #[arg(long)]
    docker: bool,

    /// Vacuum the systemd journal down to this size, e.g. 200M. Omit to skip.
    #[arg(long, value_name = "SIZE")]
    journal: Option<String>,

    /// Find & optionally remove stray `result` symlinks / dev-shell profiles pinning old store paths alive
    #[arg(long)]
    roots: bool,

    /// Only remove root symlinks older than this many days (used with --roots --yes)
    #[arg(long, default_value_t = 30)]
    roots_older_than_days: u64,

    /// Shortcut: turn on --gc --optimise --docker --roots and set journal=200M, using the defaults above
    #[arg(long)]
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
        println!("  -> `nix-reaper clean --roots --yes` removes the stale ones");
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
    println!("Run `nix-reaper clean --all` to see a cleanup plan (dry-run), then add --yes to execute it.");
    Ok(())
}

// ---------- clean ----------

fn clean(mut args: CleanArgs) -> Result<()> {
    if args.all {
        args.gc = true;
        args.optimise = true;
        args.docker = true;
        args.roots = true;
        if args.journal.is_none() {
            args.journal = Some("200M".to_string());
        }
    }

    let home = dirs_home()?;
    let dry_run = !args.yes;

    if dry_run {
        println!("DRY RUN — nothing will actually change. Re-run with --yes once this plan looks right.\n");
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
    );

    // 3. home-manager generations, if present
    if command_exists("home-manager") {
        let hm_flag = format!("-{} days", args.keep_home_days);
        let why_hm = format!("expire home-manager generations older than {} days", args.keep_home_days);
        step(dry_run, false, "home-manager", &["expire-generations", hm_flag.as_str()], &why_hm);
    }

    // 4. actual gc
    if args.gc {
        step(
            dry_run,
            false,
            "nix-collect-garbage",
            &["-d"],
            "delete everything no longer reachable from a live generation (add sudo yourself if this errors on permissions)",
        );
    }

    // 5. optimise
    if args.optimise {
        step(
            dry_run,
            false,
            "nix",
            &["store", "optimise"],
            "hardlink duplicate files inside the store (saves space, deletes nothing reachable)",
        );
    }

    // 6. journal
    if let Some(size) = &args.journal {
        let flag = format!("--vacuum-size={size}");
        let why_journal = format!("shrink the systemd journal down to {size}");
        step(dry_run, true, "journalctl", &[flag.as_str()], &why_journal);
    }

    // 7. docker / podman
    if args.docker {
        if command_exists("docker") {
            step(
                dry_run,
                false,
                "docker",
                &["system", "prune", "-af", "--volumes"],
                "remove unused docker images, stopped containers, and unused volumes",
            );
        }
        if command_exists("podman") {
            step(
                dry_run,
                false,
                "podman",
                &["system", "prune", "-af", "--volumes"],
                "remove unused podman images, stopped containers, and unused volumes",
            );
        }
    }

    // 8. stray build result / dev-shell gcroots under $HOME
    if args.roots {
        clean_roots(&home, dry_run, args.roots_older_than_days);
    }

    if !dry_run {
        if let Some(before) = before_size {
            if let Some(after) = run_captured("du", &["-sh", "/nix/store"]) {
                println!("\nnix store size: {before} -> {after}");
            }
        }
    } else {
        println!("\nLooks right? Re-run the same command with --yes to actually do it.");
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
                "[dry-run] rm {}   ({age_days}d old — project directory stays, only the pinning symlink goes)",
                r.display()
            );
            continue;
        }

        match fs::symlink_metadata(r) {
            Ok(meta) if meta.file_type().is_symlink() => match fs::remove_file(r) {
                Ok(_) => println!("removed {}", r.display()),
                Err(e) => println!("could not remove {}: {e}", r.display()),
            },
            Ok(_) => println!("skipping {} (not a symlink, leaving it alone)", r.display()),
            Err(_) => println!("skipping {} (already gone)", r.display()),
        }
    }
}

// ---------- small helpers ----------

fn step(dry_run: bool, sudo: bool, program: &str, args: &[&str], why: &str) {
    let mut parts: Vec<&str> = Vec::new();
    if sudo {
        parts.push("sudo");
    }
    parts.push(program);
    parts.extend_from_slice(args);
    let rendered = parts.join(" ");

    if dry_run {
        println!("[dry-run] {rendered}");
        println!("          ({why})");
        return;
    }

    println!("-> {rendered}   ({why})");
    let status = if sudo {
        Command::new("sudo").arg(program).args(args).status()
    } else {
        Command::new(program).args(args).status()
    };
    match status {
        Ok(s) if s.success() => println!("   ok"),
        Ok(s) => println!("   exited with {s}"),
        Err(e) => println!("   failed to run: {e}"),
    }
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
    println!("\n== {title} ==");
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
