#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>

#define CHECK(condition, message)                                                \
    do {                                                                         \
        if (!(condition)) {                                                       \
            fprintf(stderr, "ABI2026_FAIL line=%d: %s errno=%d (%s)\n",          \
                    __LINE__, message, errno, strerror(errno));                   \
            return 1;                                                            \
        }                                                                        \
    } while (0)

static pthread_mutex_t robust_mutex;

static long read_mem_available_kb(void) {
    FILE *stream = fopen("/proc/meminfo", "r");
    if (stream == NULL) {
        return -1;
    }
    char line[128];
    long value = -1;
    while (fgets(line, sizeof(line), stream) != NULL) {
        if (sscanf(line, "MemAvailable: %ld kB", &value) == 1) {
            break;
        }
    }
    fclose(stream);
    return value;
}

static void *leave_robust_mutex_locked(void *unused) {
    (void)unused;
    if (pthread_mutex_lock(&robust_mutex) != 0) {
        return (void *)(uintptr_t)1;
    }
    return NULL;
}

static int test_procfs(void) {
    const char *paths[] = {
        "/proc/uptime",
        "/proc/cpuinfo",
        "/proc/meminfo",
        "/proc/mounts",
    };
    char buffer[256];
    for (size_t index = 0; index < sizeof(paths) / sizeof(paths[0]); ++index) {
        int fd = open(paths[index], O_RDONLY | O_CLOEXEC);
        CHECK(fd >= 0, "open procfs file");
        ssize_t count = read(fd, buffer, sizeof(buffer));
        CHECK(count > 0, "read procfs file");
        CHECK(close(fd) == 0, "close procfs file");
    }
    ssize_t count = readlink("/proc/self/exe", buffer, sizeof(buffer) - 1);
    CHECK(count > 0, "readlink /proc/self/exe");
    buffer[count] = '\0';
    CHECK(strstr(buffer, "abi-regression") != NULL, "unexpected /proc/self/exe");
    puts("ABI2026_PROCFS_OK");
    return 0;
}

static int test_eventfd_epoll(void) {
    int event = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
    CHECK(event >= 0, "eventfd");
    uint64_t value = 0;
    errno = 0;
    CHECK(read(event, &value, sizeof(value)) == -1 && errno == EAGAIN,
          "empty nonblocking eventfd");

    int epoll = epoll_create1(EPOLL_CLOEXEC);
    CHECK(epoll >= 0, "epoll_create1");
    struct epoll_event interest = {
        .events = EPOLLIN,
        .data.u64 = UINT64_C(0x2026),
    };
    CHECK(epoll_ctl(epoll, EPOLL_CTL_ADD, event, &interest) == 0, "epoll add");
    value = 7;
    CHECK(write(event, &value, sizeof(value)) == (ssize_t)sizeof(value), "eventfd write");
    struct epoll_event ready = {0};
    CHECK(epoll_wait(epoll, &ready, 1, 1000) == 1, "epoll eventfd wait");
    CHECK(ready.data.u64 == UINT64_C(0x2026) && (ready.events & EPOLLIN),
          "epoll eventfd payload");
    value = 0;
    CHECK(read(event, &value, sizeof(value)) == (ssize_t)sizeof(value) && value == 7,
          "eventfd read");
    CHECK(close(epoll) == 0 && close(event) == 0, "close event descriptors");
    puts("ABI2026_EVENTFD_EPOLL_OK");
    return 0;
}

static int test_timerfd(void) {
    int timer = timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC | TFD_NONBLOCK);
    CHECK(timer >= 0, "timerfd_create");
    uint64_t expirations = 0;
    errno = 0;
    CHECK(read(timer, &expirations, sizeof(expirations)) == -1 && errno == EAGAIN,
          "empty nonblocking timerfd");

    struct itimerspec requested = {
        .it_interval = {.tv_sec = 0, .tv_nsec = 20 * 1000 * 1000},
        .it_value = {.tv_sec = 0, .tv_nsec = 20 * 1000 * 1000},
    };
    struct itimerspec old = {0};
    CHECK(timerfd_settime(timer, 0, &requested, &old) == 0, "timerfd_settime");
    CHECK(old.it_value.tv_sec == 0 && old.it_value.tv_nsec == 0,
          "initial timerfd old value");
    struct itimerspec current = {0};
    CHECK(timerfd_gettime(timer, &current) == 0, "timerfd_gettime");
    CHECK(current.it_interval.tv_nsec == requested.it_interval.tv_nsec,
          "timerfd interval");

    int epoll = epoll_create1(EPOLL_CLOEXEC);
    CHECK(epoll >= 0, "timer epoll_create1");
    struct epoll_event interest = {
        .events = EPOLLIN,
        .data.u64 = UINT64_C(0x85),
    };
    CHECK(epoll_ctl(epoll, EPOLL_CTL_ADD, timer, &interest) == 0, "epoll timer add");
    struct epoll_event ready = {0};
    CHECK(epoll_wait(epoll, &ready, 1, 1000) == 1, "epoll timer wait");
    CHECK(ready.data.u64 == UINT64_C(0x85) && (ready.events & EPOLLIN),
          "epoll timer payload");
    CHECK(read(timer, &expirations, sizeof(expirations)) == (ssize_t)sizeof(expirations),
          "timerfd read");
    CHECK(expirations >= 1, "timerfd expiration count");

    struct itimerspec disarmed = {0};
    CHECK(timerfd_settime(timer, 0, &disarmed, NULL) == 0, "timerfd disarm");
    CHECK(timerfd_gettime(timer, &current) == 0 && current.it_value.tv_sec == 0 &&
              current.it_value.tv_nsec == 0,
          "timerfd disarmed state");
    CHECK(close(epoll) == 0 && close(timer) == 0, "close timer descriptors");
    puts("ABI2026_TIMERFD_OK");
    return 0;
}

static int test_file_mappings(void) {
    char path[] = "/tmp/abi2026-map-XXXXXX";
    int fd = mkstemp(path);
    CHECK(fd >= 0, "mkstemp");
    CHECK(unlink(path) == 0, "unlink mapped temporary file");
    CHECK(ftruncate(fd, 8192) == 0, "ftruncate mapped file");

    char *shared = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    CHECK(shared != MAP_FAILED, "MAP_SHARED");
    memcpy(shared + 4090, "SHARED-2026", 11);
    CHECK(msync(shared, 8192, MS_SYNC) == 0, "msync shared mapping");
    CHECK(munmap(shared, 8192) == 0, "munmap shared mapping");
    char verify[12] = {0};
    CHECK(pread(fd, verify, 11, 4090) == 11 && memcmp(verify, "SHARED-2026", 11) == 0,
          "shared mapping writeback");

    char *private_map = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    CHECK(private_map != MAP_FAILED, "MAP_PRIVATE");
    memcpy(private_map + 4090, "PRIVATE2026", 11);
    CHECK(munmap(private_map, 8192) == 0, "munmap private mapping");
    memset(verify, 0, sizeof(verify));
    CHECK(pread(fd, verify, 11, 4090) == 11 && memcmp(verify, "SHARED-2026", 11) == 0,
          "private mapping isolation");

    void *anonymous = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(anonymous != MAP_FAILED, "anonymous mmap");
    memset(anonymous, 0x5a, 4096);
    void *grown = mremap(anonymous, 4096, 8192, MREMAP_MAYMOVE);
    CHECK(grown != MAP_FAILED, "mremap grow");
    CHECK(((unsigned char *)grown)[0] == 0x5a && ((unsigned char *)grown)[4095] == 0x5a,
          "mremap content");
    CHECK(munmap(grown, 8192) == 0, "munmap grown mapping");
    CHECK(close(fd) == 0, "close mapped file");
    puts("ABI2026_FILE_MAPPING_OK");
    return 0;
}

static int test_robust_pthread(void) {
    pthread_mutexattr_t attr;
    CHECK(pthread_mutexattr_init(&attr) == 0, "pthread_mutexattr_init");
    CHECK(pthread_mutexattr_setrobust(&attr, PTHREAD_MUTEX_ROBUST) == 0,
          "pthread_mutexattr_setrobust");
    CHECK(pthread_mutex_init(&robust_mutex, &attr) == 0, "pthread_mutex_init");
    CHECK(pthread_mutexattr_destroy(&attr) == 0, "pthread_mutexattr_destroy");
    pthread_t thread;
    CHECK(pthread_create(&thread, NULL, leave_robust_mutex_locked, NULL) == 0,
          "pthread_create");
    void *result = NULL;
    CHECK(pthread_join(thread, &result) == 0 && result == NULL, "pthread_join");
    int lock_result = pthread_mutex_lock(&robust_mutex);
    CHECK(lock_result == EOWNERDEAD, "robust mutex owner death");
    CHECK(pthread_mutex_consistent(&robust_mutex) == 0, "pthread_mutex_consistent");
    CHECK(pthread_mutex_unlock(&robust_mutex) == 0, "pthread_mutex_unlock");
    CHECK(pthread_mutex_destroy(&robust_mutex) == 0, "pthread_mutex_destroy");
    puts("ABI2026_GLIBC_THREAD_OK");
    return 0;
}

static int test_resource_reclamation(void) {
    long before = read_mem_available_kb();
    CHECK(before > 0, "read initial MemAvailable");
    for (int iteration = 0; iteration < 64; ++iteration) {
        int event = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
        int timer = timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC | TFD_NONBLOCK);
        int epoll = epoll_create1(EPOLL_CLOEXEC);
        CHECK(event >= 0 && timer >= 0 && epoll >= 0, "resource descriptor loop");
        struct epoll_event interest = {
            .events = EPOLLIN,
            .data.u64 = (uint64_t)iteration,
        };
        CHECK(epoll_ctl(epoll, EPOLL_CTL_ADD, event, &interest) == 0,
              "resource epoll add");
        void *mapping = mmap(NULL, 256 * 1024, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        CHECK(mapping != MAP_FAILED, "resource anonymous mmap");
        memset(mapping, iteration, 256 * 1024);
        CHECK(munmap(mapping, 256 * 1024) == 0, "resource munmap");
        CHECK(close(epoll) == 0 && close(timer) == 0 && close(event) == 0,
              "resource descriptor close");
    }
    usleep(100 * 1000);
    long after = read_mem_available_kb();
    CHECK(after > 0, "read final MemAvailable");
    long lost = before > after ? before - after : 0;
    CHECK(lost < 16 * 1024, "resource loop retained at least 16 MiB");
    printf("ABI2026_RESOURCE_RECLAIM_OK before_kb=%ld after_kb=%ld lost_kb=%ld\n",
           before, after, lost);
    return 0;
}

int main(void) {
    CHECK(test_procfs() == 0, "procfs regression");
    CHECK(test_eventfd_epoll() == 0, "eventfd/epoll regression");
    CHECK(test_timerfd() == 0, "timerfd regression");
    CHECK(test_file_mappings() == 0, "file mapping regression");
    CHECK(test_robust_pthread() == 0, "pthread robust regression");
    CHECK(test_resource_reclamation() == 0, "resource reclamation regression");
    puts("ABI2026_REGRESSION_OK");
    return 0;
}
