// A Regular-policy AppKit app that posix_spawns its arguments and quits when the child exits: the
// shape of limina's supervisor before the worker moved to a launchd job. Launched with `open -n`, so
// LaunchServices (not our shell) is its parent and it gets an app coalition of its own.
//
//   open -n LpqApp.app --args <stdout-file> <program> [args...]
#import <AppKit/AppKit.h>
#include <fcntl.h>
#include <spawn.h>
#include <sys/wait.h>

extern char **environ;

int main(int argc, char **argv) {
    @autoreleasepool {
        if (argc < 3) return 2;
        [NSApplication sharedApplication];
        [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular];
        posix_spawn_file_actions_t fa;
        posix_spawn_file_actions_init(&fa);
        posix_spawn_file_actions_addopen(&fa, 1, argv[1], O_WRONLY | O_CREAT | O_APPEND, 0644);
        posix_spawn_file_actions_adddup2(&fa, 1, 2);
        pid_t pid;
        if (posix_spawn(&pid, argv[2], &fa, NULL, &argv[2], environ)) return 1;
        dispatch_async(dispatch_get_global_queue(QOS_CLASS_DEFAULT, 0), ^{
            int st;
            waitpid(pid, &st, 0);
            dispatch_async(dispatch_get_main_queue(), ^{ [NSApp terminate:nil]; });
        });
        [NSApp run];
    }
    return 0;
}
