/* Linux-only helper for precise per-child wall, CPU and peak RSS sampling.
 * A small C parent avoids inflated wait4 RSS from Python posix_spawn/CLONE_VM.
 * Run only trusted, already-validated benchmark executables.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static long long nanos(struct timespec t) {
    return (long long)t.tv_sec * 1000000000LL + t.tv_nsec;
}

static int parse_count(const char *arg, int minimum, int *out) {
    char *end = NULL;
    errno = 0;
    long value = strtol(arg, &end, 10);
    if (errno || end == arg || *end || value < minimum || value > 2000) return -1;
    *out = (int)value;
    return 0;
}

int main(int argc, char **argv) {
    int runs, warmup;
    if (argc != 4 || parse_count(argv[2], 1, &runs) || parse_count(argv[3], 0, &warmup)) {
        fprintf(stderr, "usage: native_runtime_probe EXECUTABLE RUNS(1..2000) WARMUP(0..2000)\n");
        return 2;
    }
    int devnull = open("/dev/null", O_WRONLY | O_CLOEXEC);
    if (devnull < 0) { perror("open /dev/null"); return 2; }
    for (int i = 0; i < runs + warmup; i++) {
        struct timespec start, stop;
        struct rusage usage;
        if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) { perror("clock_gettime"); return 2; }
        pid_t child = fork();
        if (child < 0) { perror("fork"); return 2; }
        if (child == 0) {
            if (dup2(devnull, STDOUT_FILENO) < 0 || dup2(devnull, STDERR_FILENO) < 0) _exit(127);
            close(devnull);
            execl(argv[1], argv[1], (char *)NULL);
            _exit(127);
        }
        int status;
        pid_t completed;
        do { completed = wait4(child, &status, 0, &usage); } while (completed < 0 && errno == EINTR);
        if (completed != child) { perror("wait4"); return 2; }
        if (clock_gettime(CLOCK_MONOTONIC, &stop) != 0) { perror("clock_gettime"); return 2; }
        if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
            if (WIFEXITED(status)) fprintf(stderr, "nonzero exit from %s: exit %d\n", argv[1], WEXITSTATUS(status));
            else if (WIFSIGNALED(status)) fprintf(stderr, "nonzero exit from %s: signal %d\n", argv[1], WTERMSIG(status));
            else fprintf(stderr, "nonzero exit from %s: unknown status\n", argv[1]);
            return 1;
        }
        if (i >= warmup) {
            long long wall = nanos(stop) - nanos(start);
            long long cpu = (long long)usage.ru_utime.tv_sec * 1000000000LL
                          + (long long)usage.ru_utime.tv_usec * 1000LL
                          + (long long)usage.ru_stime.tv_sec * 1000000000LL
                          + (long long)usage.ru_stime.tv_usec * 1000LL;
            printf("%lld\t%lld\t%ld\n", wall, cpu, usage.ru_maxrss);
        }
    }
    close(devnull);
    return 0;
}
