/*
 * Linux-only SQLite WAL syscall fault shim for disposable test subprocesses.
 *
 * Preload ONLY in the rusqlite_atomic_fixture process, NEVER globally.
 * Match exactly NULANG_SQLITE_FAULT_DB_PATH + "-wal" before intercepting.
 * This exercises SQLite's handling of ENOSPC and EIO from libc syscalls;
 * it does not claim physical disk exhaustion or a failing hardware fsync.
 */
#define _GNU_SOURCE
#define _LARGEFILE64_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <limits.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

static int targeted_wal_fd(int fd) {
    const char *db = getenv("NULANG_SQLITE_FAULT_DB_PATH");
    if (!db || db[0] != '/' || fd < 0) return 0;
    size_t len = strlen(db);
    if (len == 0 || len >= PATH_MAX - 5) return 0;
    char fd_path[64];
    int sz = snprintf(fd_path, sizeof(fd_path), "/proc/self/fd/%d", fd);
    if (sz <= 0 || (size_t)sz >= sizeof(fd_path)) return 0;
    char resolved[PATH_MAX];
    ssize_t n = readlink(fd_path, resolved, sizeof(resolved) - 1);
    if (n <= 0 || (size_t)n >= sizeof(resolved)) return 0;
    resolved[n] = '\0';
    return (size_t)n == len + 4 &&
        memcmp(resolved, db, len) == 0 &&
        memcmp(resolved + len, "-wal", 5) == 0;
}
static int target_mode(const char *mode, int fd) {
    const char *selected = getenv("NULANG_SQLITE_FAULT_MODE");
    return selected && strcmp(selected, mode) == 0 && targeted_wal_fd(fd);
}
static void announce(const char *message) {
    (void)syscall(SYS_write, STDERR_FILENO, message, strlen(message));
}

ssize_t pwrite(int fd, const void *data, size_t count, off_t offset) {
    if (target_mode("enospc-write", fd)) {
        announce("NULANG_FAULT_INJECTED_ENOSPC_WAL_WRITE\n");
        errno = ENOSPC;
        return -1;
    }
    ssize_t (*real_pwrite)(int, const void *, size_t, off_t) = dlsym(RTLD_NEXT, "pwrite");
    if (!real_pwrite) { errno = ENOSYS; return -1; }
    return real_pwrite(fd, data, count, offset);
}

ssize_t pwrite64(int fd, const void *data, size_t count, off64_t offset) {
    if (target_mode("enospc-write", fd)) {
        announce("NULANG_FAULT_INJECTED_ENOSPC_WAL_WRITE\n");
        errno = ENOSPC;
        return -1;
    }
    ssize_t (*real_pwrite64)(int, const void *, size_t, off64_t) = dlsym(RTLD_NEXT, "pwrite64");
    if (!real_pwrite64) { errno = ENOSYS; return -1; }
    return real_pwrite64(fd, data, count, offset);
}

ssize_t write(int fd, const void *data, size_t count) {
    if (target_mode("enospc-write", fd)) {
        announce("NULANG_FAULT_INJECTED_ENOSPC_WAL_WRITE\n");
        errno = ENOSPC;
        return -1;
    }
    ssize_t (*real_write)(int, const void *, size_t) = dlsym(RTLD_NEXT, "write");
    if (!real_write) { errno = ENOSYS; return -1; }
    return real_write(fd, data, count);
}

int fdatasync(int fd) {
    if (target_mode("eio-sync", fd)) {
        announce("NULANG_FAULT_INJECTED_EIO_WAL_SYNC\n");
        errno = EIO;
        return -1;
    }
    int (*real_fdatasync)(int) = dlsym(RTLD_NEXT, "fdatasync");
    if (!real_fdatasync) { errno = ENOSYS; return -1; }
    return real_fdatasync(fd);
}

int fsync(int fd) {
    if (target_mode("eio-sync", fd)) {
        announce("NULANG_FAULT_INJECTED_EIO_WAL_SYNC\n");
        errno = EIO;
        return -1;
    }
    int (*real_fsync)(int) = dlsym(RTLD_NEXT, "fsync");
    if (!real_fsync) { errno = ENOSYS; return -1; }
    return real_fsync(fd);
}
