# nix-reaper

A small CLI that does everything `nix-collect-garbage -d` / `nix store optimise`
don't: trims old generations (system, user, home-manager), runs the real GC,
finds stray `result` symlinks and nix-direnv dev-shell profiles that pin whole
closures alive forever, and cleans the non-Nix stuff (docker/podman, journal)
that survives a reinstall if you keep the same home directory.

**Dry-run by default.** Every `clean` invocation just prints the plan unless
you pass `--yes`.

## First-time build

```bash
nix develop -c cargo generate-lockfile   # one-time, needs network, creates Cargo.lock
nix build                                # ./result/bin/nix-reaper
# or just run it straight away:
nix run . -- status
```

## Usage

```bash
# see what's taking up space — read-only
nix-reaper status

# see the full cleanup plan without touching anything
nix-reaper clean --all

# actually run it
nix-reaper clean --all --yes

# or pick individual steps
nix-reaper clean --keep-system 5 --keep-user 5 --gc --yes
nix-reaper clean --roots --roots-older-than-days 14 --yes
nix-reaper clean --journal 200M --docker --yes
```

## Flags (`clean` subcommand)

| Flag | Default | What it does |
|---|---|---|
| `--keep-system N` | 3 | Keep newest N system profile generations |
| `--keep-user N` | 3 | Keep newest N of your own `nix-env` profile generations |
| `--keep-home-days N` | 30 | Expire home-manager generations older than N days (skipped if no home-manager) |
| `--gc` | off | Run `nix-collect-garbage -d` |
| `--optimise` | off | Run `nix store optimise` |
| `--docker` | off | Prune unused docker/podman images, containers, volumes |
| `--journal SIZE` | off | `journalctl --vacuum-size=SIZE`, e.g. `200M` |
| `--roots` | off | Find/remove stray `result*` symlinks & dev-shell GC roots under `$HOME` |
| `--roots-older-than-days N` | 30 | Only remove roots older than N days |
| `--all` | off | Shortcut: turns on gc, optimise, docker, roots, journal=200M |
| `--yes` / `-y` | off | Actually execute — otherwise it's a dry run |

## What it does NOT do automatically

- **Edit your NixOS config.** `status` tells you whether to add
  `boot.loader.systemd-boot.configurationLimit = 5;` (or the GRUB equivalent)
  but won't touch `configuration.nix` itself — that's yours to review.
- **Run without a lockfile.** `nix build` needs `Cargo.lock` present; generate
  it once as shown above.
- **Delete non-symlinks under `--roots`.** It only ever removes the pinning
  symlink (`result`, `.direnv/...`), never the project directory itself.

## How the `--roots` scan works

It reads `nix-store --gc --print-roots`, which lists every currently-live GC
root — this is the same mechanism that keeps a `nix build` output or a
`nix develop`/nix-direnv shell alive indefinitely even after you've stopped
touching that project. Anything under your home directory that shows up
there is a candidate; anything older than the cutoff gets removed (with
`--yes`), leaving the actual project files untouched. The store paths they
were pinning become eligible for the next `--gc` run.
