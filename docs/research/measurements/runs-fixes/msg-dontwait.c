#include <sys/socket.h>
#include <stdio.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <unistd.h>
int main(int c,char**v){int sv[2];socketpair(AF_UNIX,SOCK_STREAM,0,sv);static char buf[1<<20];size_t len=c>1?(size_t)atoi(v[1]):sizeof buf;
int sz=0;socklen_t sl=sizeof sz;getsockopt(sv[0],SOL_SOCKET,SO_SNDBUF,&sz,&sl);printf("sndbuf %d\n",sz);
alarm(2);
ssize_t n=send(sv[0],buf,len,MSG_DONTWAIT);printf("send %zu: %zd errno %d\n",len,n,n<0?errno:0);
n=send(sv[0],buf,len,MSG_DONTWAIT);printf("send2: %zd errno %d\n",n,n<0?errno:0);
struct pollfd p={sv[0],POLLOUT,0};printf("poll %d rev %x\n",poll(&p,1,100),p.revents);
shutdown(sv[1],SHUT_RDWR);p.revents=0;printf("poll after peer shut %d rev %x\n",poll(&p,1,100),p.revents);
close(sv[1]);p.revents=0;printf("poll after peer close %d rev %x\n",poll(&p,1,100),p.revents);return 0;}
