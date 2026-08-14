#define _POSIX_C_SOURCE 200809L

#include <arpa/inet.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static int fail(const char *what)
{
    fprintf(stderr, "TCP EOF regression failed: %s (errno=%d: %s)\n",
            what, errno, strerror(errno));
    return 1;
}

int main(void)
{
    static const char expected[] = "ONSITE_TCP_EOF\n";
    struct sockaddr_in server = {0};
    char received[sizeof(expected)] = {0};
    size_t used = 0;
    int socket_fd = socket(AF_INET, SOCK_STREAM, 0);

    if (socket_fd < 0)
        return fail("socket");
    server.sin_family = AF_INET;
    server.sin_port = htons(27899);
    if (inet_pton(AF_INET, "10.0.2.2", &server.sin_addr) != 1)
        return fail("inet_pton");
    if (connect(socket_fd, (struct sockaddr *)&server, sizeof(server)) != 0)
        return fail("connect to closing host server");

    while (used < sizeof(expected) - 1) {
        ssize_t count = recv(socket_fd, received + used,
                             sizeof(expected) - 1 - used, 0);
        if (count < 0)
            return fail("receive response body");
        if (count == 0)
            break;
        used += (size_t)count;
    }
    if (used != sizeof(expected) - 1 || memcmp(received, expected, used) != 0)
        return fail("response body mismatch");

    if (recv(socket_fd, received, sizeof(received), 0) != 0)
        return fail("blocking recv did not return EOF");
    if (recv(socket_fd, received, sizeof(received), MSG_DONTWAIT) != 0)
        return fail("nonblocking recv did not preserve EOF");

    close(socket_fd);
    puts("NETWORK_TCP_EOF_OK");
    return 0;
}
