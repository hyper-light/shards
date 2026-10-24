// What becomes of a thread's QoS class as Mach policies are set on it: QoS user-interactive
// asked, then the vCPUs' time-constraint policy (0.5 ms in 1 ms), then timeshare, its class
// read after each (pthread_get_qos_class_np). On macOS 26.4.1: 0x21, then 0 (unspecified)
// and 0: the time-constraint policy takes the class off as it is set (osfmk/kern/
// thread_policy.c, thread_policy_set; PM M317).
//   cc -O2 -o policy-qos policy-qos.c && ./policy-qos
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <mach/thread_policy.h>
#include <pthread.h>
#include <pthread/qos.h>
#include <stdio.h>
int main(void) {
	qos_class_t q; int rel;
	pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
	pthread_get_qos_class_np(pthread_self(), &q, &rel);
	printf("after QoS UI: 0x%x\n", q);
	mach_timebase_info_data_t tb; mach_timebase_info(&tb);
	thread_time_constraint_policy_data_t p = { 0, (uint32_t)(500000ull * tb.denom / tb.numer), (uint32_t)(1000000ull * tb.denom / tb.numer), 1 };
	kern_return_t kr = thread_policy_set(pthread_mach_thread_np(pthread_self()), THREAD_TIME_CONSTRAINT_POLICY, (thread_policy_t)&p, THREAD_TIME_CONSTRAINT_POLICY_COUNT);
	pthread_get_qos_class_np(pthread_self(), &q, &rel);
	printf("after time-constraint (kr %d): 0x%x\n", kr, q);
	thread_extended_policy_data_t ts = { 1 };
	kr = thread_policy_set(pthread_mach_thread_np(pthread_self()), THREAD_EXTENDED_POLICY, (thread_policy_t)&ts, THREAD_EXTENDED_POLICY_COUNT);
	pthread_get_qos_class_np(pthread_self(), &q, &rel);
	printf("after timeshare (kr %d): 0x%x\n", kr, q);
	return 0;
}
