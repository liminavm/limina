#include <stdio.h>
#include <pthread.h>
#include <unistd.h>
#include <stdint.h>
#include <sys/qos.h>
#include <mach/mach_time.h>

static volatile uint64_t iters = 0;
static volatile int stop = 0;

static void *body(void *arg) {
    pthread_set_qos_class_self_np(QOS_CLASS_BACKGROUND, 0);
    volatile double x = 1.0;
    while (!stop) { for (int i = 0; i < 100000; i++) x = x * 1.000001 + 0.5; iters++; }
    return NULL;
}

static uint64_t sample(const char *label) {
    uint64_t a = iters; usleep(1500000); uint64_t b = iters;
    printf("%-34s %6llu units/1.5s\n", label, (unsigned long long)(b - a));
    return b - a;
}

int main(void) {
    pthread_t w; pthread_create(&w, NULL, body, NULL);
    usleep(300000);

    uint64_t bg = sample("BACKGROUND (self-set):");
    pthread_override_t ov = pthread_override_qos_class_start_np(w, QOS_CLASS_USER_INITIATED, 0);
    uint64_t ovr = sample("+ external override USER_INIT:");
    pthread_override_qos_class_end_np(ov);
    uint64_t back = sample("after override ended:");

    printf("\noverride effect: %.2fx   (restored to %.2fx of baseline)\n",
           bg ? (double)ovr / bg : 0.0, bg ? (double)back / bg : 0.0);
    stop = 1; pthread_join(w, NULL);
    return 0;
}
