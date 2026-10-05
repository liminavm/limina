/*
 * zink: lost wakeup in the multi-context wait for another context's unflushed
 * batch (zink_batch_usage_unflushed_wait).
 *
 * Two shared GLES contexts on two threads. Thread A writes a buffer on the GPU
 * (glCopyBufferSubData) and flushes; thread B maps the same buffer for reading
 * while A's batch may still be unflushed, so B sleeps on A's batch usage
 * condition variable. The waiter checks `unflushed` without the mutex and does
 * not re-check it before cnd_wait, and the submit thread clears it and
 * broadcasts without the mutex: a flush that completes in between leaves B
 * asleep forever.
 *
 * Build: cc -O2 -o zink-unflushed-wait zink-unflushed-wait.c -lEGL -lGLESv2 -lpthread
 * Run:   MESA_LOADER_DRIVER_OVERRIDE=zink ./zink-unflushed-wait [seconds, default 60]
 *        (a race: occasional in a plain run, frequent under gdb; see README)
 * Unfixed: "HANG after N iterations ..." and exit 3 once B stops making
 *          progress for 10 s. With HANG_TRAP=1 it raises SIGTRAP instead,
 *          for a debugger.
 * Fixed:   "ok: N iterations in S s", exit 0.
 */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

#define SIZE (64 * 1024)

static EGLDisplay dpy;
static EGLContext ctx_a, ctx_b;
static GLuint src_buf, dst_buf;
static atomic_uint turn_a, turn_b, done_b;   /* handshake counters */
static atomic_ulong progress;
static atomic_bool stop;
static atomic_uint threads_done;

static void die(const char *m) { fprintf(stderr, "%s\n", m); exit(2); }

static double now(void)
{
   struct timespec ts;
   clock_gettime(CLOCK_MONOTONIC, &ts);
   return ts.tv_sec + ts.tv_nsec / 1e9;
}

/* A: record a GPU write into dst_buf, let B start mapping, then flush */
static void *thread_a(void *arg)
{
   (void)arg;
   if (!eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx_a)) die("A: makecurrent");
   glBindBuffer(GL_COPY_READ_BUFFER, src_buf);
   glBindBuffer(GL_COPY_WRITE_BUFFER, dst_buf);
   for (unsigned i = 1; !atomic_load(&stop); i++) {
      /* B must have unmapped the buffer before it can be written again */
      while (atomic_load(&done_b) != i - 1 && !atomic_load(&stop))
         sched_yield();
      /* recorded into A's batch, not flushed yet */
      glCopyBufferSubData(GL_COPY_READ_BUFFER, GL_COPY_WRITE_BUFFER, 0, 0, SIZE);
      atomic_store(&turn_b, i);
      while (atomic_load(&turn_a) != i && !atomic_load(&stop))
         sched_yield();
      glFlush();
   }
   glFinish();
   atomic_fetch_add(&threads_done, 1);
   return NULL;
}

/* B: map dst_buf for reading, which must wait for A's batch to be flushed */
static void *thread_b(void *arg)
{
   (void)arg;
   if (!eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx_b)) die("B: makecurrent");
   glBindBuffer(GL_COPY_WRITE_BUFFER, dst_buf);
   for (unsigned i = 1; !atomic_load(&stop); i++) {
      while (atomic_load(&turn_b) != i && !atomic_load(&stop))
         sched_yield();
      atomic_store(&turn_a, i);
      void *p = glMapBufferRange(GL_COPY_WRITE_BUFFER, 0, SIZE, GL_MAP_READ_BIT);
      if (!p) die("B: map failed");
      glUnmapBuffer(GL_COPY_WRITE_BUFFER);
      atomic_store(&done_b, i);
      atomic_fetch_add(&progress, 1);
   }
   atomic_fetch_add(&threads_done, 1);
   return NULL;
}

int main(int argc, char **argv)
{
   double seconds = argc > 1 ? atof(argv[1]) : 60;
   /* glthread would defer B's unmap past the handshake, and A's copy would
    * then fail with "writeBuffer is mapped" */
   setenv("mesa_glthread", "false", 1);

   PFNEGLGETPLATFORMDISPLAYEXTPROC get_display =
      (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
   dpy = get_display(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
   if (!eglInitialize(dpy, NULL, NULL)) die("eglInitialize failed");
   eglBindAPI(EGL_OPENGL_ES_API);
   static const EGLint attr[] = { EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE };
   ctx_a = eglCreateContext(dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, attr);
   ctx_b = eglCreateContext(dpy, EGL_NO_CONFIG_KHR, ctx_a, attr);
   if (ctx_a == EGL_NO_CONTEXT || ctx_b == EGL_NO_CONTEXT) die("context failed");

   eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx_a);
   printf("GL_RENDERER: %s\n", glGetString(GL_RENDERER));
   static unsigned char data[SIZE];
   glGenBuffers(1, &src_buf);
   glGenBuffers(1, &dst_buf);
   glBindBuffer(GL_COPY_READ_BUFFER, src_buf);
   glBufferData(GL_COPY_READ_BUFFER, SIZE, data, GL_STATIC_DRAW);
   glBindBuffer(GL_COPY_READ_BUFFER, dst_buf);
   glBufferData(GL_COPY_READ_BUFFER, SIZE, data, GL_DYNAMIC_READ);
   glFinish();
   eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);

   pthread_t a, b;
   pthread_create(&a, NULL, thread_a, NULL);
   pthread_create(&b, NULL, thread_b, NULL);

   double start = now(), last_change = start;
   unsigned long last = 0;
   while (atomic_load(&threads_done) < 2) {
      usleep(100 * 1000);
      unsigned long p = atomic_load(&progress);
      double t = now();
      if (p != last) {
         last = p;
         last_change = t;
      } else if (t - last_change > 10) {
         printf("HANG after %lu iterations (%.1f s), no progress for 10 s\n", p, t - start);
         fflush(stdout);
         if (getenv("HANG_TRAP"))
            raise(SIGTRAP); /* stop here when running under a debugger */
         _exit(3);
      }
      if (t - start > seconds)
         atomic_store(&stop, true);
   }
   pthread_join(a, NULL);
   pthread_join(b, NULL);
   printf("ok: %lu iterations in %.1f s\n", last, now() - start);
   return 0;
}
