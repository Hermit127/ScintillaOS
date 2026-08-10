#ifndef LWEXT4_FREESTANDING_STDIO_H
#define LWEXT4_FREESTANDING_STDIO_H

typedef struct lwext4_file FILE;

extern FILE *stdin;
extern FILE *stdout;
extern FILE *stderr;

int printf(const char *format, ...);
int fflush(FILE *stream);

#endif
