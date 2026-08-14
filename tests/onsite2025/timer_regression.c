#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

static volatile sig_atomic_t timer_hits;

static void timer_handler(int signo, siginfo_t *info, void *context)
{
    (void)context;
    if (signo == SIGUSR1 && info != NULL && info->si_value.sival_int == 2025)
        ++timer_hits;
}

static int fail(const char *what)
{
    fprintf(stderr, "timer regression failed: %s (errno=%d: %s)\n",
            what, errno, strerror(errno));
    return 1;
}

static void add_milliseconds(struct timespec *value, long milliseconds)
{
    value->tv_nsec += milliseconds * 1000000L;
    value->tv_sec += value->tv_nsec / 1000000000L;
    value->tv_nsec %= 1000000000L;
}

int main(void)
{
    struct sigaction action = {0};
    struct sigevent event = {0};
    struct itimerspec setting = {0};
    struct itimerspec current = {0};
    struct itimerspec previous = {0};
    sigset_t blocked;
    sigset_t wait_mask;
    timer_t timer;
    timer_t absolute_timer;
    timer_t realtime_timer;

    action.sa_sigaction = timer_handler;
    action.sa_flags = SA_SIGINFO;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGUSR1, &action, NULL) != 0)
        return fail("sigaction");

    sigemptyset(&blocked);
    sigaddset(&blocked, SIGUSR1);
    if (sigprocmask(SIG_BLOCK, &blocked, NULL) != 0)
        return fail("sigprocmask block");
    sigemptyset(&wait_mask);

    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGUSR1;
    event.sigev_value.sival_int = 2025;
    if (timer_create(CLOCK_MONOTONIC, &event, &timer) != 0)
        return fail("timer_create monotonic");

    setting.it_value.tv_nsec = 30000000L;
    if (timer_settime(timer, 0, &setting, &previous) != 0)
        return fail("one-shot timer_settime");
    if (previous.it_value.tv_sec != 0 || previous.it_value.tv_nsec != 0)
        return fail("unexpected previous timer value");
    if (timer_gettime(timer, &current) != 0)
        return fail("timer_gettime armed");
    if (current.it_value.tv_sec == 0 && current.it_value.tv_nsec == 0)
        return fail("armed timer reported as disarmed");

    while (timer_hits < 1)
        sigsuspend(&wait_mask);

    memset(&setting, 0, sizeof(setting));
    setting.it_value.tv_nsec = 20000000L;
    setting.it_interval.tv_nsec = 20000000L;
    if (timer_settime(timer, 0, &setting, NULL) != 0)
        return fail("periodic timer_settime");
    while (timer_hits < 4)
        sigsuspend(&wait_mask);
    if (timer_getoverrun(timer) < 0)
        return fail("timer_getoverrun");

    memset(&setting, 0, sizeof(setting));
    if (timer_settime(timer, 0, &setting, NULL) != 0)
        return fail("timer disarm");
    if (timer_delete(timer) != 0)
        return fail("timer_delete");

    errno = 0;
    if (timer_gettime(timer, &current) != -1 || errno != EINVAL)
        return fail("deleted timer ID must return EINVAL");
    errno = 0;
    if (timer_create((clockid_t)-123, &event, &timer) != -1 || errno != EINVAL)
        return fail("invalid clock must return EINVAL");

    event.sigev_notify = SIGEV_NONE;
    if (timer_create(CLOCK_MONOTONIC, &event, &absolute_timer) != 0)
        return fail("timer_create absolute");
    memset(&setting, 0, sizeof(setting));
    if (clock_gettime(CLOCK_MONOTONIC, &setting.it_value) != 0)
        return fail("clock_gettime monotonic");
    add_milliseconds(&setting.it_value, 40);
    if (timer_settime(absolute_timer, TIMER_ABSTIME, &setting, NULL) != 0)
        return fail("absolute timer_settime");
    if (timer_gettime(absolute_timer, &current) != 0)
        return fail("absolute timer_gettime");
    if (timer_delete(absolute_timer) != 0)
        return fail("absolute timer_delete");

    if (timer_create(CLOCK_REALTIME, NULL, &realtime_timer) != 0)
        return fail("timer_create realtime");
    if (timer_delete(realtime_timer) != 0)
        return fail("realtime timer_delete");

    if (sigprocmask(SIG_UNBLOCK, &blocked, NULL) != 0)
        return fail("sigprocmask unblock");
    printf("TIMER_REGRESSION_OK hits=%d\n", (int)timer_hits);
    return 0;
}
