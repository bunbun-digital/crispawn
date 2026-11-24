/* SPDX-License-Identifier: Apache-2.0 */
/* Test-process-only tracing and syncfs fault injection. Never linked into ISO. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static void record(const char *operation, int fd) {
    const char *trace = getenv("CRISPAWN_TEST_SYNC_TRACE");
    if (!trace) return;
    char link[64], path[PATH_MAX], line[PATH_MAX + 128];
    snprintf(link, sizeof link, "/proc/self/fd/%d", fd);
    ssize_t n = readlink(link, path, sizeof path - 1);
    if (n < 0) n = 0;
    path[n] = 0;
    const char *archive = getenv("CRISPAWN_TEST_ARCHIVE");
    char marker[PATH_MAX];
    snprintf(marker, sizeof marker, "%s/COMMITTED", archive ? archive : "");
    int count = snprintf(line, sizeof line, "%s\t%s\tcommitted=%d\n",
                         operation, path, access(marker, F_OK) == 0);
    int output = syscall(SYS_openat, AT_FDCWD, trace, O_WRONLY | O_APPEND | O_CREAT, 0600);
    if (output >= 0) {
        syscall(SYS_write, output, line, count);
        syscall(SYS_close, output);
    }
}
int fsync(int fd) {
    record("fsync", fd);
    int (*real)(int) = dlsym(RTLD_NEXT, "fsync");
    return real(fd);
}
int fdatasync(int fd) {
    record("fdatasync", fd);
    int (*real)(int) = dlsym(RTLD_NEXT, "fdatasync");
    return real(fd);
}
int syncfs(int fd) {
    record("syncfs", fd);
    static int failed;
    if (getenv("CRISPAWN_TEST_SYNCFS_EIO") && !__sync_lock_test_and_set(&failed, 1)) { errno = EIO; return -1; }
    int (*real)(int) = dlsym(RTLD_NEXT, "syncfs");
    return real(fd);
}
static int part(int fd) {
    char link[64], path[PATH_MAX];
    snprintf(link, sizeof link, "/proc/self/fd/%d", fd);
    ssize_t n = readlink(link, path, sizeof path - 1);
    if (n < 0) return 0;
    path[n] = 0;
    return strstr(path, "/segments/") != NULL;
}
ssize_t pread(int fd, void *bytes, size_t count, off_t offset) {
    if (part(fd)) record("pread", fd);
    ssize_t (*real)(int, void *, size_t, off_t) = dlsym(RTLD_NEXT, "pread");
    return real(fd, bytes, count, offset);
}
ssize_t pread64(int fd, void *bytes, size_t count, off64_t offset) {
    if (part(fd)) record("pread64", fd);
    ssize_t (*real)(int, void *, size_t, off64_t) = dlsym(RTLD_NEXT, "pread64");
    return real(fd, bytes, count, offset);
}
