// What reading a file costs just after the process that had it mapped shared and wrote it
// exits (D190, PM M278): a microVM's writable layer, which its VM process maps shared and
// its guest writes, read by the daemon as soon as the VM's socket ends.
//
// A child makes nothing but a mapping of a sparse file of SIZE_GIB GiB, MAP_SHARED and
// writable, writes DIRTY_KIB KiB of it, a page in each 32 KiB from its start (as ext4's
// metadata and journal lie in a layer), and ends, as MODE says:
//   exit    exit with the mapping in place, its pipe to the parent closed as it exits, as
//           a VM process's socket to the daemon is
//   munmap  the mapping unmapped first (timed), then the pipe closed, then exit
//   msync   msync(MS_SYNC) of the mapping first (timed), unmapped, the pipe closed, exit
//   async   msync(MS_ASYNC) of the mapping first (timed), then exit
//   close   the file kept open, as a VM process keeps its layer's: the mapping unmapped,
//           then the file closed (both timed), the pipe closed, then exit
//   held    the file kept open: the pipe closed, then exit, its mapping and the file
//           going as it exits, after the parent hears it end, as a VM process's socket
//           ends before it exits
// The parent waits for the pipe's end, as the daemon waits for its socket's, then opens
// the file and reads 4 KiB at its start and at the last page written, and records how
// long after the end that took, and how long until the child is reaped. With `filled`,
// the file is written whole first (and synced), its blocks all allocated, where it is
// otherwise sparse, as a layer's file is.
//
//   cc -O2 -o unmap unmap.c && ./unmap DIR SIZE_GIB DIRTY_KIB MODE ROUNDS [filled]
//
// Prints n, p50, p90, p99 and max in microseconds.
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static double now_us(void) {
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ts.tv_sec * 1e6 + ts.tv_nsec / 1e3;
}

static int cmp(const void *a, const void *b) {
	double x = *(const double *)a, y = *(const double *)b;
	return (x > y) - (x < y);
}

static void report(const char *what, double *xs, int n) {
	qsort(xs, n, sizeof *xs, cmp);
	printf("%-10s n %d p50 %10.1f p90 %10.1f p99 %10.1f max %10.1f us\n", what, n,
	       xs[n / 2], xs[(int)(n * 0.9)], xs[(int)(n * 0.99)], xs[n - 1]);
}

int main(int argc, char **argv) {
	if (argc != 6 && !(argc == 7 && !strcmp(argv[6], "filled"))) {
		fprintf(stderr, "usage: %s DIR SIZE_GIB DIRTY_KIB exit|munmap|msync|async|close|held ROUNDS [filled]\n", argv[0]);
		return 2;
	}
	int filled = argc == 7;
	// As a restore fills a layer before its guest runs (copy_used): KiB written with
	// pwrite first, a page in each 32 KiB from the file's middle. And as the daemon keeps
	// a layer: the file renamed before it is read.
	size_t copied = getenv("UNMAP_COPY") ? (size_t)strtoull(getenv("UNMAP_COPY"), 0, 10) << 10 : 0;
	int renamed = getenv("UNMAP_RENAME") != 0;
	const char *dir = argv[1];
	size_t size = (size_t)strtoull(argv[2], 0, 10) << 30;
	size_t dirty = (size_t)strtoull(argv[3], 0, 10) << 10;
	const char *mode = argv[4];
	int rounds = atoi(argv[5]);
	const size_t page = 4096, stride = 32768;
	if (strcmp(mode, "exit") && strcmp(mode, "munmap") && strcmp(mode, "msync") && strcmp(mode, "async") &&
	    strcmp(mode, "close") && strcmp(mode, "held")) {
		fprintf(stderr, "mode: exit, munmap, msync, async, close or held\n");
		return 2;
	}
	int keep_open = !strcmp(mode, "close") || !strcmp(mode, "held");
	size_t pages = dirty / page;
	if (pages == 0 || pages * stride > size) {
		fprintf(stderr, "DIRTY_KIB: 4 or more, a page in each 32 KiB of the file\n");
		return 2;
	}
	double *read_after = calloc(rounds, sizeof(double));
	double *reaped_after = calloc(rounds, sizeof(double));
	double *child_took = calloc(rounds, sizeof(double));
	char path[4096];
	for (int r = 0; r < rounds; r++) {
		snprintf(path, sizeof path, "%s/unmap-%d-%d", dir, (int)getpid(), r);
		int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
		if (fd < 0 || ftruncate(fd, (off_t)size) != 0) {
			perror(path);
			return 1;
		}
		for (size_t i = 0; i < copied / page; i++) {
			static unsigned char ones[4096];
			memset(ones, 1, sizeof ones);
			if (pwrite(fd, ones, page, (off_t)(size / 2 + i * stride)) != (ssize_t)page) {
				perror(path);
				return 1;
			}
		}
		if (filled) {
			static unsigned char zeros[1 << 20];
			for (size_t at = 0; at < size; at += sizeof zeros) {
				if (pwrite(fd, zeros, sizeof zeros, (off_t)at) != sizeof zeros) {
					perror(path);
					return 1;
				}
			}
			fsync(fd);
		}
		close(fd);
		int p[2];
		if (pipe(p) != 0) {
			perror("pipe");
			return 1;
		}
		pid_t child = fork();
		if (child < 0) {
			perror("fork");
			return 1;
		}
		if (child == 0) {
			close(p[0]);
			int f = open(path, O_RDWR);
			unsigned char *m = mmap(0, size, PROT_READ | PROT_WRITE, MAP_SHARED, f, 0);
			if (f < 0 || m == MAP_FAILED)
				_exit(1);
			if (!keep_open)
				close(f);
			for (size_t i = 0; i < pages; i++)
				memset(m + i * stride, 0x5a, page);
			double t = now_us();
			if (!strcmp(mode, "msync"))
				msync(m, size, MS_SYNC);
			if (!strcmp(mode, "async"))
				msync(m, size, MS_ASYNC);
			else if (!strcmp(mode, "close")) {
				munmap(m, size);
				close(f);
			} else if (strcmp(mode, "exit") && strcmp(mode, "held"))
				munmap(m, size);
			double took = now_us() - t;
			if (write(p[1], &took, sizeof took) != sizeof took)
				_exit(1);
			if (strcmp(mode, "exit") && strcmp(mode, "async"))
				close(p[1]);
			// Held: the parent hears the end first, then the mapping and the file go.
			_exit(0);
		}
		close(p[1]);
		double took = 0;
		if (read(p[0], &took, sizeof took) != sizeof took) {
			fprintf(stderr, "the child failed\n");
			return 1;
		}
		char c;
		while (read(p[0], &c, 1) > 0) {
		}
		double ended = now_us();
		close(p[0]);
		char kept[4200];
		snprintf(kept, sizeof kept, "%s.kept", path);
		if (renamed) {
			if (rename(path, kept) != 0) {
				perror(kept);
				return 1;
			}
			memcpy(path, kept, strlen(kept) + 1);
		}
		int f = open(path, O_RDONLY);
		unsigned char buf[4096];
		if (f < 0 || pread(f, buf, sizeof buf, 0) != sizeof buf ||
		    pread(f, buf, sizeof buf, (off_t)((pages - 1) * stride)) != sizeof buf || buf[0] != 0x5a) {
			fprintf(stderr, "reading %s: %s\n", path, strerror(errno));
			return 1;
		}
		read_after[r] = now_us() - ended;
		close(f);
		int status;
		waitpid(child, &status, 0);
		reaped_after[r] = now_us() - ended;
		child_took[r] = took;
		unlink(path);
	}
	printf("%s: %zu GiB%s, %zu KiB written, %s\n", dir, size >> 30, filled ? " filled" : "",
	       dirty >> 10, mode);
	report("read", read_after, rounds);
	report("reaped", reaped_after, rounds);
	report("child", child_took, rounds);
	return 0;
}
