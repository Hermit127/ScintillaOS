#define _POSIX_C_SOURCE 200809L

#include <arpa/inet.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static int fail(const char *what)
{
    fprintf(stderr, "sockname regression failed: %s (errno=%d: %s)\n",
            what, errno, strerror(errno));
    return 1;
}

int main(void)
{
    struct sockaddr_in local = {0};
    struct sockaddr_in second_local = {0};
    struct sockaddr_in proxy = {0};
    socklen_t length = sizeof(local);
    int socket_fd = socket(AF_INET, SOCK_STREAM, 0);
    int second_fd;

    if (socket_fd < 0)
        return fail("socket");
    if (getsockname(socket_fd, (struct sockaddr *)&local, &length) != 0)
        return fail("getsockname on unbound socket");
    if (length != sizeof(local) || local.sin_family != AF_INET ||
        local.sin_addr.s_addr != htonl(INADDR_ANY) || local.sin_port != 0)
        return fail("unexpected unbound socket address");

    proxy.sin_family = AF_INET;
    proxy.sin_port = htons(27899);
    if (inet_pton(AF_INET, "10.0.2.2", &proxy.sin_addr) != 1)
        return fail("inet_pton");
    if (connect(socket_fd, (struct sockaddr *)&proxy, sizeof(proxy)) != 0)
        return fail("connect to host regression server");

    memset(&local, 0, sizeof(local));
    length = sizeof(local);
    if (getsockname(socket_fd, (struct sockaddr *)&local, &length) != 0)
        return fail("getsockname after connect");
    if (length != sizeof(local) || local.sin_family != AF_INET ||
        local.sin_addr.s_addr == htonl(INADDR_ANY) || local.sin_port == 0)
        return fail("connected socket has no selected local endpoint");

    close(socket_fd);
    second_fd = socket(AF_INET, SOCK_STREAM, 0);
    if (second_fd < 0)
        return fail("second socket");
    if (connect(second_fd, (struct sockaddr *)&proxy, sizeof(proxy)) != 0)
        return fail("second connect to host regression server");
    length = sizeof(second_local);
    if (getsockname(second_fd, (struct sockaddr *)&second_local, &length) != 0)
        return fail("getsockname after second connect");
    if (second_local.sin_port == 0 || second_local.sin_port == local.sin_port)
        return fail("outbound TCP reused an active ephemeral port");

    printf("NETWORK_SOCKNAME_OK address=%s ports=%u,%u\n",
           inet_ntoa(local.sin_addr), (unsigned)ntohs(local.sin_port),
           (unsigned)ntohs(second_local.sin_port));
    close(second_fd);
    return 0;
}
