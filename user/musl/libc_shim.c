/* mpos musl-compatible libc shim.
 *
 * Provides the minimum C runtime needed to link uutils coreutils compiled
 * for x86_64-unknown-linux-musl.  Every syscall goes through the `syscall`
 * instruction, which mpos handles via the Linux ABI entry point.
 *
 * Build with:
 *   musl-gcc -nostdlib -c -o crt1.o crt1.c
 *   musl-gcc -nostdlib -c -o libc_shim.o libc_shim.c
 *   ar rcs libc.a libc_shim.o
 */

#include <stddef.h>
#include <stdint.h>

/* ------------------------------------------------------------------ */
/* Linux x86_64 syscall numbers (same as linux/nr.rs)                  */
/* ------------------------------------------------------------------ */
#define SYS_READ            0
#define SYS_WRITE           1
#define SYS_OPEN            2
#define SYS_CLOSE           3
#define SYS_STAT            4
#define SYS_FSTAT           5
#define SYS_LSTAT           6
#define SYS_LSEEK           8
#define SYS_MMAP            9
#define SYS_MPROTECT       10
#define SYS_MUNMAP         11
#define SYS_BRK            12
#define SYS_RT_SIGACTION   13
#define SYS_RT_SIGPROCMASK 14
#define SYS_RT_SIGRETURN   15
#define SYS_IOCTL          16
#define SYS_READV          19
#define SYS_WRITEV         20
#define SYS_ACCESS         21
#define SYS_PIPE           22
#define SYS_DUP            32
#define SYS_DUP2           33
#define SYS_NANOSLEEP      35
#define SYS_GETPID         39
#define SYS_SOCKET         41
#define SYS_CONNECT        42
#define SYS_ACCEPT         43
#define SYS_SENDTO         44
#define SYS_RECVFROM       45
#define SYS_SENDMSG        46
#define SYS_RECVMSG        47
#define SYS_SHUTDOWN       48
#define SYS_BIND           49
#define SYS_LISTEN         50
#define SYS_ACCEPT4        288
#define SYS_FORK           57
#define SYS_VFORK          58
#define SYS_EXECVE         59
#define SYS_EXIT           60
#define SYS_WAIT4          61
#define SYS_KILL           62
#define SYS_UNAME          63
#define SYS_FCNTL          72
#define SYS_GETCWD         79
#define SYS_CHDIR          80
#define SYS_FCHDIR         81
#define SYS_RENAMEAT       82
#define SYS_MKDIRAT        83
#define SYS_UNLINKAT       87
#define SYS_READLINKAT     88
#define SYS_GETTIMEOFDAY   96
#define SYS_GETUID        102
#define SYS_GETGID        104
#define SYS_SETUID        105
#define SYS_SETGID        106
#define SYS_GETEUID       107
#define SYS_GETEGID       108
#define SYS_SETEUID       109
#define SYS_GETPPID       110
#define SYS_SETSID        112
#define SYS_GETTID        186
#define SYS_TKILL         200
#define SYS_FUTEX         202
#define SYS_SET_TID_ADDRESS 218
#define SYS_GETDENTS64    217
#define SYS_CLOCK_GETTIME 228
#define SYS_CLOCK_NANOSLEEP 230
#define SYS_EXIT_GROUP    231
#define SYS_OPENAT        257
#define SYS_NEWFSTATAT    262
#define SYS_FACCESSAT     269
#define SYS_DUP3          292
#define SYS_PIPE2         293
#define SYS_PRLIMIT64     302
#define SYS_RENAMEAT2     311
#define SYS_GETRANDOM     318
#define SYS_ARCH_PRCTL    158

/* ------------------------------------------------------------------ */
/* Raw syscall stubs                                                   */
/* ------------------------------------------------------------------ */

static inline long __syscall1(long n, long a1) {
    long ret;
    __asm__ volatile (
        "syscall"
        : "=a"(ret)
        : "a"(n), "D"(a1)
        : "rcx", "r11", "memory"
    );
    return ret;
}

static inline long __syscall2(long n, long a1, long a2) {
    long ret;
    __asm__ volatile (
        "syscall"
        : "=a"(ret)
        : "a"(n), "D"(a1), "S"(a2)
        : "rcx", "r11", "memory"
    );
    return ret;
}

static inline long __syscall3(long n, long a1, long a2, long a3) {
    long ret;
    __asm__ volatile (
        "syscall"
        : "=a"(ret)
        : "a"(n), "D"(a1), "S"(a2), "d"(a3)
        : "rcx", "r11", "memory"
    );
    return ret;
}

static inline long __syscall4(long n, long a1, long a2, long a3, long a4) {
    long ret;
    __asm__ volatile (
        "syscall"
        : "=a"(ret)
        : "a"(n), "D"(a1), "S"(a2), "d"(a3), "r"(a4)
        : "rcx", "r11", "memory"
    );
    return ret;
}

static inline long __syscall5(long n, long a1, long a2, long a3, long a4, long a5) {
    long ret;
    register long r10 __asm__("r10") = a4;
    register long r8 __asm__("r8") = a5;
    __asm__ volatile (
        "syscall"
        : "=a"(ret)
        : "a"(n), "D"(a1), "S"(a2), "d"(a3), "r"(r10), "r"(r8)
        : "rcx", "r11", "memory"
    );
    return ret;
}

static inline long __syscall6(long n, long a1, long a2, long a3, long a4, long a5, long a6) {
    long ret;
    register long r10 __asm__("r10") = a4;
    register long r8 __asm__("r8") = a5;
    register long r9 __asm__("r9") = a6;
    __asm__ volatile (
        "syscall"
        : "=a"(ret)
        : "a"(n), "D"(a1), "S"(a2), "d"(a3), "r"(r10), "r"(r8), "r"(r9)
        : "rcx", "r11", "memory"
    );
    return ret;
}

/* ------------------------------------------------------------------ */
/* errno (thread-local, but we are uniprocessor)                       */
/* ------------------------------------------------------------------ */
static long __errno_storage = 0;
#define errno __errno_storage

static void __set_errno(long val) {
    if (val < 0) {
        errno = -val;
    } else {
        errno = 0;
    }
}

/* ------------------------------------------------------------------ */
/* Syscall wrappers                                                    */
/* ------------------------------------------------------------------ */

/* I/O */
ssize_t read(int fd, void *buf, size_t count) {
    long r = __syscall3(SYS_READ, fd, (long)buf, count);
    if (r < 0) { __set_errno(r); return -1; }
    return r;
}

ssize_t write(int fd, const void *buf, size_t count) {
    long r = __syscall3(SYS_WRITE, fd, (long)buf, count);
    if (r < 0) { __set_errno(r); return -1; }
    return r;
}

ssize_t writev(int fd, const struct iovec *iov, int iovcnt) {
    long r = __syscall3(SYS_WRITEV, fd, (long)iov, iovcnt);
    if (r < 0) { __set_errno(r); return -1; }
    return r;
}

ssize_t readv(int fd, const struct iovec *iov, int iovcnt) {
    long r = __syscall3(SYS_READV, fd, (long)iov, iovcnt);
    if (r < 0) { __set_errno(r); return -1; }
    return r;
}

int close(int fd) {
    long r = __syscall1(SYS_CLOSE, fd);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

off_t lseek(int fd, off_t offset, int whence) {
    long r = __syscall3(SYS_LSEEK, fd, offset, whence);
    if (r < 0) { __set_errno(r); return -1; }
    return r;
}

int open(const char *path, int flags, ...) {
    long mode = 0;
    /* mode is optional, not always present */
    __asm__ volatile("" : "=r"(mode));
    long r = __syscall3(SYS_OPEN, (long)path, flags, mode);
    if (r < 0) { __set_errno(r); return -1; }
    return (int)r;
}

int openat(int dirfd, const char *path, int flags, ...) {
    long mode = 0;
    __asm__ volatile("" : "=r"(mode));
    long r = __syscall4(SYS_OPENAT, dirfd, (long)path, flags, mode);
    if (r < 0) { __set_errno(r); return -1; }
    return (int)r;
}

int pipe(int pipefd[2]) {
    long r = __syscall1(SYS_PIPE, (long)pipefd);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int pipe2(int pipefd[2], int flags) {
    long r = __syscall2(SYS_PIPE2, (long)pipefd, flags);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int dup(int oldfd) {
    long r = __syscall1(SYS_DUP, oldfd);
    if (r < 0) { __set_errno(r); return -1; }
    return (int)r;
}

int dup2(int oldfd, int newfd) {
    long r = __syscall2(SYS_DUP2, oldfd, newfd);
    if (r < 0) { __set_errno(r); return -1; }
    return (int)r;
}

int dup3(int oldfd, int newfd, int flags) {
    long r = __syscall3(SYS_DUP3, oldfd, newfd, flags);
    if (r < 0) { __set_errno(r); return -1; }
    return (int)r;
}

int fcntl(int fd, int cmd, ...) {
    long arg = 0;
    __asm__ volatile("" : "=r"(arg));
    long r = __syscall3(SYS_FCNTL, fd, cmd, arg);
    if (r < 0) { __set_errno(r); return -1; }
    return (int)r;
}

int ioctl(int fd, unsigned long request, ...) {
    long arg = 0;
    __asm__ volatile("" : "=r"(arg));
    long r = __syscall3(SYS_IOCTL, fd, request, arg);
    if (r < 0) { __set_errno(r); return -1; }
    return (int)r;
}

/* File info */
int stat(const char *path, struct stat *buf) {
    long r = __syscall2(SYS_STAT, (long)path, (long)buf);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int fstat(int fd, struct stat *buf) {
    long r = __syscall2(SYS_FSTAT, fd, (long)buf);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int lstat(const char *path, struct stat *buf) {
    long r = __syscall2(SYS_LSTAT, (long)path, (long)buf);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int fstatat(int dirfd, const char *path, struct stat *buf, int flags) {
    long r = __syscall4(SYS_NEWFSTATAT, dirfd, (long)path, (long)buf, flags);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

/* Directory */
int getdents64(int fd, void *dirp, size_t count) {
    long r = __syscall3(SYS_GETDENTS64, fd, (long)dirp, count);
    if (r < 0) { __set_errno(r); return -1; }
    return (int)r;
}

/* Path operations */
int access(const char *path, int mode) {
    long r = __syscall2(SYS_ACCESS, (long)path, mode);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int faccessat(int dirfd, const char *path, int mode, int flags) {
    long r = __syscall4(SYS_FACCESSAT, dirfd, (long)path, mode, flags);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int chdir(const char *path) {
    long r = __syscall1(SYS_CHDIR, (long)path);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int fchdir(int fd) {
    long r = __syscall1(SYS_FCHDIR, fd);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

char *getcwd(char *buf, size_t size) {
    long r = __syscall2(SYS_GETCWD, (long)buf, size);
    if (r < 0) { __set_errno(r); return NULL; }
    return buf;
}

int unlink(const char *path) {
    long r = __syscall2(SYS_UNLINKAT, AT_FDCWD, (long)path, 0);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int unlinkat(int dirfd, const char *path, int flags) {
    long r = __syscall3(SYS_UNLINKAT, dirfd, (long)path, flags);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int mkdir(const char *path, mode_t mode) {
    long r = __syscall3(SYS_MKDIRAT, AT_FDCWD, (long)path, mode);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int mkdirat(int dirfd, const char *path, mode_t mode) {
    long r = __syscall3(SYS_MKDIRAT, dirfd, (long)path, mode);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int rename(const char *old, const char *new) {
    long r = __syscall5(SYS_RENAMEAT2, AT_FDCWD, (long)old, AT_FDCWD, (long)new, 0);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int renameat(int olddirfd, const char *old, int newdirfd, const char *new) {
    long r = __syscall5(SYS_RENAMEAT2, olddirfd, (long)old, newdirfd, (long)new, 0);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int renameat2(int olddirfd, const char *old, int newdirfd, const char *new, unsigned flags) {
    long r = __syscall5(SYS_RENAMEAT2, olddirfd, (long)old, newdirfd, (long)new, flags);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

ssize_t readlinkat(int dirfd, const char *path, char *buf, size_t bufsiz) {
    long r = __syscall4(SYS_READLINKAT, dirfd, (long)path, (long)buf, bufsiz);
    if (r < 0) { __set_errno(r); return -1; }
    return r;
}

/* Process */
_Noreturn void exit(int code) {
    __syscall1(SYS_EXIT, code);
    __builtin_unreachable();
}

_Noreturn void exit_group(int code) {
    __syscall1(SYS_EXIT_GROUP, code);
    __builtin_unreachable();
}

pid_t getpid(void) {
    return (pid_t)__syscall0(SYS_GETPID);
}

pid_t getppid(void) {
    return (pid_t)__syscall0(SYS_GETPPID);
}

pid_t gettid(void) {
    return (pid_t)__syscall0(SYS_GETTID);
}

uid_t getuid(void) { return (uid_t)__syscall0(SYS_GETUID); }
uid_t geteuid(void) { return (uid_t)__syscall0(SYS_GETEUID); }
gid_t getgid(void) { return (gid_t)__syscall0(SYS_GETGID); }
gid_t getegid(void) { return (gid_t)__syscall0(SYS_GETEGID); }
int setuid(uid_t uid) { return (int)__syscall1(SYS_SETUID, uid); }
int setgid(gid_t gid) { return (int)__syscall1(SYS_SETGID, gid); }
int seteuid(uid_t uid) { return (int)__syscall1(SYS_SETEUID, uid); }
int setsid(void) { return (int)__syscall0(SYS_SETSID); }

int kill(pid_t pid, int sig) {
    long r = __syscall2(SYS_KILL, pid, sig);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int tgid(pid_t tid, int sig) {
    long r = __syscall2(SYS_TKILL, tid, sig);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int execve(const char *path, char *const argv[], char *const envp[]) {
    long r = __syscall3(SYS_EXECVE, (long)path, (long)argv, (long)envp);
    __set_errno(r);
    return -1;
}

pid_t fork(void) {
    long r = __syscall0(SYS_FORK);
    if (r < 0) { __set_errno(r); return -1; }
    return (pid_t)r;
}

pid_t vfork(void) {
    long r = __syscall0(SYS_VFORK);
    if (r < 0) { __set_errno(r); return -1; }
    return (pid_t)r;
}

pid_t wait4(pid_t pid, int *wstatus, int options, void *rusage) {
    long r = __syscall4(SYS_WAIT4, pid, (long)wstatus, options, (long)rusage);
    if (r < 0) { __set_errno(r); return -1; }
    return (pid_t)r;
}

pid_t waitpid(pid_t pid, int *wstatus, int options) {
    return wait4(pid, wstatus, options, NULL);
}

/* Signals */
int rt_sigaction(int signum, const void *act, void *oldact, size_t sigsetsize) {
    long r = __syscall4(SYS_RT_SIGACTION, signum, (long)act, (long)oldact, sigsetsize);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int rt_sigprocmask(int how, const void *set, void *oldset, size_t sigsetsize) {
    long r = __syscall4(SYS_RT_SIGPROCMASK, how, (long)set, (long)oldset, sigsetsize);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

/* Time */
int nanosleep(const struct timespec *req, struct timespec *rem) {
    long r = __syscall2(SYS_NANOSLEEP, (long)req, (long)rem);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int clock_gettime(int clk_id, struct timespec *tp) {
    long r = __syscall2(SYS_CLOCK_GETTIME, clk_id, (long)tp);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int gettimeofday(struct timeval *tv, void *tz) {
    long r = __syscall2(SYS_GETTIMEOFDAY, (long)tv, (long)tz);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int clock_nanosleep(int clock_id, int flags, const struct timespec *req, struct timespec *rem) {
    long r = __syscall4(SYS_CLOCK_NANOSLEEP, clock_id, flags, (long)req, (long)rem);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

/* Memory */
void *mmap(void *addr, size_t length, int prot, int flags, int fd, off_t offset) {
    long r = __syscall6(SYS_MMAP, (long)addr, length, prot, flags, fd, offset);
    if (r < 0) { __set_errno(r); return (void*)-1; }
    return (void*)r;
}

int munmap(void *addr, size_t length) {
    long r = __syscall2(SYS_MUNMAP, (long)addr, length);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

int mprotect(void *addr, size_t length, int prot) {
    long r = __syscall3(SYS_MPROTECT, (long)addr, length, prot);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

void *brk(void *addr) {
    long r = __syscall1(SYS_BRK, (long)addr);
    return (void*)r;
}

/* Futex */
int futex(int *uaddr, int op, int val, const struct timespec *timeout, int *uaddr2, int val3) {
    long r = __syscall6(SYS_FUTEX, (long)uaddr, op, val, (long)timeout, (long)uaddr2, val3);
    if (r < 0) { __set_errno(r); return -1; }
    return (int)r;
}

int set_tid_address(int *tidptr) {
    return (int)__syscall1(SYS_SET_TID_ADDRESS, (long)tidptr);
}

/* Random */
ssize_t getrandom(void *buf, size_t buflen, unsigned flags) {
    long r = __syscall3(SYS_GETRANDOM, (long)buf, buflen, flags);
    if (r < 0) { __set_errno(r); return -1; }
    return r;
}

/* Resource limits */
int prlimit64(pid_t pid, int resource, const void *new_limit, void *old_limit) {
    long r = __syscall4(SYS_PRLIMIT64, pid, resource, (long)new_limit, (long)old_limit);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

/* uname */
int uname(void *buf) {
    long r = __syscall1(SYS_UNAME, (long)buf);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

/* arch_prctl */
int arch_prctl(int code, unsigned long addr) {
    long r = __syscall2(SYS_ARCH_PRCTL, code, addr);
    if (r < 0) { __set_errno(r); return -1; }
    return 0;
}

/* ------------------------------------------------------------------ */
/* Minimal string / memory functions                                   */
/* ------------------------------------------------------------------ */

void *memcpy(void *dst, const void *src, size_t n) {
    unsigned char *d = dst;
    const unsigned char *s = src;
    while (n--) *d++ = *s++;
    return dst;
}

void *memmove(void *dst, const void *src, size_t n) {
    unsigned char *d = dst;
    const unsigned char *s = src;
    if (d < s) {
        while (n--) *d++ = *s++;
    } else {
        d += n; s += n;
        while (n--) *--d = *--s;
    }
    return dst;
}

void *memset(void *dst, int c, size_t n) {
    unsigned char *d = dst;
    while (n--) *d++ = (unsigned char)c;
    return dst;
}

int memcmp(const void *a, const void *b, size_t n) {
    const unsigned char *pa = a, *pb = b;
    while (n--) {
        if (*pa != *pb) return *pa - *pb;
        pa++; pb++;
    }
    return 0;
}

size_t strlen(const char *s) {
    size_t n = 0;
    while (*s++) n++;
    return n;
}

size_t strnlen(const char *s, size_t maxlen) {
    size_t n = 0;
    while (n < maxlen && s[n]) n++;
    return n;
}

char *strcpy(char *dst, const char *src) {
    char *d = dst;
    while ((*d++ = *src++));
    return dst;
}

char *strncpy(char *dst, const char *src, size_t n) {
    char *d = dst;
    while (n && (*d++ = *src++)) n--;
    while (n--) *d++ = 0;
    return dst;
}

int strcmp(const char *a, const char *b) {
    while (*a && *a == *b) { a++; b++; }
    return (unsigned char)*a - (unsigned char)*b;
}

int strncmp(const char *a, const char *b, size_t n) {
    while (n && *a && *a == *b) { a++; b++; n--; }
    return n ? (unsigned char)*a - (unsigned char)*b : 0;
}

char *strcat(char *dst, const char *src) {
    char *d = dst;
    while (*d) d++;
    while ((*d++ = *src++));
    return dst;
}

char *strchr(const char *s, int c) {
    while (*s) {
        if (*s == (char)c) return (char*)s;
        s++;
    }
    return c == 0 ? (char*)s : NULL;
}

char *strrchr(const char *s, int c) {
    const char *last = NULL;
    while (*s) {
        if (*s == (char)c) last = s;
        s++;
    }
    return c == 0 ? (char*)s : (char*)last;
}

char *strstr(const char *haystack, const char *needle) {
    if (!*needle) return (char*)haystack;
    size_t nlen = strlen(needle);
    while (*haystack) {
        if (strncmp(haystack, needle, nlen) == 0) return (char*)haystack;
        haystack++;
    }
    return NULL;
}

char *strdup(const char *s) {
    size_t len = strlen(s) + 1;
    char *d = malloc(len);
    if (d) memcpy(d, s, len);
    return d;
}

char *strndup(const char *s, size_t n) {
    size_t len = strnlen(s, n);
    char *d = malloc(len + 1);
    if (d) {
        memcpy(d, s, len);
        d[len] = 0;
    }
    return d;
}

/* ------------------------------------------------------------------ */
/* Minimal printf / snprintf                                           */
/* ------------------------------------------------------------------ */

/* Forward declaration */
static void __snputs(void (*out)(char, void*), void *ctx, const char *s, size_t len);

static void __snput_char(char c, void *ctx) {
    char **p = (char**)ctx;
    *(*p)++ = c;
}

static int __vsnprintf_internal(char *buf, size_t maxlen, const char *fmt, void **ap) {
    size_t pos = 0;
    #define EMIT(c) do { if (pos < maxlen - 1) buf[pos] = (c); pos++; } while(0)
    #define EMIT_STR(s, len) do { \
        size_t _l = (len); \
        for (size_t _i = 0; _i < _l; _i++) { \
            if (pos < maxlen - 1) buf[pos] = (s)[_i]; \
            pos++; \
        } \
    } while(0)

    while (*fmt) {
        if (*fmt != '%') { EMIT(*fmt); fmt++; continue; }
        fmt++; /* skip '%' */

        /* flags */
        int left = 0, zero = 0, plus = 0, space = 0, hash = 0;
        for (;;) {
            switch (*fmt) {
                case '-': left = 1; fmt++; continue;
                case '0': zero = 1; fmt++; continue;
                case '+': plus = 1; fmt++; continue;
                case ' ': space = 1; fmt++; continue;
                case '#': hash = 1; fmt++; continue;
            }
            break;
        }

        /* width */
        int width = 0;
        if (*fmt == '*') {
            width = (int)(long)*ap++;
            if (width < 0) { left = 1; width = -width; }
            fmt++;
        } else {
            while (*fmt >= '0' && *fmt <= '9') width = width * 10 + (*fmt++ - '0');
        }

        /* precision */
        int prec = -1;
        if (*fmt == '.') {
            fmt++;
            prec = 0;
            if (*fmt == '*') {
                prec = (int)(long)*ap++;
                fmt++;
            } else {
                while (*fmt >= '0' && *fmt <= '9') prec = prec * 10 + (*fmt++ - '0');
            }
        }

        /* length modifier */
        int long_flag = 0, long_long = 0, size_t_flag = 0, short_flag = 0;
        while (*fmt == 'l' || *fmt == 'z' || *fmt == 'h') {
            if (*fmt == 'l') { fmt++; long_flag++; if (*fmt == 'l') { long_long++; fmt++; } }
            else if (*fmt == 'z') { size_t_flag++; fmt++; }
            else if (*fmt == 'h') { short_flag++; fmt++; }
        }

        char tmp[32];
        int tpos = 0;

        switch (*fmt) {
        case 'd': case 'i': {
            long val;
            if (long_flag) val = (long)*ap++;
            else if (long_long) val = (long)*ap++;
            else val = (int)(long)*ap++;
            if (short_flag) val = (short)val;
            if (val < 0) { EMIT('-'); val = -val; }
            else if (plus) EMIT('+');
            else if (space) EMIT(' ');
            if (val == 0) { tmp[tpos++] = '0'; }
            else {
                while (val > 0) { tmp[tpos++] = '0' + (val % 10); val /= 10; }
            }
            for (int i = tpos - 1; i >= 0; i--) EMIT(tmp[i]);
            break;
        }
        case 'u': {
            unsigned long val;
            if (long_flag) val = (unsigned long)*ap++;
            else if (size_t_flag) val = (size_t)*ap++;
            else val = (unsigned)(long)*ap++;
            if (val == 0) { tmp[tpos++] = '0'; }
            else {
                while (val > 0) { tmp[tpos++] = '0' + (val % 10); val /= 10; }
            }
            for (int i = tpos - 1; i >= 0; i--) EMIT(tmp[i]);
            break;
        }
        case 'x': case 'X': {
            const char *hex = (*fmt == 'x') ? "0123456789abcdef" : "0123456789ABCDEF";
            unsigned long val;
            if (long_flag) val = (unsigned long)*ap++;
            else if (size_t_flag) val = (size_t)*ap++;
            else val = (unsigned)(long)*ap++;
            if (hash && val != 0) { EMIT('0'); EMIT(*fmt); }
            if (val == 0) { tmp[tpos++] = '0'; }
            else {
                while (val > 0) { tmp[tpos++] = hex[val & 0xf]; val >>= 4; }
            }
            for (int i = tpos - 1; i >= 0; i--) EMIT(tmp[i]);
            break;
        }
        case 'o': {
            unsigned long val;
            if (long_flag) val = (unsigned long)*ap++;
            else val = (unsigned)(long)*ap++;
            if (hash && val != 0) EMIT('0');
            if (val == 0) { tmp[tpos++] = '0'; }
            else {
                while (val > 0) { tmp[tpos++] = '0' + (val & 7); val >>= 3; }
            }
            for (int i = tpos - 1; i >= 0; i--) EMIT(tmp[i]);
            break;
        }
        case 'p': {
            unsigned long val = (unsigned long)*ap++;
            const char *hex = "0123456789abcdef";
            EMIT('0'); EMIT('x');
            if (val == 0) { tmp[tpos++] = '0'; }
            else {
                while (val > 0) { tmp[tpos++] = hex[val & 0xf]; val >>= 4; }
            }
            for (int i = tpos - 1; i >= 0; i--) EMIT(tmp[i]);
            break;
        }
        case 's': {
            const char *s = (const char*)*ap++;
            if (!s) s = "(null)";
            size_t len = strlen(s);
            if (prec >= 0 && (size_t)prec < len) len = prec;
            EMIT_STR(s, len);
            break;
        }
        case 'c': {
            char c = (char)(int)(long)*ap++;
            EMIT(c);
            break;
        }
        case '%':
            EMIT('%');
            break;
        case 'n': {
            int *p = (int*)*ap++;
            *p = (int)pos;
            break;
        }
        default:
            EMIT('%'); EMIT(*fmt);
            break;
        }
        fmt++;
    }
    if (maxlen > 0) buf[(pos < maxlen - 1) ? pos : maxlen - 1] = 0;
    #undef EMIT
    #undef EMIT_STR
    return (int)pos;
}

int snprintf(char *buf, size_t maxlen, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int ret = __vsnprintf_internal(buf, maxlen, fmt, ap.__stack);
    va_end(ap);
    return ret;
}

int vsnprintf(char *buf, size_t maxlen, const char *fmt, va_list ap) {
    return __vsnprintf_internal(buf, maxlen, fmt, ap.__stack);
}

int printf(const char *fmt, ...) {
    char buf[4096];
    va_list ap;
    va_start(ap, fmt);
    int ret = __vsnprintf_internal(buf, sizeof(buf), fmt, ap.__stack);
    va_end(ap);
    if (ret > 0) write(1, buf, ret < (int)sizeof(buf) ? ret : sizeof(buf) - 1);
    return ret;
}

int fprintf(void *f, const char *fmt, ...) {
    /* Only support stderr (fd 2) and stdout (fd 1) */
    int fd = (f == NULL) ? 2 : 1;
    char buf[4096];
    va_list ap;
    va_start(ap, fmt);
    int ret = __vsnprintf_internal(buf, sizeof(buf), fmt, ap.__stack);
    va_end(ap);
    if (ret > 0) write(fd, buf, ret < (int)sizeof(buf) ? ret : sizeof(buf) - 1);
    return ret;
}

int sprintf(char *buf, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int ret = __vsnprintf_internal(buf, (size_t)-1, fmt, ap.__stack);
    va_end(ap);
    return ret;
}

int vfprintf(void *f, const char *fmt, va_list ap) {
    int fd = (f == NULL) ? 2 : 1;
    char buf[4096];
    int ret = __vsnprintf_internal(buf, sizeof(buf), fmt, ap.__stack);
    if (ret > 0) write(fd, buf, ret < (int)sizeof(buf) ? ret : sizeof(buf) - 1);
    return ret;
}

int vprintf(const char *fmt, va_list ap) {
    return vfprintf(NULL, fmt, ap);
}

/* ------------------------------------------------------------------ */
/* Memory allocator (simple bump + free list)                          */
/* ------------------------------------------------------------------ */

#define HEAP_SIZE (4 * 1024 * 1024)
static unsigned char heap[HEAP_SIZE];
static size_t heap_ptr = 0;

typedef struct free_block {
    size_t size;
    struct free_block *next;
} free_block_t;

static free_block_t *free_list = NULL;

void *malloc(size_t size) {
    if (size == 0) return NULL;
    size = (size + 15) & ~15; /* 16-byte align */

    /* Try free list first */
    free_block_t **prev = &free_list;
    free_block_t *blk = free_list;
    while (blk) {
        if (blk->size >= size) {
            *prev = blk->next;
            return (void*)(blk + 1);
        }
        prev = &blk->next;
        blk = blk->next;
    }

    /* Bump allocate */
    if (heap_ptr + size + sizeof(free_block_t) <= HEAP_SIZE) {
        void *p = &heap[heap_ptr];
        heap_ptr += size;
        return p;
    }

    return NULL;
}

void free(void *ptr) {
    if (!ptr) return;
    free_block_t *blk = ((free_block_t*)ptr) - 1;
    blk->next = free_list;
    free_list = blk;
}

void *calloc(size_t nmemb, size_t size) {
    size_t total = nmemb * size;
    void *p = malloc(total);
    if (p) memset(p, 0, total);
    return p;
}

void *realloc(void *ptr, size_t size) {
    if (!ptr) return malloc(size);
    if (size == 0) { free(ptr); return NULL; }
    void *new = malloc(size);
    if (new) {
        /* We don't know old size, copy up to new size */
        memcpy(new, ptr, size);
        free(ptr);
    }
    return new;
}

size_t malloc_usable_size(void *ptr) {
    return 0; /* stub */
}

/* ------------------------------------------------------------------ */
/* errno                                                               */
/* ------------------------------------------------------------------ */
int *__errno_location(void) {
    return (int*)&__errno_storage;
}

/* ------------------------------------------------------------------ */
/* setjmp / longjmp stubs (for error handling)                         */
/* ------------------------------------------------------------------ */
typedef struct { unsigned long __jmp_buf[8]; } jmp_buf[1];

int setjmp(jmp_buf env) {
    (void)env;
    return 0;
}

void longjmp(jmp_buf env, int val) {
    (void)env;
    (void)val;
    /* Cannot actually longjmp in this minimal implementation */
    exit(1);
}

/* ------------------------------------------------------------------ */
/* AT_FDCWD                                                            */
/* ------------------------------------------------------------------ */
#ifndef AT_FDCWD
#define AT_FDCWD (-100)
#endif

/* ------------------------------------------------------------------ */
/* Stub functions that uutils/musl might reference                     */
/* ------------------------------------------------------------------ */
int raise(int sig) { return kill(getpid(), sig); }
int pthread_sigmask(int how, const void *set, void *oldset) {
    return rt_sigprocmask(how, set, oldset, 8);
}

/* SBRK-like: we use mmap/brk, but some code may reference sbrk */
void *sbrk(intptr_t increment) {
    static void *brk_addr = (void*)0x60000000;
    void *old = brk_addr;
    brk_addr = (void*)((char*)brk_addr + increment);
    return old;
}
