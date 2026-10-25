#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

/* nix-reaper: small POSIX C implementation. External Nix/Docker tools still do
 * the work. */
static bool dry_run = false, clear_cache = false, color = false;
static bool short_names =
    true; /* default: "glibc"; --long gives "glibc-2.40-66" */
static const char *journal = "1s";
#define RESET (color ? "\033[0m" : "")
#define BOLD (color ? "\033[1m" : "")
#define DIM (color ? "\033[2m" : "")
#define RED (color ? "\033[31m" : "")
#define GREEN (color ? "\033[32m" : "")
#define YELLOW (color ? "\033[33m" : "")
#define CYAN (color ? "\033[36m" : "")

static void usage(FILE *f) {
  fprintf(f,
          "Usage: nix-reaper <status|clean> [options]\n"
          "  status                  Report disk usage (read-only)\n"
          "  clean [--dry-run]       Remove old generations, garbage, caches, "
          "etc.\n"
          "       [--journal TIME]   journalctl vacuum time (default 1s; off "
          "skips)\n"
          "       [--clear-cache]    Also empty ~/.cache\n"
          "       [--long]           Show versions in removed package names\n");
}
static bool exists_exec(const char *name) {
  if (strchr(name, '/'))
    return access(name, X_OK) == 0;
  const char *env = getenv("PATH");
  if (!env)
    env = "/run/current-system/sw/bin:/usr/bin:/bin";
  char *copy = strdup(env);
  if (!copy)
    return false;
  bool found = false;
  char *save = NULL;
  for (char *d = strtok_r(copy, ":", &save); d;
       d = strtok_r(NULL, ":", &save)) {
    if (!*d)
      d = ".";
    size_t n = strlen(d) + strlen(name) + 2;
    char *p = malloc(n);
    if (p) {
      snprintf(p, n, "%s/%s", d, name);
      if (access(p, X_OK) == 0)
        found = true;
      free(p);
    }
    if (found)
      break;
  }
  free(copy);
  return found;
}

/* "<32-char-hash>-name-1.2" -> "name-1.2" (or "name" when short_names) */
static void pretty_name(const char *p, char *out, size_t sz) {
  if (strlen(p) > 33 && p[32] == '-')
    p += 33;
  size_t i = 0;
  for (; p[i] && p[i] != '\'' && p[i] != '\n' && i + 1 < sz; i++)
    out[i] = p[i];
  out[i] = 0;
  if (short_names) /* cut at the first "-<digit>" */
    for (size_t j = 0; out[j]; j++)
      if (out[j] == '-' && out[j + 1] >= '0' && out[j + 1] <= '9') {
        out[j] = 0;
        break;
      }
}

/* Print a line, stripping "/nix/store/<hash>-" wherever it appears. */
static void strip_store_paths(const char *line) {
  const char *s = line, *m;
  while ((m = strstr(s, "/nix/store/"))) {
    const char *h = m + 11;
    fwrite(s, 1, (size_t)(m - s), stdout);
    if (strlen(h) > 33 && h[32] == '-')
      s = h + 33;
    else {
      fwrite(m, 1, 11, stdout);
      s = h;
    }
  }
  fputs(s, stdout);
}

static void print_gc_line(const char *line) {
  const char *pfx = "deleting '/nix/store/";
  size_t pl = strlen(pfx);
  if (!strncmp(line, pfx, pl)) {
    char name[1024];
    pretty_name(line + pl, name, sizeof name);
    printf("  %s-%s %s\n", RED, RESET, name);
    return;
  }
  strip_store_paths(line);
}

static int run(char *const argv[], bool stream) {
  (void)stream;
  if (dry_run) {
    printf("%s[dry-run]%s", YELLOW, RESET);
    for (int i = 0; argv[i]; i++)
      printf(" %s", argv[i]);
    putchar('\n');
    return 0;
  }
  bool filter = !strcmp(argv[0], "nix-collect-garbage");
  int fd[2] = {-1, -1};
  if (filter && pipe(fd))
    filter = false;
  fflush(stdout);
  pid_t p = fork();
  if (p < 0) {
    perror("fork");
    return -1;
  }
  if (p == 0) {
    if (filter) {
      close(fd[0]);
      dup2(fd[1], STDOUT_FILENO);
      dup2(fd[1], STDERR_FILENO);
      close(fd[1]);
    }
    execvp(argv[0], argv);
    fprintf(stderr, "%s: %s\n", argv[0], strerror(errno));
    _exit(127);
  }
  if (filter) {
    close(fd[1]);
    FILE *f = fdopen(fd[0], "r");
    char *line = NULL;
    size_t cap = 0;
    while (f && getline(&line, &cap, f) > 0)
      print_gc_line(line);
    free(line);
    if (f)
      fclose(f);
  }
  int st;
  while (waitpid(p, &st, 0) < 0)
    if (errno != EINTR) {
      perror("waitpid");
      return -1;
    }
  if (WIFEXITED(st) && WEXITSTATUS(st) == 0)
    return 0;
  fprintf(stderr, "%s%s failed%s\n", RED, argv[0], RESET);
  return WIFEXITED(st) ? WEXITSTATUS(st) : 128;
}
static int run_sudo(const char *prog, char *const args[]) {
  int n = 0;
  while (args[n])
    n++;
  char **v = calloc((size_t)n + 3, sizeof(char *));
  if (!v)
    return -1;
  v[0] = "sudo";
  v[1] = (char *)prog;
  for (int i = 0; i < n; i++)
    v[i + 2] = args[i];
  v[n + 2] = NULL;
  int r = run(v, true);
  free(v);
  return r;
}
static char *capture(const char *prog, char *const args[]) {
  int fd[2];
  if (pipe(fd))
    return NULL;
  pid_t p = fork();
  if (p < 0) {
    close(fd[0]);
    close(fd[1]);
    return NULL;
  }
  if (!p) {
    close(fd[0]);
    dup2(fd[1], STDOUT_FILENO);
    close(fd[1]);
    int n = 0;
    while (args[n])
      n++;
    char **v = calloc((size_t)n + 2, sizeof(char *));
    if (!v)
      _exit(127);
    v[0] = (char *)prog;
    for (int i = 0; i < n; i++)
      v[i + 1] = args[i];
    execvp(prog, v);
    _exit(127);
  }
  close(fd[1]);
  size_t cap = 4096, len = 0;
  char *s = malloc(cap);
  if (!s) {
    close(fd[0]);
    waitpid(p, NULL, 0);
    return NULL;
  }
  for (;;) {
    if (len + 2048 + 1 > cap) {
      cap *= 2;
      char *t = realloc(s, cap);
      if (!t)
        break;
      s = t;
    }
    ssize_t n = read(fd[0], s + len, cap - len - 1);
    if (n <= 0)
      break;
    len += (size_t)n;
  }
  close(fd[0]);
  waitpid(p, NULL, 0);
  s[len] = 0;
  while (len && (s[len - 1] == '\n' || s[len - 1] == ' ' || s[len - 1] == '\t'))
    s[--len] = 0;
  return len ? s : (free(s), (char *)NULL);
}
static void section(const char *s) {
  printf("\n%s%s── %s ──%s\n", BOLD, CYAN, s, RESET);
}
static void step(const char *prog, char *const args[], const char *why,
                 bool sudo) {
  printf("\n%s->%s %s%s%s  %s(%s)%s\n", CYAN, RESET, sudo ? "sudo " : "", BOLD,
         prog, DIM, why, RESET);
  if (dry_run) {
    printf("  %s[dry-run]%s", YELLOW, RESET);
    for (int i = 0; args[i]; i++)
      printf(" %s", args[i]);
    putchar('\n');
    return;
  }
  int r;
  if (sudo)
    r = run_sudo(prog, args);
  else {
    int n = 0;
    while (args[n])
      n++;
    char **v = calloc((size_t)n + 2, sizeof(char *));
    if (!v)
      return;
    v[0] = (char *)prog;
    for (int i = 0; i < n; i++)
      v[i + 1] = args[i];
    r = run(v, true);
    free(v);
  }
  if (r == 0)
    printf("  %sok%s\n", GREEN, RESET);
}
static const char *home_dir(void) {
  const char *h = getenv("HOME");
  return h ? h : "/root";
}
static void remove_contents(const char *label, const char *path) {
  DIR *d = opendir(path);
  if (!d)
    return;
  printf("\n%s->%s %snuking %s%s (%s)\n", CYAN, RESET, BOLD, label, RESET,
         path);
  struct dirent *e;
  unsigned long count = 0;
  while ((e = readdir(d))) {
    if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, ".."))
      continue;
    size_t n = strlen(path) + strlen(e->d_name) + 2;
    char *p = malloc(n);
    if (!p)
      continue;
    snprintf(p, n, "%s/%s", path, e->d_name);
    struct stat st;
    if (lstat(p, &st) == 0) {
      if (S_ISDIR(st.st_mode)) { /* recursive deletion via find, avoids unsafe
                                    shell expansion */
        char *av[] = {"rm", "-rf", "--", p, NULL};
        if (!dry_run && run(av, true) == 0)
          count++;
        else if (dry_run) {
          printf("[dry-run] rm -rf -- %s\n", p);
          count++;
        }
      } else if (dry_run) {
        printf("[dry-run] rm -- %s\n", p);
        count++;
      } else if (unlink(p) == 0)
        count++;
    }
    free(p);
  }
  closedir(d);
  printf("  %sok%s — removed %lu entries\n", GREEN, RESET, count);
}
static void status(void) {
  section("Nix store");
  char *a[] = {"-sh", "/nix/store", NULL};
  char *s = capture("du", a);
  if (s) {
    printf("  size on disk: %s\n", s);
    free(s);
  }
  char *b[] = {"-h", "/nix/store", NULL};
  s = capture("df", b);
  if (s) {
    printf("%s\n", s);
    free(s);
  }
  section("Generations");
  char *c[] = {"-p", "/nix/var/nix/profiles/system", "--list-generations",
               NULL};
  s = capture("nix-env", c);
  if (s) {
    int n = 0;
    for (char *p = s; *p; p++)
      if (*p == '\n')
        n++;
    printf("  system profile: %d generation(s)\n", n + (s[0] != 0));
    free(s);
  } else
    puts("  system profile: unavailable");
  char profile[4096];
  snprintf(profile, sizeof profile, "%s/.nix-profile", home_dir());
  char *d[] = {"-p", profile, "--list-generations", NULL};
  s = capture("nix-env", d);
  if (s) {
    int n = 0;
    for (char *p = s; *p; p++)
      if (*p == '\n')
        n++;
    printf("  user profile: %d generation(s)\n", n + (s[0] != 0));
    free(s);
  }
  section("Boot entries (not deleted)");
  if (access("/boot/loader/entries", F_OK) == 0)
    puts("  systemd-boot entries exist; set "
         "boot.loader.systemd-boot.configurationLimit in NixOS config");
  else
    puts("  probably GRUB; set boot.loader.grub.configurationLimit in NixOS "
         "config");
  section("Caches and journal");
  char path[4096];
  snprintf(path, sizeof path, "%s/.cache", home_dir());
  char *e[] = {"-sh", path, NULL};
  s = capture("du", e);
  if (s) {
    printf("  ~/.cache: %s\n", s);
    free(s);
  }
  char *f[] = {"--disk-usage", NULL};
  s = capture("journalctl", f);
  if (s) {
    printf("  journal: %s\n", s);
    free(s);
  }
  puts("\nRun nix-reaper clean --dry-run to preview.");
}
static int clean(void) {
  const char *h = home_dir();
  if (dry_run)
    printf("%sDRY RUN%s — nothing will actually change.\n", YELLOW, RESET);
  else
    printf("%s%s>>> CLEANING <<<%s\n", RED, BOLD, RESET);
  /* Important: do not run `du -sh /nix/store` here. It recursively scans the
     store and was delaying the very first cleanup command. */
  char *a[] = {"-p", "/nix/var/nix/profiles/system", "--delete-generations",
               "old", NULL};
  step("nix-env", a, "delete old system generations", true);
  char *b[] = {"--delete-generations", "old", NULL};
  step("nix-env", b, "delete old user generations", false);
  if (exists_exec("home-manager")) {
    char *c[] = {"expire-generations", "-1 seconds", NULL};
    step("home-manager", c, "expire old Home Manager generations", false);
  }
  char *d[] = {"-d", NULL};
  step("nix-collect-garbage", d, "collect unreachable Nix store paths", false);
  char *e[] = {"store", "optimise", NULL};
  step("nix", e, "hardlink duplicate store files", false);
  if (strcasecmp(journal, "off") && strcasecmp(journal, "none") &&
      strcasecmp(journal, "skip")) {
    char arg[256];
    snprintf(arg, sizeof arg, "--vacuum-time=%s", journal);
    char *f[] = {arg, NULL};
    step("journalctl", f, "vacuum systemd journal", true);
  }
  if (exists_exec("docker")) {
    char *g[] = {"system", "prune", "-af", "--volumes", NULL};
    step("docker", g, "remove unused Docker data", false);
    char *i[] = {"builder", "prune", "-af", NULL};
    step("docker", i, "remove Docker build cache", false);
  }
  if (exists_exec("podman")) {
    char *g[] = {"system", "prune", "-af", "--volumes", NULL};
    step("podman", g, "remove unused Podman data", false);
  }
  char path[4096];
  snprintf(path, sizeof path, "%s/.local/share/Trash", h);
  remove_contents("trash", path);
  if (clear_cache) {
    snprintf(path, sizeof path, "%s/.cache", h);
    remove_contents("~/.cache", path);
  } else
    printf("\n%s[~/.cache] skipped — pass --clear-cache to empty it%s\n", DIM,
           RESET);
  if (exists_exec("cargo")) {
    snprintf(path, sizeof path, "%s/.cargo/registry/cache", h);
    remove_contents("Cargo registry cache", path);
    snprintf(path, sizeof path, "%s/.cargo/registry/src", h);
    remove_contents("Cargo registry sources", path);
  }
  if (exists_exec("npm")) {
    char *g[] = {"cache", "clean", "--force", NULL};
    step("npm", g, "wipe npm cache", false);
  }
  if (exists_exec("go")) {
    snprintf(path, sizeof path, "%s/.cache/go-build", h);
    remove_contents("Go build cache", path);
  }
  if (access("/var/lib/systemd/coredump", F_OK) == 0) {
    char *g[] = {"/var/lib/systemd/coredump", "-mindepth", "1", "-delete",
                 NULL};
    step("find", g, "delete systemd coredumps", true);
  }
  if (!dry_run)
    printf("\n%s%s>>> DONE <<<%s\n", GREEN, BOLD, RESET);
  else
    puts("\nRe-run without --dry-run to execute.");
  return 0;
}
int main(int argc, char **argv) {
  color = isatty(STDOUT_FILENO);
  if (argc < 2) {
    usage(stderr);
    return 2;
  }
  const char *cmd = argv[1];
  for (int i = 2; i < argc; i++) {
    if (!strcmp(argv[i], "--dry-run"))
      dry_run = true;
    else if (!strcmp(argv[i], "--clear-cache"))
      clear_cache = true;
    else if (!strcmp(argv[i], "--long"))
      short_names = false;
    else if (!strcmp(argv[i], "--short"))
      short_names = true;
    else if (!strcmp(argv[i], "--journal") && i + 1 < argc)
      journal = argv[++i];
    else if (!strncmp(argv[i], "--journal=", 10))
      journal = argv[i] + 10;
    else if (!strcmp(argv[i], "--help") || !strcmp(argv[i], "-h")) {
      usage(stdout);
      return 0;
    } else if (!strcmp(argv[i], "--all")) {
    } else {
      fprintf(stderr, "Unknown option: %s\n", argv[i]);
      usage(stderr);
      return 2;
    }
  }
  if (!strcmp(cmd, "status"))
    status();
  else if (!strcmp(cmd, "clean"))
    clean();
  else {
    usage(stderr);
    return 2;
  }
  return 0;
}
