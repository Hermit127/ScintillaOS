#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

enum { THREADS = 8, LOOPS = 20000 };

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static uint64_t counter;

static void *worker(void *arg) {
    (void)arg;
    for (int i = 0; i < LOOPS; ++i) {
        if (pthread_mutex_lock(&lock) || pthread_mutex_unlock(&lock))
            return (void *)1;
        __atomic_fetch_add(&counter, 1, __ATOMIC_RELAXED);
    }
    return NULL;
}

static int check_threads(void) {
    pthread_t threads[THREADS];
    for (int i = 0; i < THREADS; ++i)
        if (pthread_create(&threads[i], NULL, worker, NULL)) return 1;
    for (int i = 0; i < THREADS; ++i) {
        void *result = NULL;
        if (pthread_join(threads[i], &result) || result) return 1;
    }
    return counter != (uint64_t)THREADS * LOOPS;
}

static int check_pipe_poll_fork(void) {
    int fds[2];
    if (pipe(fds)) return 1;
    pid_t pid = fork();
    if (pid < 0) return 1;
    if (pid == 0) {
        close(fds[0]);
        const char text[] = "starry26-pipe";
        _exit(write(fds[1], text, sizeof(text)) == (ssize_t)sizeof(text) ? 0 : 2);
    }
    close(fds[1]);
    struct pollfd pfd = { .fd = fds[0], .events = POLLIN };
    char buf[32] = {0};
    int status = 0;
    int ok = poll(&pfd, 1, 3000) == 1 &&
             read(fds[0], buf, sizeof(buf)) > 0 &&
             strcmp(buf, "starry26-pipe") == 0 &&
             waitpid(pid, &status, 0) == pid && WIFEXITED(status) &&
             WEXITSTATUS(status) == 0;
    close(fds[0]);
    return !ok;
}

static int check_mmap_and_rename(const char *dir) {
    char old_path[256], new_path[256];
    snprintf(old_path, sizeof(old_path), "%s/mmap.old", dir);
    snprintf(new_path, sizeof(new_path), "%s/mmap.new", dir);
    int fd = open(old_path, O_CREAT | O_TRUNC | O_RDWR, 0644);
    if (fd < 0 || ftruncate(fd, 8192)) return 1;
    unsigned char *map = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (map == MAP_FAILED) return 1;
    map[0] = 0x26;
    map[4096] = 0x5a;
    int failed = msync(map, 8192, MS_SYNC) || munmap(map, 8192) || fsync(fd) ||
                 close(fd) || rename(old_path, new_path);
    fd = open(new_path, O_RDONLY);
    unsigned char check[4097];
    if (fd < 0 || pread(fd, check, sizeof(check), 0) != (ssize_t)sizeof(check)) failed = 1;
    if (!failed && (check[0] != 0x26 || check[4096] != 0x5a)) failed = 1;
    if (fd >= 0) close(fd);
    return failed;
}

int main(int argc, char **argv) {
    const char *dir = argc > 1 ? argv[1] : "/tmp";
    if (check_threads()) return 10;
    if (check_pipe_poll_fork()) return 11;
    if (check_mmap_and_rename(dir)) return 12;
    puts("STARRY26_C_ABI_OK");
    return 0;
}
