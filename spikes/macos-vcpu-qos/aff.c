#include <stdio.h>
#include <pthread.h>
#include <mach/mach.h>
#include <mach/thread_policy.h>

int main(void) {
    thread_port_t t = pthread_mach_thread_np(pthread_self());

    thread_affinity_policy_data_t aff = { .affinity_tag = 1 };
    kern_return_t kr = thread_policy_set(t, THREAD_AFFINITY_POLICY,
                                         (thread_policy_t)&aff,
                                         THREAD_AFFINITY_POLICY_COUNT);
    printf("THREAD_AFFINITY_POLICY  -> kr=%d (%s)\n", kr,
           kr == KERN_SUCCESS ? "SUCCESS" :
           kr == KERN_NOT_SUPPORTED ? "KERN_NOT_SUPPORTED" : "other");

    // Read it back: even a "successful" set is meaningless if nothing remembers it.
    thread_affinity_policy_data_t got = {0};
    mach_msg_type_number_t cnt = THREAD_AFFINITY_POLICY_COUNT;
    boolean_t def = FALSE;
    kr = thread_policy_get(t, THREAD_AFFINITY_POLICY, (thread_policy_t)&got, &cnt, &def);
    printf("  read back            -> kr=%d tag=%d default=%d\n", kr, got.affinity_tag, def);

    return 0;
}
