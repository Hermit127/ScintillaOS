#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <termios.h>
#include <unistd.h>

static int fail(const char *what)
{
    fprintf(stderr, "tty regression failed: %s (errno=%d: %s)\n",
            what, errno, strerror(errno));
    return 1;
}

static int expect_enotty(int fd, const char *what)
{
    struct termios term;

    errno = 0;
    if (ioctl(fd, TCGETS, &term) != -1 || errno != ENOTTY)
        return fail(what);
    return 0;
}

int main(void)
{
    struct termios original;
    struct termios changed;
    struct winsize original_size;
    struct winsize changed_size;
    int available = -1;
    int tty;
    int regular;
    int pipes[2];
    pid_t foreground;

    tty = open("/dev/tty", O_RDWR);
    if (tty < 0)
        return fail("open /dev/tty");
    if (tcgetattr(tty, &original) != 0)
        return fail("TCGETS");
    changed = original;
    changed.c_lflag ^= ECHO;
    if (tcsetattr(tty, TCSANOW, &changed) != 0)
        return fail("TCSETS");
    if (tcsetattr(tty, TCSADRAIN, &original) != 0)
        return fail("TCSETSW restore");
    if (tcsetattr(tty, TCSAFLUSH, &original) != 0)
        return fail("TCSETSF restore");

    if (ioctl(tty, TIOCGWINSZ, &original_size) != 0)
        return fail("TIOCGWINSZ");
    changed_size = original_size;
    changed_size.ws_row = 31;
    changed_size.ws_col = 97;
    if (ioctl(tty, TIOCSWINSZ, &changed_size) != 0)
        return fail("TIOCSWINSZ");
    memset(&changed_size, 0, sizeof(changed_size));
    if (ioctl(tty, TIOCGWINSZ, &changed_size) != 0 ||
        changed_size.ws_row != 31 || changed_size.ws_col != 97)
        return fail("window size persistence");
    if (ioctl(tty, TIOCSWINSZ, &original_size) != 0)
        return fail("window size restore");

    foreground = tcgetpgrp(tty);
    if (foreground <= 0)
        return fail("TIOCGPGRP");
    if (tcsetpgrp(tty, foreground) != 0)
        return fail("TIOCSPGRP");
    if (ioctl(tty, FIONREAD, &available) != 0 || available < 0)
        return fail("FIONREAD");

    regular = open("/tmp/tty-regular", O_CREAT | O_TRUNC | O_RDWR, 0600);
    if (regular < 0)
        return fail("open regular file");
    if (expect_enotty(regular, "regular file must return ENOTTY") != 0)
        return 1;
    if (pipe(pipes) != 0)
        return fail("pipe");
    if (expect_enotty(pipes[0], "pipe read end must return ENOTTY") != 0)
        return 1;
    if (expect_enotty(pipes[1], "pipe write end must return ENOTTY") != 0)
        return 1;

    close(pipes[0]);
    close(pipes[1]);
    close(regular);
    close(tty);
    puts("TTY_REGRESSION_OK");
    return 0;
}
