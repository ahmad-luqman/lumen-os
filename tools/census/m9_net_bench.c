#define _GNU_SOURCE
/*
 * M9 syscall census microbenchmark (#451): exercises AF_UNIX socketpair +
 * SCM_RIGHTS FD passing, epoll (level and edge-triggered; musl's epoll_wait()
 * issues the epoll_pwait syscall), eventfd2, timerfd, and loopback TCP socket
 * options/accept4.
 */
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>

static void die(const char *msg) {
    perror(msg);
    exit(1);
}

static void die_msg(const char *msg) {
    fprintf(stderr, "m9_net_bench: %s\n", msg);
    exit(1);
}

static void check_scm_rights_and_epoll(void) {
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0, sv) < 0) {
        die("socketpair");
    }

    int pfd[2];
    if (pipe2(pfd, O_CLOEXEC | O_NONBLOCK) < 0) {
        die("pipe2");
    }
    const char payload[] = "scm-ok";
    if (write(pfd[1], payload, sizeof(payload) - 1) != (ssize_t)(sizeof(payload) - 1)) {
        die("pipe write");
    }

    /* Pass pfd[0] across the Unix socketpair via SCM_RIGHTS. */
    char tag = 'F';
    struct iovec siov = {.iov_base = &tag, .iov_len = 1};
    union {
        char buf[CMSG_SPACE(sizeof(int))];
        struct cmsghdr align;
    } cmsg_send;
    memset(&cmsg_send, 0, sizeof(cmsg_send));

    struct msghdr smsg;
    memset(&smsg, 0, sizeof(smsg));
    smsg.msg_iov = &siov;
    smsg.msg_iovlen = 1;
    smsg.msg_control = cmsg_send.buf;
    smsg.msg_controllen = sizeof(cmsg_send.buf);

    struct cmsghdr *cmsg = CMSG_FIRSTHDR(&smsg);
    cmsg->cmsg_level = SOL_SOCKET;
    cmsg->cmsg_type = SCM_RIGHTS;
    cmsg->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(cmsg), &pfd[0], sizeof(int));

    if (sendmsg(sv[0], &smsg, MSG_NOSIGNAL) != 1) {
        die("sendmsg SCM_RIGHTS");
    }
    close(pfd[0]);
    close(pfd[1]);

    char rtag = 0;
    struct iovec riov = {.iov_base = &rtag, .iov_len = 1};
    union {
        char buf[CMSG_SPACE(sizeof(int))];
        struct cmsghdr align;
    } cmsg_recv;
    memset(&cmsg_recv, 0, sizeof(cmsg_recv));

    struct msghdr rmsg;
    memset(&rmsg, 0, sizeof(rmsg));
    rmsg.msg_iov = &riov;
    rmsg.msg_iovlen = 1;
    rmsg.msg_control = cmsg_recv.buf;
    rmsg.msg_controllen = sizeof(cmsg_recv.buf);

    if (recvmsg(sv[1], &rmsg, MSG_CMSG_CLOEXEC) != 1 || rtag != 'F') {
        die("recvmsg SCM_RIGHTS");
    }
    int recv_fd = -1;
    for (cmsg = CMSG_FIRSTHDR(&rmsg); cmsg != NULL; cmsg = CMSG_NXTHDR(&rmsg, cmsg)) {
        if (cmsg->cmsg_level == SOL_SOCKET && cmsg->cmsg_type == SCM_RIGHTS) {
            memcpy(&recv_fd, CMSG_DATA(cmsg), sizeof(int));
            break;
        }
    }
    if (recv_fd < 0) {
        die_msg("missing SCM_RIGHTS fd");
    }
    char buf[16];
    ssize_t n = read(recv_fd, buf, sizeof(buf));
    if (n != (ssize_t)(sizeof(payload) - 1) || memcmp(buf, payload, (size_t)n) != 0) {
        die_msg("unexpected payload from received fd");
    }
    close(recv_fd);

    /* Exercise epoll (ET + LT; musl's epoll_wait() makes the epoll_pwait syscall), eventfd2 and timerfd. */
    int ep = epoll_create1(EPOLL_CLOEXEC);
    if (ep < 0) {
        die("epoll_create1");
    }
    int efd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
    if (efd < 0) {
        die("eventfd");
    }
    int tfd = timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC | TFD_NONBLOCK);
    if (tfd < 0) {
        die("timerfd_create");
    }

    struct epoll_event ev;
    ev.events = EPOLLIN | EPOLLET;
    ev.data.fd = sv[1];
    if (epoll_ctl(ep, EPOLL_CTL_ADD, sv[1], &ev) < 0) {
        die("epoll_ctl ADD sv[1]");
    }
    ev.events = EPOLLIN;
    ev.data.fd = efd;
    if (epoll_ctl(ep, EPOLL_CTL_ADD, efd, &ev) < 0) {
        die("epoll_ctl ADD efd");
    }
    ev.events = EPOLLIN;
    ev.data.fd = tfd;
    if (epoll_ctl(ep, EPOLL_CTL_ADD, tfd, &ev) < 0) {
        die("epoll_ctl ADD tfd");
    }

    struct itimerspec its;
    memset(&its, 0, sizeof(its));
    its.it_value.tv_nsec = 20 * 1000 * 1000;
    if (timerfd_settime(tfd, 0, &its, NULL) < 0) {
        die("timerfd_settime");
    }
    struct itimerspec cur;
    if (timerfd_gettime(tfd, &cur) < 0) {
        die("timerfd_gettime");
    }

    const char edge_msg[] = "edge";
    if (write(sv[0], edge_msg, sizeof(edge_msg) - 1) != (ssize_t)(sizeof(edge_msg) - 1)) {
        die("sv[0] write");
    }
    uint64_t one = 1;
    if (write(efd, &one, sizeof(one)) != (ssize_t)sizeof(one)) {
        die("eventfd write");
    }

    int got_sv = 0, got_efd = 0, got_tfd = 0;
    for (int round = 0; !(got_sv && got_efd && got_tfd) && round < 20; round++) {
        struct epoll_event ready[4];
        int nfds = epoll_wait(ep, ready, 4, 1000);
        if (nfds < 0) {
            if (errno == EINTR) {
                continue;
            }
            die("epoll_wait");
        }
        if (nfds == 0) {
            die_msg("epoll timed out");
        }
        for (int i = 0; i < nfds; i++) {
            int fd = ready[i].data.fd;
            if (fd == sv[1]) {
                n = read(sv[1], buf, sizeof(buf));
                if (n != (ssize_t)(sizeof(edge_msg) - 1) ||
                    memcmp(buf, edge_msg, (size_t)n) != 0) {
                    die_msg("bad edge payload");
                }
                got_sv = 1;
            } else if (fd == efd) {
                uint64_t cnt = 0;
                if (read(efd, &cnt, sizeof(cnt)) != (ssize_t)sizeof(cnt) || cnt != 1) {
                    die_msg("bad eventfd count");
                }
                got_efd = 1;
            } else if (fd == tfd) {
                uint64_t exp = 0;
                if (read(tfd, &exp, sizeof(exp)) != (ssize_t)sizeof(exp) || exp < 1) {
                    die_msg("bad timerfd expirations");
                }
                got_tfd = 1;
            }
        }
    }
    if (!(got_sv && got_efd && got_tfd)) {
        die_msg("did not observe all epoll events");
    }

    ev.events = EPOLLIN | EPOLLOUT;
    ev.data.fd = sv[1];
    if (epoll_ctl(ep, EPOLL_CTL_MOD, sv[1], &ev) < 0) {
        die("epoll_ctl MOD");
    }
    if (epoll_ctl(ep, EPOLL_CTL_DEL, sv[1], NULL) < 0) {
        die("epoll_ctl DEL");
    }

    close(tfd);
    close(efd);
    close(ep);
    close(sv[0]);
    close(sv[1]);
}

static void check_tcp_loopback(void) {
    int srv = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, IPPROTO_TCP);
    if (srv < 0) {
        die("socket tcp");
    }
    int yes = 1;
    if (setsockopt(srv, SOL_SOCKET, SO_REUSEADDR, &yes, sizeof(yes)) < 0) {
        die("setsockopt SO_REUSEADDR");
    }
    if (setsockopt(srv, IPPROTO_TCP, TCP_NODELAY, &yes, sizeof(yes)) < 0) {
        die("setsockopt TCP_NODELAY");
    }

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    addr.sin_port = 0;
    if (bind(srv, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        die("bind");
    }
    if (listen(srv, 4) < 0) {
        die("listen");
    }
    socklen_t alen = sizeof(addr);
    if (getsockname(srv, (struct sockaddr *)&addr, &alen) < 0) {
        die("getsockname");
    }

    int cli = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK, IPPROTO_TCP);
    if (cli < 0) {
        die("socket client");
    }
    if (connect(cli, (struct sockaddr *)&addr, sizeof(addr)) < 0 && errno != EINPROGRESS) {
        die("connect");
    }

    struct sockaddr_in peer;
    socklen_t plen = sizeof(peer);
    int acc = accept4(srv, (struct sockaddr *)&peer, &plen, SOCK_NONBLOCK | SOCK_CLOEXEC);
    if (acc < 0) {
        die("accept4");
    }
    if (getpeername(acc, (struct sockaddr *)&peer, &plen) < 0) {
        die("getpeername");
    }

    int ep = epoll_create1(EPOLL_CLOEXEC);
    if (ep < 0) {
        die("epoll_create1 tcp");
    }
    struct epoll_event ev = {.events = EPOLLOUT, .data.fd = cli};
    if (epoll_ctl(ep, EPOLL_CTL_ADD, cli, &ev) < 0) {
        die("epoll_ctl tcp");
    }
    struct epoll_event ready;
    if (epoll_wait(ep, &ready, 1, 1000) != 1) {
        die("epoll_wait connect");
    }
    int so_err = -1;
    socklen_t elen = sizeof(so_err);
    if (getsockopt(cli, SOL_SOCKET, SO_ERROR, &so_err, &elen) < 0 || so_err != 0) {
        die_msg("connect SO_ERROR non-zero");
    }

    const char msg[] = "tcp-ok";
    if (sendto(cli, msg, sizeof(msg) - 1, MSG_NOSIGNAL, NULL, 0) != (ssize_t)(sizeof(msg) - 1)) {
        die("sendto");
    }
    if (shutdown(cli, SHUT_WR) < 0) {
        die("shutdown");
    }

    /* Delete cli (still level-triggered EPOLLOUT) so the wait below observes acc's EPOLLIN. */
    if (epoll_ctl(ep, EPOLL_CTL_DEL, cli, NULL) < 0) {
        die("epoll_ctl DEL cli");
    }
    ev.events = EPOLLIN;
    ev.data.fd = acc;
    if (epoll_ctl(ep, EPOLL_CTL_ADD, acc, &ev) < 0) {
        die("epoll_ctl acc");
    }
    if (epoll_wait(ep, &ready, 1, 1000) != 1) {
        die("epoll_wait recv");
    }
    if (ready.data.fd != acc) {
        die_msg("unexpected ready fd");
    }
    char buf[16];
    ssize_t n = recvfrom(acc, buf, sizeof(buf), 0, NULL, NULL);
    if (n != (ssize_t)(sizeof(msg) - 1) || memcmp(buf, msg, (size_t)n) != 0) {
        die_msg("unexpected tcp payload");
    }

    close(ep);
    close(acc);
    close(cli);
    close(srv);
}

int main(void) {
    check_scm_rights_and_epoll();
    check_tcp_loopback();
    return 0;
}
