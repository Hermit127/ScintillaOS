#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <termios.h>
#include <unistd.h>
#include <fcntl.h>

static int fail(const char *what)
{
    fprintf(stderr, "TTY ICRNL regression failed: %s (errno=%d: %s)\n",
            what, errno, strerror(errno));
    return 1;
}

static int read_one(int fd, unsigned char *byte)
{
    ssize_t count;
    do {
        count = read(fd, byte, 1);
    } while (count < 0 && errno == EINTR);
    return count == 1 ? 0 : -1;
}

int main(void)
{
    struct termios original;
    struct termios setting;
    unsigned char byte = 0;
    char line[32] = {0};
    ssize_t count;
    int tty = open("/dev/tty", O_RDWR);

    if (tty < 0)
        return fail("open /dev/tty");
    if (tcgetattr(tty, &original) != 0)
        return fail("tcgetattr");

    setting = original;
    setting.c_lflag &= ~(tcflag_t)(ICANON | ECHO);
    setting.c_iflag &= ~(tcflag_t)(IGNCR | INLCR);
    setting.c_iflag |= ICRNL;
    setting.c_cc[VMIN] = 1;
    setting.c_cc[VTIME] = 0;
    if (tcsetattr(tty, TCSANOW, &setting) != 0)
        return fail("enable ICRNL");

    puts("TTY_ICRNL_SEND_CR");
    fflush(stdout);
    if (read_one(tty, &byte) != 0 || byte != '\n')
        return fail("ICRNL did not translate CR to LF");

    setting.c_iflag &= ~(tcflag_t)ICRNL;
    if (tcsetattr(tty, TCSANOW, &setting) != 0)
        return fail("disable ICRNL");
    puts("TTY_RAW_SEND_CR");
    fflush(stdout);
    if (read_one(tty, &byte) != 0 || byte != '\r')
        return fail("disabled ICRNL did not preserve CR");
    puts("TTY_RAW_SEND_DEL");
    fflush(stdout);
    if (read_one(tty, &byte) != 0 || byte != 0x7f)
        return fail("noncanonical mode did not preserve DEL");

    setting = original;
    setting.c_iflag &= ~(tcflag_t)(IGNCR | INLCR);
    setting.c_iflag |= ICRNL;
    setting.c_lflag |= ICANON;
    setting.c_lflag |= ECHO | ECHOE;
    setting.c_cc[VERASE] = 0x7f;
    if (tcsetattr(tty, TCSANOW, &setting) != 0)
        return fail("enable canonical erase");
    puts("TTY_ERASE_SEND_DEL");
    fflush(stdout);
    do {
        count = read(tty, line, sizeof(line));
    } while (count < 0 && errno == EINTR);
    if (count != 7 || memcmp(line, "lyzhcm\n", 7) != 0)
        return fail("VERASE did not remove DEL and preceding byte");

    memset(line, 0, sizeof(line));
    setting.c_cc[VERASE] = 0x08;
    if (tcsetattr(tty, TCSANOW, &setting) != 0)
        return fail("configure BS as VERASE");
    puts("TTY_ERASE_SEND_BS");
    fflush(stdout);
    do {
        count = read(tty, line, sizeof(line));
    } while (count < 0 && errno == EINTR);
    if (count != 4 || memcmp(line, "abc\n", 4) != 0)
        return fail("VERASE did not remove BS and preceding byte");

    if (tcsetattr(tty, TCSANOW, &original) != 0)
        return fail("restore termios");
    close(tty);
    puts("TTY_ERASE_OK");
    puts("TTY_ICRNL_OK");
    return 0;
}
