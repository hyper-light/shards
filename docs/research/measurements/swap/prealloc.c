/* How long reserving a run's swap file's disk takes, against leaving it sparse
 * (D159, docs/research/platform-measurements.md M230): open(O_CREAT|O_TRUNC), then
 * F_PREALLOCATE of MIB (F_ALLOCATEALL, from the end of the file) or nothing, then
 * ftruncate to MIB; timed from the open's return to the ftruncate's, N times, each file
 * unlinked after.
 *
 *     clang -O2 -o prealloc prealloc.c && ./prealloc PATH MIB N PREALLOCATE(0|1)
 */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec*1e6+t.tv_nsec/1e3;}
static int cmp(const void*a,const void*b){double x=*(double*)a,y=*(double*)b;return x<y?-1:x>y;}
int main(int argc,char**argv){
  const char*p=argv[1]; long long mib=atoll(argv[2]); int n=atoi(argv[3]); int pre=atoi(argv[4]);
  double*ts=malloc(sizeof(double)*n);
  for(int i=0;i<n;i++){
    int fd=open(p,O_RDWR|O_CREAT|O_TRUNC,0600);
    double t=now();
    if(pre){fstore_t st={F_ALLOCATEALL,F_PEOFPOSMODE,0,mib<<20,0}; if(fcntl(fd,F_PREALLOCATE,&st)!=0){perror("F_PREALLOCATE");return 1;}}
    if(ftruncate(fd,mib<<20)!=0){perror("ftruncate");return 1;}
    ts[i]=now()-t; close(fd); unlink(p);
  }
  qsort(ts,n,sizeof(double),cmp);
  printf("%lld MiB prealloc=%d: n=%d p50=%.0f p90=%.0f p99=%.0f max=%.0f us\n",mib,pre,n,ts[n/2],ts[n*9/10],ts[n*99/100],ts[n-1]);
  return 0;
}
