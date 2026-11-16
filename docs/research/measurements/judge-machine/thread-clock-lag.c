/* The process CPU clock beside a spinning thread, against that thread's own clock (D209 Part 5,
   CI run 43575280954): gcc -O2 -pthread thread-clock-lag.c -o lag && ./lag PACE_US. Counts the looks
   at which the process clock less the looking thread's bracketed run showed no other thread's
   run, and those at which the spinner's own clock did not grow. */
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <time.h>
#include <stdatomic.h>
static atomic_int stop;
static void *spin(void *a){ volatile uint64_t n=0; while(!atomic_load(&stop)) n++; return 0; }
static uint64_t rd(clockid_t c){ struct timespec t; clock_gettime(c,&t); return (uint64_t)t.tv_sec*1000000000ull+t.tv_nsec; }
int main(int argc,char**argv){
  long us=argc>1?atol(argv[1]):239;
  pthread_t th; pthread_create(&th,0,spin,0); clockid_t sc; pthread_getcpuclockid(th,&sc);
  struct timespec ms={0,us*1000}; nanosleep(&ms,0);
  uint64_t lb=rd(CLOCK_THREAD_CPUTIME_ID), lp=rd(CLOCK_PROCESS_CPUTIME_ID), ls=rd(sc);
  int n=0, still_proc=0, still_thread=0; uint64_t t0=rd(CLOCK_MONOTONIC), lastgrow=t0, maxgap=0;
  for(int i=0;i<2000;i++){
    nanosleep(&ms,0);
    uint64_t b=rd(CLOCK_THREAD_CPUTIME_ID), p=rd(CLOCK_PROCESS_CPUTIME_ID), a=rd(CLOCK_THREAD_CPUTIME_ID), s=rd(sc), now=rd(CLOCK_MONOTONIC);
    int64_t others=(int64_t)(p-lp)-(int64_t)(a-lb);
    n++;
    if(others<=0){ still_proc++; if(now-lastgrow>maxgap) maxgap=now-lastgrow; } else lastgrow=now;
    if(s==ls) still_thread++;
    lb=b; lp=p; ls=s;
  }
  double wall=(rd(CLOCK_MONOTONIC)-t0)/1e6;
  atomic_store(&stop,1); pthread_join(th,0);
  printf("pace %ldus: %d looks over %.1f ms; process clock less waiter still: %d; spinner thread clock still: %d; longest no-growth of process clock beside the waiter: %.3f ms\n",us,n,wall,still_proc,still_thread,maxgap/1e6);
  return 0;
}
