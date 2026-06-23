#ifndef PG_WASI_TERMIOS_H
#define PG_WASI_TERMIOS_H
/* wasi has no terminal control; postgres src/port/sprompt.c (password echo
   toggling) uses termios but is never reached by the postgres_scanner. Minimal
   stub so it compiles + links (stubs in pg-wasi/stubs.c). */
#include <sys/types.h>
typedef unsigned int  tcflag_t;
typedef unsigned char cc_t;
typedef unsigned int  speed_t;
#define NCCS 32
struct termios {
    tcflag_t c_iflag, c_oflag, c_cflag, c_lflag;
    cc_t     c_cc[NCCS];
    speed_t  c_ispeed, c_ospeed;
};
#define ECHO      0000010
#define ICANON    0000002
#define TCSANOW   0
#define TCSADRAIN 1
#define TCSAFLUSH 2
int tcgetattr(int, struct termios *);
int tcsetattr(int, int, const struct termios *);
#endif
