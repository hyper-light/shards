// N threads spinning for SECONDS at the vCPUs' policy (QoS user-interactive plus
// THREAD_TIME_CONSTRAINT_POLICY, 0.5 ms in 1 ms), watched every 50 ms by the main thread,
// itself at a tighter time constraint (0.1 ms in 0.2 ms) so that it wins their deadlines:
// one line a look, the seconds since the start, then each thread's policy (r real-time, t
// timeshare) and CPU ms since the look before.
//   cc -O2 -o spin-timeline spin-timeline.c && ./spin-timeline N SECONDS
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <mach/thread_policy.h>
#include <pthread.h>
#include <pthread/qos.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

static double seconds;

static double now(void) {
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	return t.tv_sec + t.tv_nsec / 1e9;
}

static void realtime(uint64_t computation_ns, uint64_t constraint_ns) {
	pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
	mach_timebase_info_data_t tb;
	mach_timebase_info(&tb);
	thread_time_constraint_policy_data_t p = {
		.period = 0,
		.computation = (uint32_t)(computation_ns * tb.denom / tb.numer),
		.constraint = (uint32_t)(constraint_ns * tb.denom / tb.numer),
		.preemptible = 1,
	};
	if (thread_policy_set(pthread_mach_thread_np(pthread_self()), THREAD_TIME_CONSTRAINT_POLICY,
	        (thread_policy_t)&p, THREAD_TIME_CONSTRAINT_POLICY_COUNT) != KERN_SUCCESS)
		fprintf(stderr, "time-constraint policy refused\n");
}

static void *spin(void *arg) {
	(void)arg;
	realtime(500000, 1000000);
	double end = now() + seconds;
	volatile unsigned long n = 0;
	while (now() < end)
		for (int i = 0; i < 100000; i++)
			n++;
	return NULL;
}

int main(int argc, char **argv) {
	if (argc != 3)
		return 2;
	int n = atoi(argv[1]);
	seconds = atof(argv[2]);
	realtime(100000, 200000);
	pthread_t t[64];
	thread_t m[64];
	double last[64] = {0};
	double start = now();
	for (int i = 0; i < n && i < 64; i++) {
		pthread_create(&t[i], NULL, spin, NULL);
		m[i] = pthread_mach_thread_np(t[i]);
	}
	while (now() - start < seconds) {
		usleep(50000);
		printf("%.2f", now() - start);
		for (int i = 0; i < n && i < 64; i++) {
			thread_basic_info_data_t info;
			mach_msg_type_number_t count = THREAD_BASIC_INFO_COUNT;
			if (thread_info(m[i], THREAD_BASIC_INFO, (thread_info_t)&info, &count) != KERN_SUCCESS)
				continue;
			double cpu = info.user_time.seconds * 1e3 + info.user_time.microseconds / 1e3 +
			             info.system_time.seconds * 1e3 + info.system_time.microseconds / 1e3;
			printf(" %c%.0f", info.policy == POLICY_TIMESHARE ? 't' : 'r', cpu - last[i]);
			last[i] = cpu;
		}
		printf("\n");
	}
	for (int i = 0; i < n && i < 64; i++)
		pthread_join(t[i], NULL);
	return 0;
}
