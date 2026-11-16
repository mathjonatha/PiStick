
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <unistd.h>

enum {
    LMK_GETKILLCNT = 4,
    LMK_UPDATE_PROPS = 7,
};

#define MAX_CLIENTES 16
#define TAM_BUF 4096

static void responder(int fd, uint32_t comando, uint32_t valor) {
    uint32_t resposta[2] = {htonl(comando), htonl(valor)};
    ssize_t n;
    do {
        n = send(fd, resposta, sizeof resposta, MSG_NOSIGNAL);
    } while (n < 0 && errno == EINTR);
}

static int atender(int fd) {
    uint32_t buf[TAM_BUF / sizeof(uint32_t)];
    ssize_t n;
    do {
        n = recv(fd, buf, sizeof buf, 0);
    } while (n < 0 && errno == EINTR);
    if (n < 0) {
        return (errno == EAGAIN || errno == EWOULDBLOCK) ? 0 : -1;
    }
    if (n == 0) {
        return -1;
    }
    if ((size_t)n < sizeof(uint32_t)) {
        return 0;
    }
    switch (ntohl(buf[0])) {
        case LMK_GETKILLCNT:
            responder(fd, LMK_GETKILLCNT, 0);
            break;
        case LMK_UPDATE_PROPS:
            responder(fd, LMK_UPDATE_PROPS, 0);
            break;
        default:
            break;
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "--reinit") == 0) {
        return 0;
    }

    const char *env = getenv("ANDROID_SOCKET_lmkd");
    if (env == NULL || *env == '\0') {
        fprintf(stderr, "lmkd-substituto: ANDROID_SOCKET_lmkd não definido (só o init do Android o inicia)\n");
        return 1;
    }
    char *fim = NULL;
    long valor = strtol(env, &fim, 10);
    if (*fim != '\0' || valor < 0 || valor > 1 << 20) {
        fprintf(stderr, "lmkd-substituto: ANDROID_SOCKET_lmkd inválido: %s\n", env);
        return 1;
    }
    int escuta = (int)valor;
    if (listen(escuta, 4) < 0) {
        perror("lmkd-substituto: listen");
        return 1;
    }

    struct pollfd pfds[1 + MAX_CLIENTES];
    int n = 1;
    pfds[0].fd = escuta;
    pfds[0].events = POLLIN;

    for (;;) {
        if (poll(pfds, (nfds_t)n, -1) < 0) {
            if (errno == EINTR) {
                continue;
            }
            perror("lmkd-substituto: poll");
            return 1;
        }
        for (int i = n - 1; i >= 1; i--) {
            short ev = pfds[i].revents;
            if (ev == 0) {
                continue;
            }
            if ((ev & (POLLIN | POLLHUP | POLLERR)) == 0 || atender(pfds[i].fd) < 0) {
                close(pfds[i].fd);
                pfds[i] = pfds[--n];
            }
        }
        if (pfds[0].revents & POLLIN) {
            int c = accept4(escuta, NULL, NULL, SOCK_CLOEXEC);
            if (c >= 0) {
                if (n < 1 + MAX_CLIENTES) {
                    pfds[n].fd = c;
                    pfds[n].events = POLLIN;
                    n++;
                } else {
                    close(c);
                }
            } else if (errno != EINTR && errno != EAGAIN && errno != ECONNABORTED) {
                perror("lmkd-substituto: accept");
            }
        }
    }
}
