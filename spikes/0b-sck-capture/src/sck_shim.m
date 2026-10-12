#import <Foundation/Foundation.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <sys/resource.h>
#import <string.h>
#import "sck_shim.h"

/* ScreenCaptureKit is entirely completion-handler based. For a spike we want a
 * blocking call, so each async entry point is wrapped in a dispatch semaphore
 * with a TIMEOUT -- without the timeout a missing TCC grant can hang forever
 * instead of reporting, which is exactly the silent-failure mode we are hunting. */
static const int64_t kBhcTimeoutNs = 5LL * NSEC_PER_SEC;

static void bhc_set_err(char *err, int32_t errcap, NSString *msg) {
    if (!err || errcap <= 0) return;
    const char *c = msg ? [msg UTF8String] : NULL;
    if (!c) c = "unknown error";
    strncpy(err, c, (size_t)errcap - 1);
    err[(size_t)errcap - 1] = '\0';
}

int32_t bhc_preflight_permission(void) {
    return CGPreflightScreenCaptureAccess() ? 1 : 0;
}

int32_t bhc_request_permission(void) {
    return CGRequestScreenCaptureAccess() ? 1 : 0;
}

/* Blocking fetch of shareable content. Returns nil and sets *errOut on failure. */
static SCShareableContent *bhc_fetch_content(NSError **errOut) {
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    __block SCShareableContent *result = nil;
    __block NSError *blockErr = nil;

    [SCShareableContent getShareableContentExcludingDesktopWindows:YES
                                               onScreenWindowsOnly:YES
                                                 completionHandler:^(SCShareableContent *c, NSError *e) {
        result = c;
        blockErr = e;
        dispatch_semaphore_signal(sem);
    }];

    if (dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, kBhcTimeoutNs)) != 0) {
        if (errOut) {
            *errOut = [NSError errorWithDomain:@"com.biswashost.bhcapture"
                                          code:-1000
                                      userInfo:@{NSLocalizedDescriptionKey:
                              @"timed out waiting for SCShareableContent (5s) -- usually a missing Screen Recording grant"}];
        }
        return nil;
    }
    if (errOut) *errOut = blockErr;
    return result;
}

static void bhc_physical_size(CGDirectDisplayID did, int32_t *pw, int32_t *ph) {
    *pw = 0; *ph = 0;
    CGDisplayModeRef mode = CGDisplayCopyDisplayMode(did);
    if (mode) {
        *pw = (int32_t)CGDisplayModeGetPixelWidth(mode);
        *ph = (int32_t)CGDisplayModeGetPixelHeight(mode);
        CGDisplayModeRelease(mode);
    }
}

int32_t bhc_list_displays(BhcDisplayInfo *out, int32_t cap, char *err, int32_t errcap) {
    @autoreleasepool {
        NSError *e = nil;
        SCShareableContent *content = bhc_fetch_content(&e);
        if (!content) {
            bhc_set_err(err, errcap, e ? e.localizedDescription : @"SCShareableContent returned nil");
            return -1;
        }
        int32_t n = 0;
        for (SCDisplay *d in content.displays) {
            if (n >= cap) break;
            CGRect f = d.frame;
            out[n].display_id = (uint32_t)d.displayID;
            out[n].x         = (int32_t)f.origin.x;
            out[n].y         = (int32_t)f.origin.y;
            out[n].pt_width  = (int32_t)f.size.width;
            out[n].pt_height = (int32_t)f.size.height;
            bhc_physical_size(d.displayID, &out[n].px_width, &out[n].px_height);
            n++;
        }
        return n;
    }
}

int32_t bhc_capture_display(uint32_t display_id, BhcImage *out, char *err, int32_t errcap) {
    @autoreleasepool {
        if (!out) { bhc_set_err(err, errcap, @"null out param"); return -1; }
        memset(out, 0, sizeof(*out));

        /* The whole SCScreenshotManager path lives inside the @available block so the
         * compiler can verify it -- an early `if (!@available) return` does NOT satisfy
         * -Wunguarded-availability-new, and this app really does target 12.3. */
        if (@available(macOS 14.0, *)) {
            NSError *e = nil;
            SCShareableContent *content = bhc_fetch_content(&e);
            if (!content) {
                bhc_set_err(err, errcap, e ? e.localizedDescription : @"SCShareableContent returned nil");
                return -1;
            }

            SCDisplay *target = nil;
            for (SCDisplay *d in content.displays) {
                if ((uint32_t)d.displayID == display_id) { target = d; break; }
            }
            if (!target) { bhc_set_err(err, errcap, @"display id not found in shareable content"); return -1; }

            int32_t pw = 0, ph = 0;
            bhc_physical_size(target.displayID, &pw, &ph);
            if (pw <= 0 || ph <= 0) { pw = (int32_t)target.width; ph = (int32_t)target.height; }

            SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:target excludingWindows:@[]];
            SCStreamConfiguration *cfg = [[SCStreamConfiguration alloc] init];
            cfg.width  = (size_t)pw;          /* full physical resolution, not points */
            cfg.height = (size_t)ph;
            cfg.pixelFormat = 'BGRA';         /* kCVPixelFormatType_32BGRA */
            cfg.showsCursor = NO;
            cfg.capturesAudio = NO;           /* 13.0+, satisfied inside this block */

            dispatch_semaphore_t sem = dispatch_semaphore_create(0);
            __block CGImageRef img = NULL;
            __block NSError *shotErr = nil;

            [SCScreenshotManager captureImageWithFilter:filter
                                         configuration:cfg
                                     completionHandler:^(CGImageRef sampled, NSError *se) {
                if (sampled) img = CGImageRetain(sampled);   /* must outlive the block */
                shotErr = se;
                dispatch_semaphore_signal(sem);
            }];

            if (dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, kBhcTimeoutNs)) != 0) {
                bhc_set_err(err, errcap, @"timed out waiting for SCScreenshotManager (5s)");
                return -1;
            }
            if (!img) {
                bhc_set_err(err, errcap, shotErr ? shotErr.localizedDescription : @"captureImage returned no image");
                return -1;
            }

            size_t w = CGImageGetWidth(img), h = CGImageGetHeight(img);
            size_t stride = w * 4;
            uint8_t *buf = (uint8_t *)calloc(h, stride);
            if (!buf) { CGImageRelease(img); bhc_set_err(err, errcap, @"out of memory"); return -1; }

            CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
            /* PremultipliedFirst + 32Little == B,G,R,A byte order in memory. */
            CGContextRef ctx = CGBitmapContextCreate(buf, w, h, 8, stride, cs,
                kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little);
            CGColorSpaceRelease(cs);
            if (!ctx) {
                free(buf); CGImageRelease(img);
                bhc_set_err(err, errcap, @"CGBitmapContextCreate failed");
                return -1;
            }
            CGContextDrawImage(ctx, CGRectMake(0, 0, (CGFloat)w, (CGFloat)h), img);
            CGContextRelease(ctx);
            CGImageRelease(img);

            out->data   = buf;
            out->width  = (int32_t)w;
            out->height = (int32_t)h;
            out->stride = (int32_t)stride;
            return 0;
        } else {
            bhc_set_err(err, errcap,
                @"this spike needs macOS 14+ (SCScreenshotManager); the real app adds an SCStream fallback for 12.3-13");
            return -1;
        }
    }
}

void bhc_image_free(BhcImage *img) {
    if (img && img->data) { free(img->data); img->data = NULL; }
}

uint64_t bhc_peak_rss_bytes(void) {
    struct rusage ru;
    if (getrusage(RUSAGE_SELF, &ru) != 0) return 0;
    return (uint64_t)ru.ru_maxrss;   /* macOS reports BYTES here, unlike Linux */
}
