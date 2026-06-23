#ifndef PG_WASI_SYS_WAIT_H
#define PG_WASI_SYS_WAIT_H
/* wasi has no process model; postgres src/port/exec.c includes <sys/wait.h>
   defensively but the wait() path is never reached by frontend libpq. Minimal
   stub so it compiles. */
#include <sys/types.h>
#ifndef WIFEXITED
#define WIFEXITED(s)   (((s) & 0x7f) == 0)
#define WEXITSTATUS(s) (((s) >> 8) & 0xff)
#define WIFSIGNALED(s) (((signed char)(((s) & 0x7f) + 1) >> 1) > 0)
#define WTERMSIG(s)    ((s) & 0x7f)
#endif
pid_t waitpid(pid_t, int *, int);
#endif
