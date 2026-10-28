/*
 * Session supervisor: the long-lived process inside a session's sandbox. It runs the commands the host
 * sends, one at a time, so processes a command leaves running (a dev server, a watcher) survive into the
 * next command, which an exec-per-sandbox design cannot do.
 *
 * Built static (see build.rs) so it runs in any VM, whatever C library the VM has, and under gVisor.
 *
 * Protocol, all inside the control directory CTL (argv[1]), which the host can read and write:
 *   CTL/fifo          the host writes a request id and a newline
 *   CTL/<id>/argv     NUL-separated argument vector (argv[0] is looked up on the request's PATH)
 *   CTL/<id>/env      NUL-separated NAME=value list: the command's whole environment
 *   CTL/<id>/cwd      working directory
 *   CTL/<id>/timeout_ms
 * and the supervisor writes
 *   CTL/<id>/out, CTL/<id>/err   the command's stdout and stderr
 *   CTL/<id>/status              "exit N", "timeout" or "error <message>", written last and atomically
 *   CTL/ready                    once, when it accepts requests
 *
 * A command runs in its own session (setsid), so a timeout kills its whole process group. Output goes to
 * files, not pipes: a background process that keeps the output open cannot hold the request open.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/signalfd.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

extern char **environ;

static const char *ctl;
static int sigfd = -1;

static char *path_in(const char *id, const char *name) {
    char *p;
    if (asprintf(&p, "%s/%s/%s", ctl, id, name) < 0) return NULL;
    return p;
}

/* The whole file, NUL-terminated; *len gets its length. NULL if it cannot be read. */
static char *read_file(const char *path, size_t *len) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) return NULL;
    size_t cap = 4096, n = 0;
    char *buf = malloc(cap + 1);
    for (;;) {
        if (n == cap) { cap *= 2; char *b = realloc(buf, cap + 1); if (!b) { free(buf); close(fd); return NULL; } buf = b; }
        ssize_t r = read(fd, buf + n, cap - n);
        if (r < 0 && errno == EINTR) continue;
        if (r <= 0) break;
        n += (size_t)r;
    }
    close(fd);
    buf[n] = 0;
    if (len) *len = n;
    return buf;
}

/* NUL-separated list -> NULL-terminated vector pointing into buf. */
static char **split(char *buf, size_t len) {
    size_t count = 0;
    for (size_t i = 0; i < len; i++) if (buf[i] == 0) count++;
    if (len > 0 && buf[len - 1] != 0) count++;
    char **v = calloc(count + 1, sizeof(char *));
    size_t k = 0, start = 0;
    for (size_t i = 0; i <= len && k < count; i++) {
        if (i == len || buf[i] == 0) { v[k++] = buf + start; start = i + 1; }
    }
    v[count] = NULL;
    return v;
}

static void write_status(const char *id, const char *text) {
    char *tmp = path_in(id, "status.tmp"), *fin = path_in(id, "status");
    int fd = open(tmp, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0644);
    if (fd >= 0) {
        ssize_t w = write(fd, text, strlen(text));
        (void)w;
        close(fd);
        rename(tmp, fin);
    }
    free(tmp); free(fin);
}

static int64_t now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

/* Reap every finished child. Returns 1 and sets *status if `want` was among them. */
static int reap(pid_t want, int *status) {
    int found = 0, st;
    pid_t p;
    struct signalfd_siginfo si;
    while (read(sigfd, &si, sizeof si) == sizeof si) {}
    while ((p = waitpid(-1, &st, WNOHANG)) > 0) {
        if (p == want) { *status = st; found = 1; }
    }
    return found;
}

static void run(const char *id) {
    size_t alen = 0, elen = 0;
    char *pa = path_in(id, "argv"), *pe = path_in(id, "env"), *pc = path_in(id, "cwd"), *pt = path_in(id, "timeout_ms");
    char *abuf = read_file(pa, &alen), *ebuf = read_file(pe, &elen), *cwd = read_file(pc, NULL), *tbuf = read_file(pt, NULL);
    char *out = path_in(id, "out"), *err = path_in(id, "err");
    free(pa); free(pe); free(pc); free(pt);
    if (!abuf || alen == 0 || !ebuf || !cwd || !tbuf) { write_status(id, "error incomplete request"); goto done; }
    char **argv = split(abuf, alen), **envp = split(ebuf, elen);
    long timeout_ms = atol(tbuf);

    pid_t pid = fork();
    if (pid < 0) { write_status(id, "error fork failed"); free(argv); free(envp); goto done; }
    if (pid == 0) {
        sigset_t none;
        sigemptyset(&none);
        sigprocmask(SIG_SETMASK, &none, NULL);
        signal(SIGPIPE, SIG_DFL);
        setsid();
        int in = open("/dev/null", O_RDONLY);
        int o = open(out, O_WRONLY | O_CREAT | O_TRUNC, 0644);
        int e = open(err, O_WRONLY | O_CREAT | O_TRUNC, 0644);
        if (in < 0 || o < 0 || e < 0) _exit(126);
        dup2(in, 0); dup2(o, 1); dup2(e, 2);
        if (chdir(cwd) != 0) { dprintf(2, "cannot enter %s: %s\n", cwd, strerror(errno)); _exit(126); }
        environ = envp;
        execvp(argv[0], argv);
        dprintf(2, "cannot run %s: %s\n", argv[0], strerror(errno));
        _exit(127);
    }

    int st = 0, finished = 0;
    int64_t deadline = now_ms() + timeout_ms;
    while (!finished) {
        int64_t left = deadline - now_ms();
        if (left <= 0) break;
        struct pollfd pf = { .fd = sigfd, .events = POLLIN };
        poll(&pf, 1, left > 1000 ? 1000 : (int)left);
        finished = reap(pid, &st);
    }
    if (!finished) {
        kill(-pid, SIGKILL);
        while (!reap(pid, &st)) { struct pollfd pf = { .fd = sigfd, .events = POLLIN }; poll(&pf, 1, 100); }
        write_status(id, "timeout");
    } else {
        char s[32];
        snprintf(s, sizeof s, "exit %d", WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
        write_status(id, s);
    }
    free(argv); free(envp);
done:
    free(abuf); free(ebuf); free(cwd); free(tbuf); free(out); free(err);
}

/* A request id is the decimal sequence number the host assigned; anything else is ignored. */
static int valid_id(const char *s) {
    if (!*s) return 0;
    for (; *s; s++) if (*s < '0' || *s > '9') return 0;
    return 1;
}

int main(int argc, char **argv) {
    if (argc != 2) { fprintf(stderr, "usage: init CONTROL_DIR\n"); return 2; }
    ctl = argv[1];
    signal(SIGPIPE, SIG_IGN);
    sigset_t chld;
    sigemptyset(&chld);
    sigaddset(&chld, SIGCHLD);
    sigprocmask(SIG_BLOCK, &chld, NULL);
    sigfd = signalfd(-1, &chld, SFD_NONBLOCK | SFD_CLOEXEC);
    if (sigfd < 0) { perror("signalfd"); return 1; }

    char *fifo;
    if (asprintf(&fifo, "%s/fifo", ctl) < 0) return 1;
    /* read-write, so the supervisor itself keeps a writer and never sees end of file between requests */
    int ff = open(fifo, O_RDWR | O_CLOEXEC);
    if (ff < 0) { perror(fifo); return 1; }
    char *ready;
    if (asprintf(&ready, "%s/ready", ctl) < 0) return 1;
    int rf = open(ready, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0644);
    if (rf >= 0) close(rf);

    char line[64];
    size_t n = 0;
    for (;;) {
        struct pollfd pf[2] = { { .fd = ff, .events = POLLIN }, { .fd = sigfd, .events = POLLIN } };
        if (poll(pf, 2, -1) < 0) { if (errno == EINTR) continue; return 1; }
        /* orphaned processes are adopted by this process when it is pid 1 (gVisor): reap them */
        if (pf[1].revents) { int ignored; reap(-1, &ignored); }
        if (!(pf[0].revents & POLLIN)) continue;
        char c;
        while (read(ff, &c, 1) == 1) {
            if (c != '\n') { if (n < sizeof line - 1) line[n++] = c; continue; }
            line[n] = 0;
            n = 0;
            if (valid_id(line)) run(line);
            break;
        }
    }
}
