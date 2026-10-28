/*
 * Session supervisor: the long-lived process inside a session's sandbox. It runs the commands the host
 * sends, one at a time, so processes a command leaves running (a dev server, a watcher) survive into the
 * next command, which an exec-per-sandbox design cannot do.
 *
 * Built static (see build.rs) so it runs in any VM, whatever C library the VM has, and under gVisor.
 *
 * Protocol, all inside the control directory CTL (argv[1]), which the host can read and write:
 *   CTL/fifo          the host writes a request id and a newline
 *   CTL/<id>/op       "exec" (the default when absent), "read", "write" or "list"
 * exec:
 *   CTL/<id>/argv     NUL-separated argument vector (argv[0] is looked up on the request's PATH)
 *   CTL/<id>/env      NUL-separated NAME=value list: the command's whole environment
 *   CTL/<id>/cwd      working directory
 *   CTL/<id>/timeout_ms
 * read:   CTL/<id>/path, CTL/<id>/offset, CTL/<id>/len   -> the bytes in out
 * write:  CTL/<id>/path, CTL/<id>/mode (octal), CTL/<id>/in -> the file, parent directories created
 * list:   CTL/<id>/path                                   -> "<kind>\t<size>\t<name>\n" per entry in out
 * and the supervisor writes
 *   CTL/<id>/out, CTL/<id>/err   output (exec: the command's stdout and stderr; file ops: result, error)
 *   CTL/<id>/status              "exit N", "timeout" or "error <message>", written last and atomically
 *   CTL/ready                    once, when it accepts requests
 *
 * File operations run in the supervisor itself, inside the sandbox, so the sandbox's mounts apply to them
 * as to a command: a read-only path refuses a write either way.
 *
 * A command runs in its own session (setsid), so a timeout kills its whole process group. Output goes to
 * files, not pipes: a background process that keeps the output open cannot hold the request open.
 *
 * With "--forward PORT SOCKET", a separate process relays every connection to 127.0.0.1:PORT to the unix
 * socket SOCKET (the host's egress proxy, bound into the sandbox). It is its own process because a command
 * that needs the network (pip install) is running while its connections arrive.
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
#include <dirent.h>
#include <netinet/in.h>
#include <sys/signalfd.h>
#include <sys/socket.h>
#include <sys/un.h>
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

static void run_exec(const char *id) {
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

static void trim(char *s) {
    size_t n = strlen(s);
    while (n > 0 && (s[n - 1] == '\n' || s[n - 1] == ' ')) s[--n] = 0;
}

/* Write `text` to the request's err file. */
static void put_err(const char *id, const char *text) {
    char *pe = path_in(id, "err");
    int fd = open(pe, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0644);
    if (fd >= 0) { ssize_t w = write(fd, text, strlen(text)); (void)w; close(fd); }
    free(pe);
}

static void fail(const char *id, const char *what, const char *path) {
    char *msg;
    if (asprintf(&msg, "%s %s: %s\n", what, path, strerror(errno)) < 0) msg = NULL;
    put_err(id, msg ? msg : "error\n");
    free(msg);
    write_status(id, "exit 1");
}

/* Copy up to `len` bytes from `in` to `out`. -1 on error. */
static int copy_fd(int in, int out, long long len) {
    char buf[65536];
    while (len != 0) {
        size_t want = len < 0 || len > (long long)sizeof buf ? sizeof buf : (size_t)len;
        ssize_t r = read(in, buf, want);
        if (r < 0) { if (errno == EINTR) continue; return -1; }
        if (r == 0) break;
        for (ssize_t done = 0; done < r;) {
            ssize_t w = write(out, buf + done, (size_t)(r - done));
            if (w < 0) { if (errno == EINTR) continue; return -1; }
            done += w;
        }
        if (len > 0) len -= r;
    }
    return 0;
}

static void op_read(const char *id) {
    char *pp = path_in(id, "path"), *po = path_in(id, "offset"), *pl = path_in(id, "len"), *out = path_in(id, "out");
    char *path = read_file(pp, NULL), *off = read_file(po, NULL), *len = read_file(pl, NULL);
    if (!path || !off || !len) { put_err(id, "incomplete request\n"); write_status(id, "exit 1"); goto done; }
    trim(path);
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) { fail(id, "cannot open", path); goto done; }
    int o = open(out, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0644);
    if (o < 0 || lseek(fd, atoll(off), SEEK_SET) < 0 || copy_fd(fd, o, atoll(len)) != 0) { fail(id, "cannot read", path); }
    else write_status(id, "exit 0");
    if (o >= 0) close(o);
    close(fd);
done:
    free(pp); free(po); free(pl); free(out); free(path); free(off); free(len);
}

/* mkdir -p for the parent directories of `path`. */
static void make_parents(char *path) {
    for (char *p = path + 1; *p; p++) {
        if (*p != '/') continue;
        *p = 0;
        mkdir(path, 0755);
        *p = '/';
    }
}

static void op_write(const char *id) {
    char *pp = path_in(id, "path"), *pm = path_in(id, "mode"), *pi = path_in(id, "in");
    char *path = read_file(pp, NULL), *mode = read_file(pm, NULL);
    if (!path || !mode) { put_err(id, "incomplete request\n"); write_status(id, "exit 1"); goto done; }
    trim(path);
    int in = open(pi, O_RDONLY | O_CLOEXEC);
    if (in < 0) { fail(id, "cannot read request data for", path); goto done; }
    make_parents(path);
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, (mode_t)strtol(mode, NULL, 8));
    if (fd < 0) { fail(id, "cannot open", path); close(in); goto done; }
    if (copy_fd(in, fd, -1) != 0) fail(id, "cannot write", path);
    else write_status(id, "exit 0");
    close(fd);
    close(in);
done:
    free(pp); free(pm); free(pi); free(path); free(mode);
}

static void op_list(const char *id) {
    char *pp = path_in(id, "path"), *out = path_in(id, "out");
    char *path = read_file(pp, NULL);
    if (!path) { put_err(id, "incomplete request\n"); write_status(id, "exit 1"); goto done; }
    trim(path);
    DIR *d = opendir(path);
    if (!d) { fail(id, "cannot list", path); goto done; }
    FILE *o = fopen(out, "we");
    if (!o) { closedir(d); fail(id, "cannot write listing for", path); goto done; }
    struct dirent *e;
    while ((e = readdir(d))) {
        if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, "..")) continue;
        struct stat st;
        char kind = 'o';
        long long size = 0;
        if (fstatat(dirfd(d), e->d_name, &st, AT_SYMLINK_NOFOLLOW) == 0) {
            kind = S_ISREG(st.st_mode) ? 'f' : S_ISDIR(st.st_mode) ? 'd' : S_ISLNK(st.st_mode) ? 'l' : 'o';
            size = st.st_size;
        }
        fprintf(o, "%c\t%lld\t%s\n", kind, size, e->d_name);
    }
    fclose(o);
    closedir(d);
    write_status(id, "exit 0");
done:
    free(pp); free(out); free(path);
}

static void handle(const char *id) {
    char *po = path_in(id, "op");
    char *op = read_file(po, NULL);
    free(po);
    if (op) trim(op);
    if (!op || !strcmp(op, "exec")) run_exec(id);
    else if (!strcmp(op, "read")) op_read(id);
    else if (!strcmp(op, "write")) op_write(id);
    else if (!strcmp(op, "list")) op_list(id);
    else write_status(id, "error unknown operation");
    free(op);
}

/* Relay one connection between `a` and `b` until both directions are closed. */
static void relay(int a, int b) {
    char buf[65536];
    int open_a = 1, open_b = 1;
    while (open_a || open_b) {
        struct pollfd pf[2] = { { .fd = open_a ? a : -1, .events = POLLIN }, { .fd = open_b ? b : -1, .events = POLLIN } };
        if (poll(pf, 2, -1) < 0) { if (errno == EINTR) continue; return; }
        for (int i = 0; i < 2; i++) {
            if (!pf[i].revents) continue;
            int from = i == 0 ? a : b, to = i == 0 ? b : a;
            ssize_t r = read(from, buf, sizeof buf);
            if (r <= 0) {
                shutdown(to, SHUT_WR);
                if (i == 0) open_a = 0; else open_b = 0;
                continue;
            }
            for (ssize_t done = 0; done < r;) {
                ssize_t w = write(to, buf + done, (size_t)(r - done));
                if (w <= 0) return;
                done += w;
            }
        }
    }
}

/* The forwarder: 127.0.0.1:port -> unix socket. Runs until the sandbox ends. */
static void forwarder(int port, const char *sock) {
    sigset_t none;
    sigemptyset(&none);
    sigprocmask(SIG_SETMASK, &none, NULL);
    signal(SIGCHLD, SIG_IGN);  /* connection children are reaped by the kernel */
    int l = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
    int one = 1;
    setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in addr = { .sin_family = AF_INET, .sin_port = htons((uint16_t)port), .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
    if (l < 0 || bind(l, (struct sockaddr *)&addr, sizeof addr) != 0 || listen(l, 128) != 0) { perror("forwarder"); _exit(1); }
    for (;;) {
        int c = accept4(l, NULL, NULL, SOCK_CLOEXEC);
        if (c < 0) { if (errno == EINTR) continue; _exit(1); }
        if (fork() == 0) {
            close(l);
            int u = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
            struct sockaddr_un ua = { .sun_family = AF_UNIX };
            strncpy(ua.sun_path, sock, sizeof ua.sun_path - 1);
            if (u < 0 || connect(u, (struct sockaddr *)&ua, sizeof ua) != 0) _exit(1);
            relay(c, u);
            _exit(0);
        }
        close(c);
    }
}

/* A request id is the decimal sequence number the host assigned; anything else is ignored. */
static int valid_id(const char *s) {
    if (!*s) return 0;
    for (; *s; s++) if (*s < '0' || *s > '9') return 0;
    return 1;
}

int main(int argc, char **argv) {
    if (argc != 2 && !(argc == 5 && !strcmp(argv[2], "--forward"))) {
        fprintf(stderr, "usage: init CONTROL_DIR [--forward PORT SOCKET]\n");
        return 2;
    }
    ctl = argv[1];
    if (argc == 5 && fork() == 0) forwarder(atoi(argv[3]), argv[4]);
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
            if (valid_id(line)) handle(line);
            break;
        }
    }
}
