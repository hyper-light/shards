// N threads spinning for SECONDS at a policy: `rt`, the vCPUs' (QoS user-interactive
// plus THREAD_TIME_CONSTRAINT_POLICY, 0.5 ms of computation in a 1 ms constraint,
// crates/vmm/src/platform/macos_thread.rs), `ui` (QoS user-interactive alone) or `default`
// (as started). Prints the CPU time each thread got, their sum over the wall time (the
// CPUs they used together), and the scheduling policy each was in as it ended.
//   cc -O2 -o spin spin.c && ./spin POLICY N SECONDS
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <mach/thread_policy.h>
#include <pthread.h>
#include <pthread/qos.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static const char *policy;
static double seconds;

static double now(void) {
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	return t.tv_sec + t.tv_nsec / 1e9;
}

static void *spin(void *out) {
	if (strcmp(policy, "default") != 0)
		pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
	if (strcmp(policy, "rt") == 0) {
		mach_timebase_info_data_t tb;
		mach_timebase_info(&tb);
		thread_time_constraint_policy_data_t p = {
			.period = 0,
			.computation = (uint32_t)(500000ull * tb.denom / tb.numer),
			.constraint = (uint32_t)(1000000ull * tb.denom / tb.numer),
			.preemptible = 1,
		};
		if (thread_policy_set(pthread_mach_thread_np(pthread_self()), THREAD_TIME_CONSTRAINT_POLICY,
		        (thread_policy_t)&p, THREAD_TIME_CONSTRAINT_POLICY_COUNT) != KERN_SUCCESS)
			fprintf(stderr, "time-constraint policy refused\n");
	}
	double end = now() + seconds;
	volatile unsigned long n = 0;
	while (now() < end)
		for (int i = 0; i < 100000; i++)
			n++;
	struct timespec c;
	clock_gettime(CLOCK_THREAD_CPUTIME_ID, &c);
	thread_basic_info_data_t info;
	mach_msg_type_number_t count = THREAD_BASIC_INFO_COUNT;
	thread_info(pthread_mach_thread_np(pthread_self()), THREAD_BASIC_INFO, (thread_info_t)&info, &count);
	double *r = out;
	r[0] = c.tv_sec + c.tv_nsec / 1e9;
	r[1] = info.policy;
	return NULL;
}

int main(int argc, char **argv) {
	if (argc != 4)
		return 2;
	policy = argv[1];
	int n = atoi(argv[2]);
	seconds = atof(argv[3]);
	pthread_t t[256];
	double r[256][2];
	double start = now();
	for (int i = 0; i < n && i < 256; i++)
		pthread_create(&t[i], NULL, spin, r[i]);
	double sum = 0;
	for (int i = 0; i < n && i < 256; i++) {
		pthread_join(t[i], NULL);
		sum += r[i][0];
	}
	double wall = now() - start;
	printf("%s %d threads: %.2f CPUs used over %.2f s; per thread", policy, n, sum / wall, wall);
	for (int i = 0; i < n && i < 256; i++)
		printf(" %.2f%s", r[i][0] / wall, r[i][1] == POLICY_TIMESHARE ? "ts" : "rt");
	printf("\n");
	return 0;
}
