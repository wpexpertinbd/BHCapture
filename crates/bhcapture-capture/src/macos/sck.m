#import <Foundation/Foundation.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <AppKit/AppKit.h>
#import <string.h>
#import <objc/runtime.h>
#import <objc/message.h>
#import <objc/objc-exception.h>
#import "sck.h"

/* Every async entry point blocks on a semaphore WITH A TIMEOUT. Without it a missing TCC grant
 * can leave the completion handler un-called forever, which presents as a frozen app rather
 * than an error -- the silent failure mode Phase 0a was built to find. */
static const int64_t kBhcTimeoutNs = 5LL * NSEC_PER_SEC;

/* SCStreamErrorUserDeclined. Matching on the code, not on the message text, because the
 * message is localised and not API. */
static const NSInteger kSCUserDeclined = -3801;

static void bhc_set_err(char *err, int32_t errcap, NSString *msg) {
    if (!err || errcap <= 0) return;
    const char *c = msg ? [msg UTF8String] : NULL;
    if (!c) c = "unknown error";
    strncpy(err, c, (size_t)errcap - 1);
    err[(size_t)errcap - 1] = '\0';
}

static void bhc_copy_str(char *dst, size_t cap, NSString *s) {
    dst[0] = '\0';
    if (!s) return;
    const char *c = [s UTF8String];
    if (!c) return;
    strncpy(dst, c, cap - 1);
    dst[cap - 1] = '\0';
}

static int32_t bhc_classify(NSError *e) {
    if (e && e.code == kSCUserDeclined) return BHC_ERR_PERMISSION;
    return BHC_ERR_GENERIC;
}

int32_t bhc_preflight_permission(void) { return CGPreflightScreenCaptureAccess() ? 1 : 0; }
int32_t bhc_request_permission(void)   { return CGRequestScreenCaptureAccess()  ? 1 : 0; }

static SCShareableContent *bhc_fetch_content(NSError **errOut) {
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    __block SCShareableContent *result = nil;
    __block NSError *blockErr = nil;

    [SCShareableContent getShareableContentExcludingDesktopWindows:YES
                                               onScreenWindowsOnly:NO
                                                 completionHandler:^(SCShareableContent *c, NSError *e) {
        result = c; blockErr = e;
        dispatch_semaphore_signal(sem);
    }];

    if (dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, kBhcTimeoutNs)) != 0) {
        if (errOut) {
            *errOut = [NSError errorWithDomain:@"com.biswashost.bhcapture" code:-1000 userInfo:@{
                NSLocalizedDescriptionKey: @"timed out waiting for SCShareableContent (5s)"}];
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

static double bhc_display_scale(SCDisplay *d) {
    int32_t pw = 0, ph = 0;
    bhc_physical_size(d.displayID, &pw, &ph);
    (void)ph;
    double pts = (double)d.width;
    if (pts <= 0.0 || pw <= 0) return 1.0;
    return (double)pw / pts;
}

/* CGImage -> owned, tightly-strided BGRA8.
 *
 * FAST PATH: copy the pixels straight out of the image's data provider. Drawing the image into a
 * bitmap context instead (the original approach) leaked ~20 MB per capture on macOS 27 -- Core
 * Graphics kept a copy per draw even after the image was released. Measured: 12 draws grew a process
 * from 9 MB to 254 MB; with the draw skipped it stayed flat at 36 MB. A selector session takes one
 * frame per display, so the leak was ~18 MB every time the hotkey was pressed. The fallback below
 * is kept for any pixel format the fast path does not recognise.
 */
static int32_t bhc_cgimage_to_bgra(CGImageRef img, double scale, BhcImage *out, char *err, int32_t errcap) {
    size_t w = CGImageGetWidth(img), h = CGImageGetHeight(img);
    if (w == 0 || h == 0) { bhc_set_err(err, errcap, @"captured image has zero size"); return BHC_ERR_GENERIC; }

    size_t stride = w * 4;
    uint8_t *buf = (uint8_t *)calloc(h, stride);
    if (!buf) { bhc_set_err(err, errcap, @"out of memory"); return BHC_ERR_GENERIC; }

    BOOL copied = NO;
    CGBitmapInfo bi = CGImageGetBitmapInfo(img);
    if (CGImageGetBitsPerPixel(img) == 32 && CGImageGetBitsPerComponent(img) == 8 &&
        (bi & kCGBitmapByteOrderMask) == kCGBitmapByteOrder32Little &&
        (bi & kCGBitmapAlphaInfoMask) == kCGImageAlphaPremultipliedFirst) {
        CGDataProviderRef dp = CGImageGetDataProvider(img);
        CFDataRef data = dp ? CGDataProviderCopyData(dp) : NULL;
        if (data) {
            size_t bpr = CGImageGetBytesPerRow(img);
            const uint8_t *src = CFDataGetBytePtr(data);
            /* Rows may be padded (bpr > w*4); and never read past what the provider actually gave. */
            if (bpr >= stride && (size_t)CFDataGetLength(data) >= bpr * (h - 1) + stride) {
                for (size_t y = 0; y < h; y++) memcpy(buf + y * stride, src + y * bpr, stride);
                copied = YES;
            }
            CFRelease(data);
        }
    }

    if (!copied) {
        CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
        /* PremultipliedFirst + 32Little == B,G,R,A byte order in memory. */
        CGContextRef ctx = CGBitmapContextCreate(buf, w, h, 8, stride, cs,
            kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little);
        CGColorSpaceRelease(cs);
        if (!ctx) { free(buf); bhc_set_err(err, errcap, @"CGBitmapContextCreate failed"); return BHC_ERR_GENERIC; }
        CGContextDrawImage(ctx, CGRectMake(0, 0, (CGFloat)w, (CGFloat)h), img);
        CGContextRelease(ctx);
    }

    out->data = buf; out->width = (int32_t)w; out->height = (int32_t)h;
    out->stride = (int32_t)stride; out->scale = scale;
    return BHC_OK;
}

/* Shared capture path. Caller supplies the filter and the pixel dimensions it wants.
 * No @available guard: the deployment target is 14.0, so SCScreenshotManager is always present
 * and LSMinimumSystemVersion stops the app launching anywhere it is not. */
static int32_t bhc_shoot(SCContentFilter *filter, size_t px_w, size_t px_h, double scale,
                         int32_t show_cursor, BhcImage *out, char *err, int32_t errcap) {
    SCStreamConfiguration *cfg = [[SCStreamConfiguration alloc] init];
    cfg.width = px_w;
    cfg.height = px_h;
    cfg.pixelFormat = 'BGRA';
    /* Deliver sRGB. The raw pixel copy below skips Core Graphics' colour conversion, so the values
     * must already be in the space the saved PNG is assumed to be in. Without this a wide-gamut
     * (Display P3) screen would come out slightly wrong. */
    cfg.colorSpaceName = kCGColorSpaceSRGB;
    cfg.showsCursor = show_cursor ? YES : NO;
    cfg.capturesAudio = NO;

    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    __block CGImageRef img = NULL;
    __block NSError *shotErr = nil;
    /* If we give up waiting, a completion that arrives LATER must release what it would have
     * retained -- otherwise each slow capture leaks a whole-screen image (Codex review). The lock
     * makes "abandon" and "store the image" mutually exclusive. */
    __block BOOL abandoned = NO;
    NSObject *lock = [NSObject new];

    [SCScreenshotManager captureImageWithFilter:filter configuration:cfg
                              completionHandler:^(CGImageRef sampled, NSError *e) {
        @synchronized (lock) {
            if (!abandoned) {
                if (sampled) img = CGImageRetain(sampled);   /* must outlive the block */
                shotErr = e;
            }
        }
        dispatch_semaphore_signal(sem);
    }];

    if (dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, kBhcTimeoutNs)) != 0) {
        @synchronized (lock) {
            abandoned = YES;
            /* The completion may have stored an image between the timeout and this lock. */
            if (img) { CGImageRelease(img); img = NULL; }
        }
        bhc_set_err(err, errcap, @"timed out waiting for SCScreenshotManager (5s)");
        return BHC_ERR_GENERIC;
    }
    if (!img) {
        bhc_set_err(err, errcap, shotErr ? shotErr.localizedDescription : @"captureImage returned no image");
        return bhc_classify(shotErr);
    }

    int32_t rc = bhc_cgimage_to_bgra(img, scale, out, err, errcap);
    CGImageRelease(img);
    return rc;
}

int32_t bhc_list_displays(BhcDisplayInfo *out, int32_t cap, int32_t *n_out, char *err, int32_t errcap) {
    @autoreleasepool {
        *n_out = 0;
        if (!CGPreflightScreenCaptureAccess()) {
            bhc_set_err(err, errcap, @"Screen Recording not granted");
            return BHC_ERR_PERMISSION;
        }
        NSError *e = nil;
        SCShareableContent *content = bhc_fetch_content(&e);
        if (!content) {
            bhc_set_err(err, errcap, e ? e.localizedDescription : @"SCShareableContent returned nil");
            return bhc_classify(e);
        }
        int32_t n = 0;
        for (SCDisplay *d in content.displays) {
            if (n >= cap) break;
            CGRect f = d.frame;
            out[n].display_id = (uint32_t)d.displayID;
            out[n].x = (int32_t)f.origin.x;    out[n].y = (int32_t)f.origin.y;
            out[n].pt_width = (int32_t)f.size.width;
            out[n].pt_height = (int32_t)f.size.height;
            bhc_physical_size(d.displayID, &out[n].px_width, &out[n].px_height);
            n++;
        }
        *n_out = n;
        return BHC_OK;
    }
}

/* Front-to-back order and layer, from CGWindowListCopyWindowInfo.
 *
 * SCShareableContent documents NO ordering, so picking "the first window containing the point"
 * from it can highlight a window that is actually hidden behind another. CGWindowList documents
 * its result as front-to-back, which is what snap-to-window needs. Layer and order are not
 * permission-gated (only window NAMES are), so this works even before a grant. */
static void bhc_window_order(NSMutableDictionary<NSNumber *, NSNumber *> *z,
                             NSMutableDictionary<NSNumber *, NSNumber *> *layer) {
    CFArrayRef list = CGWindowListCopyWindowInfo(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements, kCGNullWindowID);
    if (!list) return;
    NSArray *arr = CFBridgingRelease(list);
    NSInteger i = 0;
    for (NSDictionary *d in arr) {
        NSNumber *num = d[(id)kCGWindowNumber];
        NSNumber *lyr = d[(id)kCGWindowLayer];
        if (num) { z[num] = @(i); if (lyr) layer[num] = lyr; }
        i++;
    }
}

int32_t bhc_list_windows(BhcWindowInfo *out, int32_t cap, int32_t *n_out, char *err, int32_t errcap) {
    @autoreleasepool {
        *n_out = 0;
        if (!CGPreflightScreenCaptureAccess()) {
            bhc_set_err(err, errcap, @"Screen Recording not granted");
            return BHC_ERR_PERMISSION;
        }
        NSError *e = nil;
        SCShareableContent *content = bhc_fetch_content(&e);
        if (!content) {
            bhc_set_err(err, errcap, e ? e.localizedDescription : @"SCShareableContent returned nil");
            return bhc_classify(e);
        }
        NSMutableDictionary<NSNumber *, NSNumber *> *zmap = [NSMutableDictionary dictionary];
        NSMutableDictionary<NSNumber *, NSNumber *> *lmap = [NSMutableDictionary dictionary];
        bhc_window_order(zmap, lmap);

        int32_t n = 0;
        for (SCWindow *w in content.windows) {
            if (n >= cap) break;
            CGRect f = w.frame;
            NSNumber *key = @(w.windowID);
            out[n].z_order = zmap[key] ? (int32_t)[zmap[key] integerValue] : -1;
            out[n].layer = lmap[key] ? (int32_t)[lmap[key] integerValue] : (int32_t)w.windowLayer;
            out[n].window_id = (uint64_t)w.windowID;
            out[n].x = (int32_t)f.origin.x;   out[n].y = (int32_t)f.origin.y;
            out[n].pt_width = (int32_t)f.size.width;
            out[n].pt_height = (int32_t)f.size.height;
            out[n].on_screen = w.isOnScreen ? 1 : 0;
            bhc_copy_str(out[n].title, BHC_TITLE_CAP, w.title);
            bhc_copy_str(out[n].app, BHC_APP_CAP, w.owningApplication.applicationName);
            n++;
        }
        *n_out = n;
        return BHC_OK;
    }
}

int32_t bhc_capture_display(uint32_t display_id, const uint32_t *exclude, int32_t exclude_n,
                            int32_t show_cursor, BhcImage *out, char *err, int32_t errcap) {
    @autoreleasepool {
        if (!out) { bhc_set_err(err, errcap, @"null out param"); return BHC_ERR_GENERIC; }
        memset(out, 0, sizeof(*out));
        if (!CGPreflightScreenCaptureAccess()) {
            bhc_set_err(err, errcap, @"Screen Recording not granted");
            return BHC_ERR_PERMISSION;
        }
        NSError *e = nil;
        SCShareableContent *content = bhc_fetch_content(&e);
        if (!content) {
            bhc_set_err(err, errcap, e ? e.localizedDescription : @"SCShareableContent returned nil");
            return bhc_classify(e);
        }
        {
            SCDisplay *target = nil;
            for (SCDisplay *d in content.displays) {
                if ((uint32_t)d.displayID == display_id) { target = d; break; }
            }
            if (!target) { bhc_set_err(err, errcap, @"display not found"); return BHC_ERR_NOT_FOUND; }

            int32_t pw = 0, ph = 0;
            bhc_physical_size(target.displayID, &pw, &ph);
            if (pw <= 0 || ph <= 0) { pw = (int32_t)target.width; ph = (int32_t)target.height; }

            NSMutableArray<SCWindow *> *skip = [NSMutableArray array];
            for (int32_t i = 0; i < exclude_n; i++) {
                for (SCWindow *w in content.windows) {
                    if ((uint32_t)w.windowID == exclude[i]) { [skip addObject:w]; break; }
                }
            }
            SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:target excludingWindows:skip];
            return bhc_shoot(filter, (size_t)pw, (size_t)ph, bhc_display_scale(target), show_cursor, out, err, errcap);
        }
    }
}

int32_t bhc_capture_window(uint64_t window_id, BhcImage *out, char *err, int32_t errcap) {
    @autoreleasepool {
        if (!out) { bhc_set_err(err, errcap, @"null out param"); return BHC_ERR_GENERIC; }
        memset(out, 0, sizeof(*out));
        if (!CGPreflightScreenCaptureAccess()) {
            bhc_set_err(err, errcap, @"Screen Recording not granted");
            return BHC_ERR_PERMISSION;
        }
        NSError *e = nil;
        SCShareableContent *content = bhc_fetch_content(&e);
        if (!content) {
            bhc_set_err(err, errcap, e ? e.localizedDescription : @"SCShareableContent returned nil");
            return bhc_classify(e);
        }
        {
            SCWindow *target = nil;
            for (SCWindow *w in content.windows) {
                if ((uint64_t)w.windowID == window_id) { target = w; break; }
            }
            if (!target) { bhc_set_err(err, errcap, @"window not found"); return BHC_ERR_NOT_FOUND; }

            /* A window must be captured at ITS OWN display's scale, otherwise dragging a window
             * to a 1x monitor silently yields a half-resolution capture. Pick the display whose
             * frame contains the window centre; fall back to the largest scale present. */
            CGRect wf = target.frame;
            CGPoint centre = CGPointMake(CGRectGetMidX(wf), CGRectGetMidY(wf));
            double scale = 0.0;
            for (SCDisplay *d in content.displays) {
                if (CGRectContainsPoint(d.frame, centre)) { scale = bhc_display_scale(d); break; }
            }
            if (scale <= 0.0) {
                for (SCDisplay *d in content.displays) {
                    double s = bhc_display_scale(d);
                    if (s > scale) scale = s;
                }
            }
            if (scale <= 0.0) scale = 1.0;

            size_t pw = (size_t)llround(wf.size.width  * scale);
            size_t ph = (size_t)llround(wf.size.height * scale);
            if (pw == 0 || ph == 0) { bhc_set_err(err, errcap, @"window has zero size"); return BHC_ERR_GENERIC; }

            SCContentFilter *filter = [[SCContentFilter alloc] initWithDesktopIndependentWindow:target];
            return bhc_shoot(filter, pw, ph, scale, 0, out, err, errcap);
        }
    }
}

void bhc_image_free(BhcImage *img) {
    if (img && img->data) { free(img->data); img->data = NULL; }
}

uint32_t bhc_cgwindow_id(void *nswindow) {
    if (!nswindow) return 0;
    NSWindow *w = (__bridge NSWindow *)nswindow;
    NSInteger n = [w windowNumber];
    return n > 0 ? (uint32_t)n : 0;
}

static void (*gDisplayCb)(void *) = NULL;
static void *gDisplayCtx = NULL;

static void bhc_display_reconfig(CGDirectDisplayID display, CGDisplayChangeSummaryFlags flags, void *userInfo) {
    (void)display; (void)userInfo;
    /* Every change arrives first as a BEGIN call (the new state is not in place yet) and then as a
     * second call per display once it is. Acting on BEGIN would read the OLD layout. */
    if (flags & kCGDisplayBeginConfigurationFlag) return;
    if (gDisplayCb) gDisplayCb(gDisplayCtx);
}

void bhc_watch_displays(void (*cb)(void *), void *ctx) {
    static dispatch_once_t once;
    gDisplayCb = cb;
    gDisplayCtx = ctx;
    dispatch_once(&once, ^{
        CGDisplayRegisterReconfigurationCallback(bhc_display_reconfig, NULL);
    });
}

/* Diagnostics: log each Objective-C exception before it is thrown (see log_objc_exceptions). */
static objc_exception_preprocessor bhc_prev_preprocessor;
static id bhc_exception_logger(id exc) {
    if ([exc isKindOfClass:[NSException class]]) {
        NSException *e = exc;
        fprintf(stderr, "[objc-exception] %s: %s\n%s\n", e.name.UTF8String ?: "?", e.reason.UTF8String ?: "?",
                [[e.callStackSymbols componentsJoinedByString:@"\n"] UTF8String] ?: "");
        fflush(stderr);
    }
    return bhc_prev_preprocessor ? bhc_prev_preprocessor(exc) : exc;
}
void bhc_log_objc_exceptions(void) {
    bhc_prev_preprocessor = objc_setExceptionPreprocessor(bhc_exception_logger);
}

/* The class bhc_make_overlay_panel replaced, per window (a Class is never freed: ASSIGN is safe). */
static char bhc_original_class_key;

@interface BHCOverlayPanel : NSPanel
@end
@implementation BHCOverlayPanel
/* A borderless window cannot become key by default, and a panel only if asked. */
- (BOOL)canBecomeKeyWindow { return YES; }
/* Never the MAIN window: that would make it look like the app's document window. */
- (BOOL)canBecomeMainWindow { return NO; }
@end

/* ⚠️ CONTRACT: the swap REPLACES Tao's own `TaoWindow` subclass (and its `focusable` ivar and
 * overrides) with this sibling NSPanel subclass. Everything BHCapture does with an overlay is fine
 * with that, but NEVER call Tauri's `set_focusable` (or anything else that reaches Tao's window
 * ivars) on an overlay window: Tao would touch an ivar the object no longer has. (Codex review.) */
int32_t bhc_make_overlay_panel(void *nswindow) {
    if (!nswindow) return 0;
    NSWindow *w = (__bridge NSWindow *)nswindow;
    if (![w isKindOfClass:[NSWindow class]]) return 0;

    Class cur = object_getClass(w);
    Class want = [BHCOverlayPanel class];
    if (cur == want) return 1;   /* already converted */

    /* Changing an object's class in place is only safe if the new class needs no more memory than
     * the object was allocated with, otherwise AppKit writes past the end of the allocation.
     * NSWindow and NSPanel are both 528 bytes on macOS 27, but check the real object every time
     * rather than trust that a future OS keeps it so. */
    if (class_getInstanceSize(want) > class_getInstanceSize(cur)) return -1;

    /* Remember the class we replace, so bhc_restore_window_class can put it back before the window
     * is destroyed. That class is usually not Tao's own but the KVO subclass macOS made for WebKit's
     * observers; tearing the window down under OUR class makes WebKit's removeObserver throw
     * ("…is not registered as an observer") and Rust aborts. (1.1.5 crash, 2026-10-25.) */
    objc_setAssociatedObject(w, &bhc_original_class_key, cur, OBJC_ASSOCIATION_ASSIGN);
    object_setClass(w, want);
    NSPanel *p = (NSPanel *)w;
    p.styleMask |= NSWindowStyleMaskNonactivatingPanel;
    p.floatingPanel = YES;
    /* Panels default to hiding when their app is inactive -- which is ALWAYS, for this app. */
    p.hidesOnDeactivate = NO;
    p.becomesKeyOnlyIfNeeded = NO;

    /* Converting an existing window skips NSPanel's own initialisation, so changing the style mask
     * does NOT update the window server's "prevents activation" flag, and a real mouse click would
     * still activate this app. The private selector below syncs it. This is the same step the
     * widely used tauri-nspanel crate takes (found by reading its source). It is private API, so
     * it is guarded: if a future macOS drops the selector we lose only that nicety. */
    SEL prevents = NSSelectorFromString(@"_setPreventsActivation:");
    if ([p respondsToSelector:prevents]) {
        ((void (*)(id, SEL, BOOL))objc_msgSend)(p, prevents, YES);
    }
    return 1;
}

/* Undo bhc_make_overlay_panel's class swap. MUST run before such a window is destroyed: see the
 * note at the swap. Returns 1 if restored, 0 if there was nothing to restore. Main thread only. */
int32_t bhc_restore_window_class(void *nswindow) {
    if (!nswindow) return 0;
    NSWindow *w = (__bridge NSWindow *)nswindow;
    if (object_getClass(w) != [BHCOverlayPanel class]) return 0;
    Class orig = objc_getAssociatedObject(w, &bhc_original_class_key);
    if (!orig) return 0;
    object_setClass(w, orig);
    objc_setAssociatedObject(w, &bhc_original_class_key, nil, OBJC_ASSOCIATION_ASSIGN);
    return 1;
}

/* macOS bug: an app that switches from accessory to regular WHILE RUNNING gets a menu bar that
 * draws but does not open -- clicks on File/Edit do nothing -- until the app is deactivated and
 * activated again. Reproduced on macOS 27 (Benjamin: "none of the menu items doing anything").
 * The standard remedy is to hand activation to the Dock and immediately take it back, which is
 * the deactivate/activate cycle the menu bar needs. Main thread only. */
void bhc_refresh_menu_bar(void) {
    NSRunningApplication *dock =
        [NSRunningApplication runningApplicationsWithBundleIdentifier:@"com.apple.dock"].firstObject;
    if (!dock) { [NSApp activateIgnoringOtherApps:YES]; return; }
    [dock activateWithOptions:0];
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 80 * NSEC_PER_MSEC), dispatch_get_main_queue(), ^{
        [NSApp activateIgnoringOtherApps:YES];
        [[NSRunningApplication currentApplication] activateWithOptions:NSApplicationActivateAllWindows];
    });
}
