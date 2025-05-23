#ifndef SYSROOT_SYS_MSG_H_
#define SYSROOT_SYS_MSG_H_

#ifdef __cplusplus
extern "C" {
#endif

#include <sys/ipc.h>

#define __NEED_pid_t
#define __NEED_key_t
#define __NEED_time_t
#define __NEED_size_t
#define __NEED_ssize_t

#include <bits/alltypes.h>

typedef unsigned long msgqnum_t;
typedef unsigned long msglen_t;

#define __msg_cbytes msg_cbytes

#define MSG_NOERROR 010000
#define MSG_EXCEPT 020000

#define MSG_STAT 11
#define MSG_INFO 12

struct msginfo {
  int msgpool, msgmap, msgmax, msgmnb, msgmni, msgssz, msgtql;
  unsigned short msgseg;
};

int msgctl(int, int, void*);
int msgget(key_t, int);
ssize_t msgrcv(int, void*, size_t, long, int);
int msgsnd(int, const void*, size_t, int);

#if defined(_GNU_SOURCE) || defined(_BSD_SOURCE)
struct msgbuf {
  long mtype;
  char mtext[1];
};
#endif

#ifdef __cplusplus
}
#endif

#endif  // SYSROOT_SYS_MSG_H_
