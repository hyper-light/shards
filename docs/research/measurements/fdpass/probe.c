/*
 * fdpass_probe: measures AF_UNIX / SCM_RIGHTS semantics of the running kernel.
 * Builds on macOS (clang) and Linux (glibc gcc, musl zig cc). Run as a
 * non-root user so filesystem permission checks are not bypassed.
 */
#define _GNU_SOURCE
#include <sys/types.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <sys/resource.h>
#include <sys/uio.h>
#include <sys/utsname.h>
#include <sys/mman.h>
#include <fcntl.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <stddef.h>
#ifdef __APPLE__
#include <sys/event.h>
#include <sys/ucred.h>
#include <mach/message.h>
#else
#include <sys/epoll.h>
#include <sys/syscall.h>
#include <sys/eventfd.h>
#ifndef SO_PEERPIDFD
#define SO_PEERPIDFD 77
#endif
#ifndef SO_PASSRIGHTS
#define SO_PASSRIGHTS 83
#endif
#endif

#define MAXFD 4096

static const char *en(int e)
{
	static char b[8][48];
	static int k;
	const char *n = NULL;
	switch (e) {
	case 0: return "OK";
	case EINVAL: n = "EINVAL"; break;
	case EBADF: n = "EBADF"; break;
	case EMSGSIZE: n = "EMSGSIZE"; break;
	case EPERM: n = "EPERM"; break;
	case ETOOMANYREFS: n = "ETOOMANYREFS"; break;
	case EADDRINUSE: n = "EADDRINUSE"; break;
	case EEXIST: n = "EEXIST"; break;
	case ECONNREFUSED: n = "ECONNREFUSED"; break;
	case ENOTSOCK: n = "ENOTSOCK"; break;
	case EACCES: n = "EACCES"; break;
	case ENAMETOOLONG: n = "ENAMETOOLONG"; break;
	case ENOENT: n = "ENOENT"; break;
	case EMFILE: n = "EMFILE"; break;
	case ENOBUFS: n = "ENOBUFS"; break;
	case EOPNOTSUPP: n = "EOPNOTSUPP"; break;
	case ENOPROTOOPT: n = "ENOPROTOOPT"; break;
	case ENOTCONN: n = "ENOTCONN"; break;
	case EAGAIN: n = "EAGAIN"; break;
	case ENOSYS: n = "ENOSYS"; break;
	case ENODATA: n = "ENODATA"; break;
	}
	k = (k + 1) % 8;
	if (n)
		snprintf(b[k], sizeof b[k], "%s", n);
	else
		snprintf(b[k], sizeof b[k], "errno %d (%s)", e, strerror(e));
	return b[k];
}

static void snap(unsigned char *m)
{
	for (int i = 0; i < MAXFD; i++)
		m[i] = fcntl(i, F_GETFD) != -1;
}

static int newfds(const unsigned char *a, const unsigned char *b)
{
	int n = 0;
	for (int i = 0; i < MAXFD; i++)
		n += !a[i] && b[i];
	return n;
}

static void closenew(const unsigned char *a, const unsigned char *b)
{
	for (int i = 0; i < MAXFD; i++)
		if (!a[i] && b[i])
			close(i);
}

/* Sends one data byte plus n fds split over ncmsg SCM_RIGHTS headers, with
 * pad extra zero bytes of msg_controllen. Returns 0 or errno. */
static int send_fds_ex(int s, const int *fds, int n, int ncmsg, int pad)
{
	char byte = 'x';
	struct iovec iov = { &byte, 1 };
	int per[2];
	per[0] = ncmsg == 2 ? n / 2 : n;
	per[1] = n - per[0];
	size_t space = pad;
	for (int k = 0; k < ncmsg; k++)
		space += CMSG_SPACE(sizeof(int) * per[k]);
	char *buf = calloc(1, space + 64);
	struct msghdr msg;
	memset(&msg, 0, sizeof msg);
	msg.msg_iov = &iov;
	msg.msg_iovlen = 1;
	msg.msg_control = buf;
	msg.msg_controllen = space;
	char *p = buf;
	int off = 0;
	for (int k = 0; k < ncmsg; k++) {
		struct cmsghdr *c = (struct cmsghdr *)p;
		c->cmsg_level = SOL_SOCKET;
		c->cmsg_type = SCM_RIGHTS;
		c->cmsg_len = CMSG_LEN(sizeof(int) * per[k]);
		memcpy(CMSG_DATA(c), fds + off, sizeof(int) * per[k]);
		off += per[k];
		p += CMSG_SPACE(sizeof(int) * per[k]);
	}
	ssize_t r = sendmsg(s, &msg, 0);
	int e = r < 0 ? errno : 0;
	free(buf);
	return e;
}

static int send_fds(int s, const int *fds, int n)
{
	return send_fds_ex(s, fds, n, 1, 0);
}

struct rres {
	ssize_t r;
	int err;
	int flags;
	int ncmsg;
	int nfds;
	int fds[600];
	unsigned retlen;
	unsigned first_cmsg_len;
};

/* ctl_fds: size the control buffer for this many fds (0 = zero-length
 * buffer). nullctl: pass msg_control = NULL. */
static void recv_fds(int s, int ctl_fds, int nullctl, int flags, struct rres *o)
{
	char byte;
	struct iovec iov = { &byte, 1 };
	size_t space = ctl_fds > 0 ? CMSG_SPACE(sizeof(int) * ctl_fds) : 0;
	char *buf = calloc(1, space + 16);
	struct msghdr msg;
	memset(&msg, 0, sizeof msg);
	msg.msg_iov = &iov;
	msg.msg_iovlen = 1;
	if (!nullctl) {
		msg.msg_control = buf;
		msg.msg_controllen = space;
	}
	memset(o, 0, sizeof *o);
	o->r = recvmsg(s, &msg, flags);
	o->err = o->r < 0 ? errno : 0;
	o->flags = msg.msg_flags;
	o->retlen = msg.msg_controllen;
	if (o->r >= 0 && !nullctl) {
		size_t off = 0, tot = msg.msg_controllen;
		while (off + sizeof(struct cmsghdr) <= tot) {
			struct cmsghdr *c = (struct cmsghdr *)(buf + off);
			size_t clen = c->cmsg_len;
			if (o->ncmsg == 0)
				o->first_cmsg_len = clen;
			if (clen < CMSG_LEN(0))
				break;
			size_t usable = clen < tot - off ? clen : tot - off;
			o->ncmsg++;
			if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS) {
				int k = (int)((usable - CMSG_LEN(0)) / sizeof(int));
				if (o->nfds + k <= 600) {
					memcpy(o->fds + o->nfds, CMSG_DATA(c), k * sizeof(int));
					o->nfds += k;
				}
			}
			off += CMSG_SPACE(clen - CMSG_LEN(0));
		}
	}
	free(buf);
}

static void closeall(struct rres *o)
{
	for (int i = 0; i < o->nfds; i++)
		if (o->fds[i] > 2)
			close(o->fds[i]);
}

static void raise_nofile(void)
{
	struct rlimit rl;
	getrlimit(RLIMIT_NOFILE, &rl);
	rlim_t want = MAXFD;
	if (rl.rlim_max != RLIM_INFINITY && rl.rlim_max < want)
		want = rl.rlim_max;
	rl.rlim_cur = want;
	setrlimit(RLIMIT_NOFILE, &rl);
	getrlimit(RLIMIT_NOFILE, &rl);
	printf("RLIMIT_NOFILE soft=%llu hard=%llu\n", (unsigned long long)rl.rlim_cur,
	    (unsigned long long)rl.rlim_max);
}

static void t_max(void)
{
	printf("\n[T1] max fds per SCM_RIGHTS message (single cmsg, controllen=CMSG_SPACE)\n");
	int ns[] = { 1, 252, 253, 254, 255, 256, 512 };
	for (unsigned i = 0; i < sizeof ns / sizeof ns[0]; i++) {
		int n = ns[i];
		int sp[2], p[2];
		socketpair(AF_UNIX, SOCK_STREAM, 0, sp);
		pipe(p);
		int *fds = malloc(sizeof(int) * n);
		for (int k = 0; k < n; k++)
			fds[k] = p[0];
		int e = send_fds(sp[0], fds, n);
		struct rres o;
		o.nfds = 0;
		if (!e) {
			recv_fds(sp[1], n, 0, MSG_DONTWAIT, &o);
			printf("  n=%-3d sendmsg=%s  recv: r=%zd err=%s nfds=%d ctrunc=%d\n", n, en(e),
			    o.r, en(o.err), o.nfds, !!(o.flags & MSG_CTRUNC));
			closeall(&o);
		} else {
			printf("  n=%-3d sendmsg=%s\n", n, en(e));
		}
		free(fds);
		close(p[0]);
		close(p[1]);
		close(sp[0]);
		close(sp[1]);
	}

	printf("[T1b] two SCM_RIGHTS cmsgs (2+2 fds) in one message\n");
	{
		int sp[2], p[2];
		socketpair(AF_UNIX, SOCK_STREAM, 0, sp);
		pipe(p);
		int fds[4] = { p[0], p[0], p[0], p[0] };
		int e = send_fds_ex(sp[0], fds, 4, 2, 0);
		printf("  sendmsg=%s", en(e));
		if (!e) {
			struct rres o;
			recv_fds(sp[1], 8, 0, MSG_DONTWAIT, &o);
			printf("  recv: ncmsg=%d nfds=%d ctrunc=%d", o.ncmsg, o.nfds, !!(o.flags & MSG_CTRUNC));
			closeall(&o);
		}
		printf("\n");
		close(p[0]); close(p[1]); close(sp[0]); close(sp[1]);
	}
	printf("[T1c] one cmsg (1 fd) but msg_controllen = CMSG_SPACE(4)+8\n");
	{
		int sp[2], p[2];
		socketpair(AF_UNIX, SOCK_STREAM, 0, sp);
		pipe(p);
		int e = send_fds_ex(sp[0], &p[0], 1, 1, 8);
		printf("  sendmsg=%s\n", en(e));
		if (!e) {
			struct rres o;
			recv_fds(sp[1], 4, 0, MSG_DONTWAIT, &o);
			closeall(&o);
		}
		close(p[0]); close(p[1]); close(sp[0]); close(sp[1]);
	}
	printf("[T1d] SOCK_DGRAM socketpair: 1 fd with zero data bytes\n");
	{
		int sp[2], p[2];
		socketpair(AF_UNIX, SOCK_DGRAM, 0, sp);
		pipe(p);
		char buf[CMSG_SPACE(sizeof(int))];
		memset(buf, 0, sizeof buf);
		struct msghdr msg;
		memset(&msg, 0, sizeof msg);
		char z = 0;
		struct iovec ziov = { &z, 0 };
		msg.msg_iov = &ziov;
		msg.msg_iovlen = 1;
		msg.msg_control = buf;
		msg.msg_controllen = sizeof buf;
		struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
		c->cmsg_level = SOL_SOCKET;
		c->cmsg_type = SCM_RIGHTS;
		c->cmsg_len = CMSG_LEN(sizeof(int));
		memcpy(CMSG_DATA(c), &p[0], sizeof(int));
		ssize_t r = sendmsg(sp[0], &msg, 0);
		printf("  dgram sendmsg(0 bytes)=%s\n", en(r < 0 ? errno : 0));
		close(p[0]); close(p[1]); close(sp[0]); close(sp[1]);
	}
	printf("[T1e] SOCK_STREAM socketpair: 1 fd with zero data bytes\n");
	{
		int sp[2], p[2];
		socketpair(AF_UNIX, SOCK_STREAM, 0, sp);
		pipe(p);
		char buf[CMSG_SPACE(sizeof(int))];
		memset(buf, 0, sizeof buf);
		struct msghdr msg;
		memset(&msg, 0, sizeof msg);
		char z = 0;
		struct iovec ziov = { &z, 0 };
		msg.msg_iov = &ziov;
		msg.msg_iovlen = 1;
		msg.msg_control = buf;
		msg.msg_controllen = sizeof buf;
		struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
		c->cmsg_level = SOL_SOCKET;
		c->cmsg_type = SCM_RIGHTS;
		c->cmsg_len = CMSG_LEN(sizeof(int));
		memcpy(CMSG_DATA(c), &p[0], sizeof(int));
		ssize_t r = sendmsg(sp[0], &msg, 0);
		printf("  stream sendmsg(0 bytes)=%s r=%zd", en(r < 0 ? errno : 0), r);
		struct rres o;
		recv_fds(sp[1], 4, 0, MSG_DONTWAIT, &o);
		printf("  recv: r=%zd err=%s nfds=%d\n", o.r, en(o.err), o.nfds);
		closeall(&o);
		close(p[0]); close(p[1]); close(sp[0]); close(sp[1]);
	}
}

static void t_trunc(void)
{
	unsigned char a[MAXFD], b[MAXFD];
	printf("\n[T2] receiver control buffer too small: send 10 fds, buffer for 2\n");
	{
		int sp[2], p[2];
		socketpair(AF_UNIX, SOCK_STREAM, 0, sp);
		pipe(p);
		int fds[10];
		for (int k = 0; k < 10; k++)
			fds[k] = p[0];
		int e = send_fds(sp[0], fds, 10);
		snap(a);
		struct rres o;
		recv_fds(sp[1], 2, 0, MSG_DONTWAIT, &o);
		snap(b);
		printf("  send=%s recv r=%zd err=%s ctrunc=%d fds_reported=%d new_fds_in_table=%d "
		       "returned_controllen=%u first_cmsg_len=%u (CMSG_LEN(40)=%u)\n",
		    en(e), o.r, en(o.err), !!(o.flags & MSG_CTRUNC), o.nfds, newfds(a, b), o.retlen,
		    o.first_cmsg_len, (unsigned)CMSG_LEN(40));
		closenew(a, b);
		close(p[0]); close(p[1]); close(sp[0]); close(sp[1]);
	}
	printf("[T3] receiver passes msg_control=NULL: send 5 fds\n");
	{
		int sp[2], p[2];
		socketpair(AF_UNIX, SOCK_STREAM, 0, sp);
		pipe(p);
		int fds[5] = { p[0], p[0], p[0], p[0], p[0] };
		int e = send_fds(sp[0], fds, 5);
		snap(a);
		struct rres o;
		recv_fds(sp[1], 0, 1, MSG_DONTWAIT, &o);
		snap(b);
		printf("  send=%s recv r=%zd err=%s ctrunc=%d new_fds_in_table=%d\n", en(e), o.r,
		    en(o.err), !!(o.flags & MSG_CTRUNC), newfds(a, b));
		closenew(a, b);
		close(p[0]); close(p[1]); close(sp[0]); close(sp[1]);
	}
	printf("[T3b] receiver passes msg_control!=NULL, msg_controllen=0: send 5 fds\n");
	{
		int sp[2], p[2];
		socketpair(AF_UNIX, SOCK_STREAM, 0, sp);
		pipe(p);
		int fds[5] = { p[0], p[0], p[0], p[0], p[0] };
		int e = send_fds(sp[0], fds, 5);
		snap(a);
		struct rres o;
		recv_fds(sp[1], 0, 0, MSG_DONTWAIT, &o);
		snap(b);
		printf("  send=%s recv r=%zd err=%s ctrunc=%d new_fds_in_table=%d\n", en(e), o.r,
		    en(o.err), !!(o.flags & MSG_CTRUNC), newfds(a, b));
		closenew(a, b);
		close(p[0]); close(p[1]); close(sp[0]); close(sp[1]);
	}
	printf("[T3c] plain read(2) on the socket: send 5 fds\n");
	{
		int sp[2], p[2];
		socketpair(AF_UNIX, SOCK_STREAM, 0, sp);
		pipe(p);
		int fds[5] = { p[0], p[0], p[0], p[0], p[0] };
		int e = send_fds(sp[0], fds, 5);
		snap(a);
		char c;
		ssize_t r = read(sp[1], &c, 1);
		int re = r < 0 ? errno : 0;
		snap(b);
		printf("  send=%s read r=%zd err=%s new_fds_in_table=%d\n", en(e), r, en(re), newfds(a, b));
		closenew(a, b);
		close(p[0]); close(p[1]); close(sp[0]); close(sp[1]);
	}
}

static void t_limit(void)
{
	unsigned char a[MAXFD], b[MAXFD];
	printf("\n[T4] receiver near RLIMIT_NOFILE: 2 free fd slots, 5 fds sent\n");
	int sp[2], p[2];
	socketpair(AF_UNIX, SOCK_STREAM, 0, sp);
	pipe(p);
	int fds[5] = { p[0], p[0], p[0], p[0], p[0] };
	int e = send_fds(sp[0], fds, 5);
	snap(a);
	int freec = 0, L;
	for (L = 0; L < MAXFD; L++) {
		if (!a[L] && ++freec == 2) {
			L++;
			break;
		}
	}
	struct rlimit old, nl;
	getrlimit(RLIMIT_NOFILE, &old);
	nl = old;
	nl.rlim_cur = L;
	int se = setrlimit(RLIMIT_NOFILE, &nl) ? errno : 0;
	struct rres o;
	recv_fds(sp[1], 5, 0, MSG_DONTWAIT, &o);
	setrlimit(RLIMIT_NOFILE, &old);
	snap(b);
	printf("  send=%s setrlimit(soft=%d)=%s recv r=%zd err=%s ctrunc=%d fds_reported=%d "
	       "new_fds_in_table=%d\n",
	    en(e), L, en(se), o.r, en(o.err), !!(o.flags & MSG_CTRUNC), o.nfds, newfds(a, b));
	closenew(a, b);
	struct rres o2;
	recv_fds(sp[1], 5, 0, MSG_DONTWAIT, &o2);
	printf("  follow-up recv (limit restored): r=%zd err=%s nfds=%d\n", o2.r, en(o2.err), o2.nfds);
	closeall(&o2);
	close(p[0]); close(p[1]); close(sp[0]); close(sp[1]);
}

static void t_cloexec(void)
{
	printf("\n[T5] close-on-exec of received fds\n");
	int sp[2], p[2];
	socketpair(AF_UNIX, SOCK_STREAM, 0, sp);
	pipe(p);
	fcntl(p[0], F_SETFD, FD_CLOEXEC);
	struct rres o;
	int e = send_fds(sp[0], &p[0], 1);
	recv_fds(sp[1], 1, 0, 0, &o);
	printf("  sender fd has FD_CLOEXEC; recv flags=0: send=%s r=%zd nfds=%d FD_CLOEXEC=%d\n", en(e),
	    o.r, o.nfds, o.nfds ? !!(fcntl(o.fds[0], F_GETFD) & FD_CLOEXEC) : -1);
	closeall(&o);
#ifdef MSG_CMSG_CLOEXEC
	int fl = MSG_CMSG_CLOEXEC;
	const char *nm = "MSG_CMSG_CLOEXEC";
#else
	int fl = 0x40000000;
	const char *nm = "0x40000000 (Linux MSG_CMSG_CLOEXEC value; undefined here)";
#endif
	e = send_fds(sp[0], &p[0], 1);
	recv_fds(sp[1], 1, 0, fl, &o);
	printf("  recv flags=%s: send=%s r=%zd err=%s nfds=%d FD_CLOEXEC=%d\n", nm, en(e), o.r, en(o.err),
	    o.nfds, o.nfds ? !!(fcntl(o.fds[0], F_GETFD) & FD_CLOEXEC) : -1);
	closeall(&o);
	close(p[0]); close(p[1]); close(sp[0]); close(sp[1]);
}

static void try_type(int sp0, int sp1, const char *name, int fd, int ferr)
{
	if (fd < 0) {
		printf("  %-26s could not create: %s\n", name, en(ferr));
		return;
	}
	int e = send_fds(sp0, &fd, 1);
	printf("  %-26s sendmsg=%s", name, en(e));
	if (!e) {
		struct rres o;
		recv_fds(sp1, 1, 0, 0, &o);
		struct stat s1, s2;
		int same = -1;
		if (o.nfds == 1 && fstat(fd, &s1) == 0 && fstat(o.fds[0], &s2) == 0)
			same = s1.st_dev == s2.st_dev && s1.st_ino == s2.st_ino;
		printf("  received=%d same_inode=%d", o.nfds, same);
		closeall(&o);
	}
	printf("\n");
	close(fd);
}

static void t_types(void)
{
	printf("\n[T6] which fd kinds can be sent\n");
	int sp[2];
	socketpair(AF_UNIX, SOCK_STREAM, 0, sp);
	int p[2];
	pipe(p);
	try_type(sp[0], sp[1], "pipe read end", p[0], 0);
	try_type(sp[0], sp[1], "pipe write end", p[1], 0);
	int us[2];
	socketpair(AF_UNIX, SOCK_STREAM, 0, us);
	try_type(sp[0], sp[1], "AF_UNIX stream socket", us[0], 0);
	close(us[1]);
	int ls = socket(AF_UNIX, SOCK_STREAM, 0);
	try_type(sp[0], sp[1], "AF_UNIX unbound socket", ls, errno);
	int ts = socket(AF_INET, SOCK_STREAM, 0);
	try_type(sp[0], sp[1], "AF_INET TCP socket", ts, errno);
	char tmpl[] = "/tmp/fdprobe.file.XXXXXX";
	int rf = mkstemp(tmpl);
	unlink(tmpl);
	try_type(sp[0], sp[1], "regular file", rf, errno);
	int df = open("/tmp", O_RDONLY | O_DIRECTORY);
	try_type(sp[0], sp[1], "directory", df, errno);
	int dn = open("/dev/null", O_RDWR);
	try_type(sp[0], sp[1], "/dev/null (char device)", dn, errno);
	int m = posix_openpt(O_RDWR | O_NOCTTY);
	int merr = errno;
	int s = -1, serr = 0;
	if (m >= 0 && grantpt(m) == 0 && unlockpt(m) == 0) {
		char *sn = ptsname(m);
		s = sn ? open(sn, O_RDWR | O_NOCTTY) : -1;
		serr = errno;
	}
	try_type(sp[0], sp[1], "pty slave (tty)", s, serr);
	try_type(sp[0], sp[1], "pty master", m, merr);
#ifdef __APPLE__
	int kq = kqueue();
	try_type(sp[0], sp[1], "kqueue", kq, errno);
	char shmname[64];
	snprintf(shmname, sizeof shmname, "/fdprobe.%d", (int)getpid());
	int shm = shm_open(shmname, O_RDWR | O_CREAT | O_EXCL, 0600);
	int shmerr = errno;
	shm_unlink(shmname);
	try_type(sp[0], sp[1], "POSIX shm (shm_open)", shm, shmerr);
#else
	int ep = epoll_create1(0);
	try_type(sp[0], sp[1], "epoll", ep, errno);
	int ev = eventfd(0, 0);
	try_type(sp[0], sp[1], "eventfd", ev, errno);
	int op = open("/", O_PATH);
	try_type(sp[0], sp[1], "O_PATH fd", op, errno);
	int mf = memfd_create("fdprobe", 0);
	try_type(sp[0], sp[1], "memfd", mf, errno);
	int pf = (int)syscall(434 /* __NR_pidfd_open */, getpid(), 0);
	try_type(sp[0], sp[1], "pidfd", pf, errno);
	unsigned char params[120];
	memset(params, 0, sizeof params);
	int ur = (int)syscall(425 /* __NR_io_uring_setup */, 1, params);
	try_type(sp[0], sp[1], "io_uring", ur, errno);
#endif
	close(sp[0]);
	close(sp[1]);
}

static void t_shared_ofd(void)
{
	printf("\n[T7] received fd shares the open file description (status flags)\n");
	int sp[2], p[2];
	socketpair(AF_UNIX, SOCK_STREAM, 0, sp);
	pipe(p);
	struct rres o;
	send_fds(sp[0], &p[1], 1);
	recv_fds(sp[1], 1, 0, 0, &o);
	int before = !!(fcntl(p[1], F_GETFL) & O_NONBLOCK);
	fcntl(o.fds[0], F_SETFL, fcntl(o.fds[0], F_GETFL) | O_NONBLOCK);
	int after = !!(fcntl(p[1], F_GETFL) & O_NONBLOCK);
	printf("  pipe: sender's fd O_NONBLOCK before=%d after receiver set it on its copy=%d\n", before,
	    after);
	closeall(&o);
	int m = posix_openpt(O_RDWR | O_NOCTTY);
	if (m >= 0 && grantpt(m) == 0 && unlockpt(m) == 0) {
		int s = open(ptsname(m), O_RDWR | O_NOCTTY);
		send_fds(sp[0], &s, 1);
		recv_fds(sp[1], 1, 0, 0, &o);
		before = !!(fcntl(s, F_GETFL) & O_NONBLOCK);
		fcntl(o.fds[0], F_SETFL, fcntl(o.fds[0], F_GETFL) | O_NONBLOCK);
		after = !!(fcntl(s, F_GETFL) & O_NONBLOCK);
		printf("  pty slave: sender's fd O_NONBLOCK before=%d after=%d\n", before, after);
		off_t x = lseek(s, 0, SEEK_CUR);
		(void)x;
		closeall(&o);
		close(s);
		close(m);
	}
	char tmpl[] = "/tmp/fdprobe.off.XXXXXX";
	int rf = mkstemp(tmpl);
	unlink(tmpl);
	write(rf, "0123456789", 10);
	lseek(rf, 2, SEEK_SET);
	send_fds(sp[0], &rf, 1);
	recv_fds(sp[1], 1, 0, 0, &o);
	lseek(o.fds[0], 7, SEEK_SET);
	printf("  regular file: sender offset after receiver lseek(7) = %lld\n",
	    (long long)lseek(rf, 0, SEEK_CUR));
	closeall(&o);
	close(rf);
	close(p[0]); close(p[1]); close(sp[0]); close(sp[1]);
}

static void t_peek(void)
{
	unsigned char a[MAXFD], b[MAXFD];
	printf("\n[T8] MSG_PEEK on a message carrying 2 fds\n");
	int sp[2], p[2];
	socketpair(AF_UNIX, SOCK_STREAM, 0, sp);
	pipe(p);
	int fds[2] = { p[0], p[0] };
	send_fds(sp[0], fds, 2);
	snap(a);
	struct rres o;
	recv_fds(sp[1], 2, 0, MSG_PEEK, &o);
	snap(b);
	printf("  peek: r=%zd nfds=%d values=[%d,%d] new_fds_in_table=%d\n", o.r, o.nfds,
	    o.nfds > 0 ? o.fds[0] : -1, o.nfds > 1 ? o.fds[1] : -1, newfds(a, b));
	closenew(a, b);
	snap(a);
	recv_fds(sp[1], 2, 0, 0, &o);
	snap(b);
	printf("  real recv: r=%zd nfds=%d new_fds_in_table=%d\n", o.r, o.nfds, newfds(a, b));
	closenew(a, b);
	close(p[0]); close(p[1]); close(sp[0]); close(sp[1]);
}

static void t_inflight_one(int soft, int sndbuf)
{
	printf("\n[T9] in-flight: sender RLIMIT_NOFILE soft=%d, SO_SNDBUF=%d, 1 fd per message, receiver never reads\n", soft, sndbuf);
	fflush(stdout);
	pid_t c = fork();
	if (c == 0) {
		struct rlimit rl;
		getrlimit(RLIMIT_NOFILE, &rl);
		rl.rlim_cur = soft;
		setrlimit(RLIMIT_NOFILE, &rl);
		int sp[2], p[2];
		socketpair(AF_UNIX, SOCK_STREAM, 0, sp);
		pipe(p);
		fcntl(sp[0], F_SETFL, O_NONBLOCK);
		if (sndbuf > 0) {
			setsockopt(sp[0], SOL_SOCKET, SO_SNDBUF, &sndbuf, sizeof sndbuf);
			setsockopt(sp[1], SOL_SOCKET, SO_RCVBUF, &sndbuf, sizeof sndbuf);
		}
		int n = 0, e = 0;
		for (; n < 20000; n++) {
			e = send_fds(sp[0], &p[0], 1);
			if (e)
				break;
		}
		printf("  messages accepted before failure: %d, failure=%s\n", n, en(e));
		fflush(stdout);
		_exit(0);
	}
	waitpid(c, NULL, 0);
}

static void t_inflight(void)
{
	t_inflight_one(64, 0);
	t_inflight_one(4096, 0);
	t_inflight_one(4096, 1 << 20);
}

static void print_creds(const char *tag, int fd)
{
#ifdef __APPLE__
	uid_t eu = (uid_t)-1;
	gid_t eg = (gid_t)-1;
	int r = getpeereid(fd, &eu, &eg);
	struct xucred xc;
	socklen_t l = sizeof xc;
	memset(&xc, 0, sizeof xc);
	int r1 = getsockopt(fd, SOL_LOCAL, LOCAL_PEERCRED, &xc, &l);
	int e1 = r1 ? errno : 0;
	pid_t pp = -1, pe = -1;
	l = sizeof pp;
	int r2 = getsockopt(fd, SOL_LOCAL, LOCAL_PEERPID, &pp, &l);
	int e2 = r2 ? errno : 0;
	l = sizeof pe;
	int r3 = getsockopt(fd, SOL_LOCAL, LOCAL_PEEREPID, &pe, &l);
	int e3 = r3 ? errno : 0;
	audit_token_t at;
	memset(&at, 0, sizeof at);
	l = sizeof at;
	int r4 = getsockopt(fd, SOL_LOCAL, LOCAL_PEERTOKEN, &at, &l);
	int e4 = r4 ? errno : 0;
	printf("  %s: getpeereid=%s euid=%d | LOCAL_PEERCRED=%s uid=%d ngroups=%d | LOCAL_PEERPID=%s %d | "
	       "LOCAL_PEEREPID=%s %d | LOCAL_PEERTOKEN=%s pid=%u pidversion=%u euid=%u\n",
	    tag, en(r ? errno : 0), (int)eu, en(e1), (int)xc.cr_uid, xc.cr_ngroups, en(e2), (int)pp, en(e3),
	    (int)pe, en(e4), at.val[5], at.val[7], at.val[1]);
#else
	struct ucred uc;
	socklen_t l = sizeof uc;
	memset(&uc, 0, sizeof uc);
	int r1 = getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &uc, &l);
	int e1 = r1 ? errno : 0;
	int pfd = -1;
	l = sizeof pfd;
	int r2 = getsockopt(fd, SOL_SOCKET, SO_PEERPIDFD, &pfd, &l);
	int e2 = r2 ? errno : 0;
	char line[256] = "n/a";
	if (!r2 && pfd >= 0) {
		char path[64];
		snprintf(path, sizeof path, "/proc/self/fdinfo/%d", pfd);
		FILE *f = fopen(path, "r");
		if (f) {
			char buf[256];
			while (fgets(buf, sizeof buf, f))
				if (!strncmp(buf, "Pid:", 4)) {
					buf[strcspn(buf, "\n")] = 0;
					snprintf(line, sizeof line, "%s", buf);
				}
			fclose(f);
		}
		close(pfd);
	}
	printf("  %s: SO_PEERCRED=%s pid=%d uid=%d gid=%d | SO_PEERPIDFD=%s fdinfo[%s]\n", tag, en(e1),
	    (int)uc.pid, (int)uc.uid, (int)uc.gid, en(e2), line);
#endif
}

static void t_creds(const char *dir)
{
	printf("\n[T10] peer credentials over a listening socket\n");
	char path[200];
	snprintf(path, sizeof path, "%s/creds.sock", dir);
	unlink(path);
	int ls = socket(AF_UNIX, SOCK_STREAM, 0);
	struct sockaddr_un sa;
	memset(&sa, 0, sizeof sa);
	sa.sun_family = AF_UNIX;
	snprintf(sa.sun_path, sizeof sa.sun_path, "%s", path);
	if (bind(ls, (struct sockaddr *)&sa, sizeof sa) || listen(ls, 4)) {
		printf("  bind/listen failed: %s\n", en(errno));
		return;
	}
	int go[2], pidp[2];
	pipe(go);
	pipe(pidp);
	printf("  server pid=%d\n", (int)getpid());
	fflush(stdout);
	pid_t A = fork();
	if (A == 0) {
		close(go[1]);
		close(pidp[0]);
		int c = socket(AF_UNIX, SOCK_STREAM, 0);
		if (connect(c, (struct sockaddr *)&sa, sizeof sa)) {
			printf("  A connect failed %s\n", en(errno));
			_exit(1);
		}
		print_creds("A (client) view of server, right after connect", c);
		fflush(stdout);
		write(c, "a", 1);
		pid_t B = fork();
		if (B == 0) {
			pid_t me = getpid();
			write(pidp[1], &me, sizeof me);
			char x;
			read(go[0], &x, 1); /* wait for server */
			write(c, "b", 1);
			read(go[0], &x, 1); /* EOF when server done */
			_exit(0);
		}
		_exit(0); /* A exits, B keeps the socket */
	}
	close(go[0]);
	close(pidp[1]);
	int s = accept(ls, NULL, NULL);
	char ch;
	read(s, &ch, 1);
	printf("  A pid=%d\n", (int)A);
	print_creds("Q1 after 'a' from A", s);
	waitpid(A, NULL, 0);
	pid_t B = -1;
	read(pidp[0], &B, sizeof B);
	printf("  A exited and was reaped; B pid=%d (child of A) holds the client socket\n", (int)B);
	print_creds("Q2 after A exited", s);
#ifndef __APPLE__
	int one = 1;
	setsockopt(s, SOL_SOCKET, SO_PASSCRED, &one, sizeof one);
#endif
	write(go[1], "g", 1);
	/* receive 'b' with room for credentials cmsg */
	char buf[256];
	struct iovec iov = { &ch, 1 };
	struct msghdr msg;
	memset(&msg, 0, sizeof msg);
	msg.msg_iov = &iov;
	msg.msg_iovlen = 1;
	msg.msg_control = buf;
	msg.msg_controllen = sizeof buf;
	ssize_t r = recvmsg(s, &msg, 0);
	printf("  recv 'b' r=%zd\n", r);
#ifndef __APPLE__
	for (struct cmsghdr *c = CMSG_FIRSTHDR(&msg); c; c = CMSG_NXTHDR(&msg, c))
		if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_CREDENTIALS) {
			struct ucred u;
			memcpy(&u, CMSG_DATA(c), sizeof u);
			printf("  SCM_CREDENTIALS on 'b': pid=%d uid=%d gid=%d\n", (int)u.pid, (int)u.uid,
			    (int)u.gid);
		}
#endif
	print_creds("Q3 after 'b' from B", s);
	close(go[1]);
	close(s);
	close(ls);
	unlink(path);
	int sp[2];
	socketpair(AF_UNIX, SOCK_STREAM, 0, sp);
	print_creds("socketpair end", sp[0]);
	close(sp[0]);
	close(sp[1]);
}

static int bind_len(const char *path, size_t pathlen, int with_nul, size_t *addrlen_out, int do_connect)
{
	unsigned char buf[600];
	memset(buf, 0, sizeof buf);
	struct sockaddr_un *su = (struct sockaddr_un *)buf;
	su->sun_family = AF_UNIX;
	size_t off = offsetof(struct sockaddr_un, sun_path);
	memcpy(buf + off, path, pathlen);
	size_t alen = off + pathlen + (with_nul ? 1 : 0);
#ifdef __APPLE__
	su->sun_len = (unsigned char)(alen > 255 ? 255 : alen);
#endif
	*addrlen_out = alen;
	int s = socket(AF_UNIX, SOCK_STREAM, 0);
	int e = bind(s, (struct sockaddr *)buf, (socklen_t)alen) ? errno : 0;
	if (!e && do_connect) {
		listen(s, 1);
		int c = socket(AF_UNIX, SOCK_STREAM, 0);
		int ce = connect(c, (struct sockaddr *)buf, (socklen_t)alen) ? errno : 0;
		printf("    connect to it: %s\n", en(ce));
		close(c);
	}
	close(s);
	return e;
}

static void t_paths(const char *dir)
{
	printf("\n[T11] sun_path length (sizeof sun_path=%zu, offsetof=%zu, sizeof sockaddr_un=%zu)\n",
	    sizeof(((struct sockaddr_un *)0)->sun_path), offsetof(struct sockaddr_un, sun_path),
	    sizeof(struct sockaddr_un));
	size_t dl = strlen(dir);
	size_t sunp = sizeof(((struct sockaddr_un *)0)->sun_path);
	size_t targets[] = { sunp - 1, sunp, sunp + 1, 150, 253, 254 };
	for (unsigned i = 0; i < sizeof targets / sizeof targets[0]; i++) {
		size_t L = targets[i];
		char path[600];
		size_t namelen = L - dl - 1;
		if (namelen > 255 || L <= dl + 1) {
			printf("  L=%zu skipped (component too long)\n", L);
			continue;
		}
		snprintf(path, sizeof path, "%s/", dir);
		memset(path + dl + 1, 'n', namelen);
		path[L] = 0;
		for (int nul = 1; nul >= 0; nul--) {
			size_t alen;
			unlink(path);
			int e = bind_len(path, L, nul, &alen, 1);
			struct stat st;
			int exists = stat(path, &st) == 0;
			char trunc[600];
			snprintf(trunc, sizeof trunc, "%.*s", (int)(sunp - 1), path);
			int trunc_exists = L >= sunp && lstat(trunc, &st) == 0;
			printf("  pathlen=%zu nul=%d addrlen=%zu bind=%s full_path_exists=%d truncated_path_exists=%d\n",
			    L, nul, alen, en(e), exists, trunc_exists);
			unlink(path);
			if (trunc_exists)
				unlink(trunc);
		}
	}
	{
		size_t alen;
		char p[300];
		memset(p, 'z', 290);
		p[290] = 0;
		int e = bind_len(p, 290, 1, &alen, 0);
		printf("  addrlen=%zu (>255) bind=%s\n", alen, en(e));
	}
}

static void t_bind_conn(const char *dir)
{
	printf("\n[T12] bind/connect edge cases\n");
	char path[200];
	struct sockaddr_un sa;
	memset(&sa, 0, sizeof sa);
	sa.sun_family = AF_UNIX;
	snprintf(path, sizeof path, "%s/b.sock", dir);
	snprintf(sa.sun_path, sizeof sa.sun_path, "%s", path);
	socklen_t alen = (socklen_t)(offsetof(struct sockaddr_un, sun_path) + strlen(path) + 1);

	/* regular file at path */
	unlink(path);
	int f = open(path, O_CREAT | O_WRONLY, 0600);
	close(f);
	int s = socket(AF_UNIX, SOCK_STREAM, 0);
	printf("  bind on existing regular file: %s\n", en(bind(s, (struct sockaddr *)&sa, alen) ? errno : 0));
	close(s);
	s = socket(AF_UNIX, SOCK_STREAM, 0);
	printf("  connect to regular file: %s\n", en(connect(s, (struct sockaddr *)&sa, alen) ? errno : 0));
	close(s);
	unlink(path);

	/* stale socket file */
	s = socket(AF_UNIX, SOCK_STREAM, 0);
	bind(s, (struct sockaddr *)&sa, alen);
	listen(s, 1);
	close(s);
	s = socket(AF_UNIX, SOCK_STREAM, 0);
	printf("  connect to stale socket file (listener closed): %s\n",
	    en(connect(s, (struct sockaddr *)&sa, alen) ? errno : 0));
	close(s);
	s = socket(AF_UNIX, SOCK_STREAM, 0);
	printf("  bind on stale socket file: %s\n", en(bind(s, (struct sockaddr *)&sa, alen) ? errno : 0));
	close(s);
	unlink(path);

	/* live listener */
	int ls = socket(AF_UNIX, SOCK_STREAM, 0);
	bind(ls, (struct sockaddr *)&sa, alen);
	listen(ls, 8);
	s = socket(AF_UNIX, SOCK_STREAM, 0);
	printf("  bind on live listener's path: %s\n", en(bind(s, (struct sockaddr *)&sa, alen) ? errno : 0));
	close(s);
	int modes[] = { 0777, 0200, 0500, 0400, 0 };
	for (unsigned i = 0; i < sizeof modes / sizeof modes[0]; i++) {
		chmod(path, modes[i]);
		s = socket(AF_UNIX, SOCK_STREAM, 0);
		int e = connect(s, (struct sockaddr *)&sa, alen) ? errno : 0;
		printf("  connect (owner uid=%d) to socket file mode %04o: %s\n", (int)getuid(), modes[i], en(e));
		close(s);
		if (!e) {
			int a = accept(ls, NULL, NULL);
			close(a);
		}
	}
	close(ls);
	unlink(path);

	/* nonexistent */
	s = socket(AF_UNIX, SOCK_STREAM, 0);
	printf("  connect to nonexistent path: %s\n", en(connect(s, (struct sockaddr *)&sa, alen) ? errno : 0));
	close(s);

	/* umask */
	mode_t ums[] = { 0077, 0022, 0000 };
	for (unsigned i = 0; i < 3; i++) {
		mode_t old = umask(ums[i]);
		s = socket(AF_UNIX, SOCK_STREAM, 0);
		int e = bind(s, (struct sockaddr *)&sa, alen) ? errno : 0;
		struct stat st;
		stat(path, &st);
		printf("  umask %04o -> bind=%s socket file mode %04o (S_ISSOCK=%d)\n", ums[i], en(e),
		    (unsigned)(st.st_mode & 07777), S_ISSOCK(st.st_mode));
		close(s);
		unlink(path);
		umask(old);
	}

	/* abstract namespace */
	struct sockaddr_un ab;
	memset(&ab, 0, sizeof ab);
	ab.sun_family = AF_UNIX;
	memcpy(ab.sun_path, "\0fdprobe-abstract", 17);
	socklen_t ablen = (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 17);
	int a1 = socket(AF_UNIX, SOCK_STREAM, 0);
	int e1 = bind(a1, (struct sockaddr *)&ab, ablen) ? errno : 0;
	int a2 = socket(AF_UNIX, SOCK_STREAM, 0);
	int e2 = bind(a2, (struct sockaddr *)&ab, ablen) ? errno : 0;
	printf("  abstract name bind=%s, second bind of same name=%s\n", en(e1), en(e2));
	close(a1);
	close(a2);
#ifndef __APPLE__
	int sp[2];
	socketpair(AF_UNIX, SOCK_STREAM, 0, sp);
	int zero = 0;
	int pe = setsockopt(sp[1], SOL_SOCKET, SO_PASSRIGHTS, &zero, sizeof zero) ? errno : 0;
	printf("  SO_PASSRIGHTS=0 on receiver: setsockopt=%s", en(pe));
	if (!pe) {
		int p[2];
		pipe(p);
		printf(", then sendmsg(SCM_RIGHTS)=%s", en(send_fds(sp[0], &p[0], 1)));
		close(p[0]);
		close(p[1]);
	}
	printf("\n");
	close(sp[0]);
	close(sp[1]);
#endif
}

int main(void)
{
	setvbuf(stdout, NULL, _IOLBF, 0);
	signal(SIGPIPE, SIG_IGN);
	struct utsname u;
	uname(&u);
	printf("host: %s %s %s %s uid=%d\n", u.sysname, u.release, u.version, u.machine, (int)getuid());
	raise_nofile();
	char dir[] = "/tmp/fdp.XXXXXX";
	if (!mkdtemp(dir)) {
		perror("mkdtemp");
		return 1;
	}
	printf("tempdir=%s (len %zu)\n", dir, strlen(dir));
	t_max();
	t_trunc();
	t_limit();
	t_cloexec();
	t_types();
	t_shared_ofd();
	t_peek();
	t_inflight();
	t_creds(dir);
	t_paths(dir);
	t_bind_conn(dir);
	rmdir(dir);
	return 0;
}
