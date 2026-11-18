// Folds a foldable iOS simulator (iPhone Duo) to a hinge position, run
// inside it with `simctl spawn`: `fold <0.0-1.0>` (0 closed, 1 open flat),
// or `fold --serve`, which stays and folds to each position it reads, one
// a line (the daemon keeps one running per device: starting a program in
// the simulator takes over a second, the fold itself under half of one).
// Compiled by the daemon with this Mac's Xcode (`devices/ios.rs`,
// `fold_tool`), with the entitlement below linked into its
// `__TEXT,__entitlements` section, as Xcode does for simulator builds.
//
// SpringBoard's display tool service (`SBSDisplayToolService`, in
// SpringBoardServices) sweeps the hinge from where it is to the position
// asked for, as a hand would, and swaps between the cover and inner
// screens where the sweep crosses its swap boundary. It serves only
// clients with the `com.apple.springboard.sbdisplay.service` entitlement,
// and drops a request made while a fold is still going on.
//
// `-swapDisplayWithSettings:completion:` takes the settings by value, a
// struct encoded `{?=qdqqBqdd}`; what each field is was read from what
// SpringBoard logs of a request ("swap request … (duration=… orientation=…
// profileOverride=… wait=…)", "absoluteSweeping (… position=…)") and from
// the names its client gives them (`duration`, `deviceOrientation`,
// `profileOverride`, `waitForCompletion`, `targetMode`,
// `targetHingePosition`). The method's encoding is checked first, so
// another layout fails here rather than sending nonsense.
//
// Exit status: 0 done, 2 bad arguments, 3 no such service (not a
// foldable's runtime, or another layout), 4 SpringBoard didn't answer.

#import <Foundation/Foundation.h>
#import <dlfcn.h>
#import <objc/runtime.h>

typedef struct {
    long reserved;               // 0
    double duration;             // seconds; -1: the median hand's
    long deviceOrientation;      // 0: as it is now
    long profileOverride;        // 0: SpringBoard's own (1 fade, 2 continuous)
    BOOL waitForCompletion;      // answer once the fold is over
    long targetMode;             // 1: to `targetHingePosition` (0: to the other screen)
    double targetHingePosition;  // 0.0 closed … 1.0 open flat
    double unused;
} SwapSettings;

static const char *const ENCODING = "{?=qdqqBqdd}";

/// One fold to `position`: 0 done, 3 no service, 4 SpringBoard didn't finish.
static int fold(id service, Class service_class, double position) {
    SEL swap = NSSelectorFromString(@"swapDisplayWithSettings:completion:");
    SwapSettings settings = {
        .reserved = 0,
        .duration = -1.0,
        .deviceOrientation = 0,
        .profileOverride = 0,
        .waitForCompletion = YES,
        .targetMode = 1,
        .targetHingePosition = position,
        .unused = 0.0,
    };
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    void (*send)(id, SEL, SwapSettings, void (^)(void)) = (void *)dlsym(RTLD_DEFAULT, "objc_msgSend");
    send(service, swap, settings, ^{
        dispatch_semaphore_signal(done);
    });
    long late = dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 20 * NSEC_PER_SEC));
    return late ? 4 : 0;
}

/// A position from 0.0 to 1.0, or -1 when `text` isn't one.
static double position_of(const char *text) {
    char *end = NULL;
    double position = strtod(text, &end);
    while (end && (*end == '\n' || *end == '\r' || *end == ' ')) end++;
    if (end == text || (end && *end != '\0') || position < 0.0 || position > 1.0) {
        return -1;
    }
    return position;
}

int main(int argc, char **argv) {
    @autoreleasepool {
        BOOL serve = argc == 2 && strcmp(argv[1], "--serve") == 0;
        double position = !serve && argc == 2 ? position_of(argv[1]) : -1;
        if (!serve && position < 0) {
            fprintf(stderr, "usage: fold <0.0-1.0> | fold --serve\n");
            return 2;
        }
        dlopen("/System/Library/PrivateFrameworks/SpringBoardServices.framework/SpringBoardServices", RTLD_NOW);
        Class service_class = NSClassFromString(@"SBSDisplayToolService");
        SEL swap = NSSelectorFromString(@"swapDisplayWithSettings:completion:");
        Method method = service_class ? class_getInstanceMethod(service_class, swap) : NULL;
        const char *types = method ? method_getTypeEncoding(method) : NULL;
        if (!types || !strstr(types, ENCODING)) {
            fprintf(stderr, "This simulator has no display tool service to fold it with (%s).\n", types ? types : "none");
            if (serve) {
                printf("error 3\n");
                fflush(stdout);
            }
            return 3;
        }
        id service = [service_class new];
        int status = 0;
        if (serve) {
            // One position a line; each answered "done" or "error <status>"
            // once the fold is over. Ends with its input.
            printf("ready\n");
            fflush(stdout);
            char line[64];
            while (fgets(line, sizeof line, stdin)) {
                double at = position_of(line);
                int folded = at < 0 ? 2 : fold(service, service_class, at);
                if (folded == 0) {
                    printf("done\n");
                } else {
                    printf("error %d\n", folded);
                }
                fflush(stdout);
            }
        } else {
            status = fold(service, service_class, position);
            if (status == 4) {
                fprintf(stderr, "SpringBoard didn't finish folding.\n");
            }
        }
        [service performSelector:NSSelectorFromString(@"invalidate")];
        return status;
    }
}
