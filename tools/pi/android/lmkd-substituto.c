// Substituto mínimo do `lmkd` (Low Memory Killer Daemon) do Android, para o kernel do Pi sem PSI.
//
// Por que existe: o `lmkd` de verdade exige /proc/pressure/memory (PSI) e o controlador de memória dos
// cgroups. O kernel de fábrica do Raspberry Pi OS desliga os dois (cgroup_disable=memory na linha de
// comando do boot), e o `lmkd` sai logo depois de subir. Sem um processo atendendo o soquete `lmkd`, o
// ActivityManager do Android trava ao falar com ele. Este programa só atende o soquete e responde o que o
// ActivityManager espera; não mata nenhum processo. A memória do contêiner fica por conta do kernel do Pi
// (8 GB, sem swap em uso).
//
// Instalado por tools/pi/instalar-android.sh em /var/lib/waydroid/overlay/system/bin/lmkd, SÓ quando
// /proc/pressure/memory não existe. Compilado no Pi: gcc -O2 -static (sem depender de nenhuma biblioteca
// dentro do Android).
//
// Protocolo (AOSP, system/memory/lmkd/include/lmkd.h): o init do Android cria o soquete `lmkd`
// (seqpacket, declarado em lmkd.rc) e passa o descritor já aberto na variável de ambiente
// ANDROID_SOCKET_lmkd. Cada pacote recebido é uma lista de inteiros de 32 bits em ordem de rede (big
// endian); o primeiro é o comando. Comandos que o Android 13 usa:
//   0 LMK_TARGET, 1 LMK_PROCPRIO, 2 LMK_PROCREMOVE, 3 LMK_PROCPURGE   sem resposta
//   4 LMK_GETKILLCNT       resposta: { 4, quantidade de mortes }  (aqui sempre 0)
//   5 LMK_SUBSCRIBE        sem resposta (os eventos assíncronos nunca acontecem)
//   7 LMK_UPDATE_PROPS     resposta: { 7, resultado }  (0 = ok)
//   10 LMK_START_MONITORING, 11 LMK_BOOT_COMPLETED, 12 LMK_PROCS_PRIO   sem resposta
// Qualquer outro comando é lido e ignorado. Um cliente que fecha a conexão é esquecido.
//
// Sem laço ocupado: o processo dorme em poll(2) sem prazo e só acorda com atividade num descritor.

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

// O ActivityManager mantém uma conexão; o `lmkd --reinit` do init abre outra por instantes.
#define MAX_CLIENTES 16
// Maior pacote do protocolo: LMK_PROCS_PRIO leva até 1 + 1 + 3 * 1024 inteiros; o resto lê-se e descarta.
// Um recv de seqpacket com buffer menor que o pacote o trunca, o que aqui não importa (só o comando
// no início conta).
#define TAM_BUF 4096

// Responde { comando, valor } em ordem de rede. Falha de envio (cliente que sumiu) é ignorada: o próximo
// poll vê o fechamento.
static void responder(int fd, uint32_t comando, uint32_t valor) {
    uint32_t resposta[2] = {htonl(comando), htonl(valor)};
    ssize_t n;
    do {
        n = send(fd, resposta, sizeof resposta, MSG_NOSIGNAL);
    } while (n < 0 && errno == EINTR);
}

// Lê um pacote do cliente. Devolve 0 se a conexão continua e -1 se deve ser fechada.
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
        return -1;  // o cliente fechou
    }
    if ((size_t)n < sizeof(uint32_t)) {
        return 0;  // pacote sem comando: ignora
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
    // O init chama `lmkd --reinit` quando uma propriedade muda (lmkd.rc). Não há nada a reler aqui.
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

    // pfds[0] é o soquete de escuta; os demais, os clientes.
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
        // De trás para a frente, para remover clientes sem pular nenhum.
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
                    close(c);  // cheio: o Android reconecta se precisar
                }
            } else if (errno != EINTR && errno != EAGAIN && errno != ECONNABORTED) {
                perror("lmkd-substituto: accept");
            }
        }
    }
}
