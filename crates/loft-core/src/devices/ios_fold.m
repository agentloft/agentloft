// Folds a foldable iOS simulator (iPhone Duo) to a hinge position, run
// inside it with `simctl spawn`: `fold <0.0-1.0>` (0 closed, 1 open flat).
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

int main(int argc, char **argv) {
    @autoreleasepool {
        char *end = NULL;
        double position = argc == 2 ? strtod(argv[1], &end) : -1;
        if (argc != 2 || end == argv[1] || *end != '\0' || position < 0.0 || position > 1.0) {
            fprintf(stderr, "usage: fold <0.0-1.0>\n");
            return 2;
        }
        dlopen("/System/Library/PrivateFrameworks/SpringBoardServices.framework/SpringBoardServices", RTLD_NOW);
        Class service_class = NSClassFromString(@"SBSDisplayToolService");
        SEL swap = NSSelectorFromString(@"swapDisplayWithSettings:completion:");
        Method method = service_class ? class_getInstanceMethod(service_class, swap) : NULL;
        const char *types = method ? method_getTypeEncoding(method) : NULL;
        if (!types || !strstr(types, ENCODING)) {
            fprintf(stderr, "This simulator has no display tool service to fold it with (%s).\n", types ? types : "none");
            return 3;
        }
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
        id service = [service_class new];
        dispatch_semaphore_t done = dispatch_semaphore_create(0);
        void (*send)(id, SEL, SwapSettings, void (^)(void)) = (void *)dlsym(RTLD_DEFAULT, "objc_msgSend");
        send(service, swap, settings, ^{
            dispatch_semaphore_signal(done);
        });
        long late = dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 20 * NSEC_PER_SEC));
        [service performSelector:NSSelectorFromString(@"invalidate")];
        if (late) {
            fprintf(stderr, "SpringBoard didn't finish folding.\n");
            return 4;
        }
        return 0;
    }
}
