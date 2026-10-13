#import <AVFoundation/AVFoundation.h>
#import <CoreVideo/CoreVideo.h>
#import <Vision/Vision.h>
#include <string.h>
#include "cam.h"

static void copy_err(char *err, int32_t cap, NSString *msg) {
    if (!err || cap <= 0) return;
    const char *s = msg.UTF8String ?: "unknown error";
    strncpy(err, s, (size_t)cap - 1);
    err[cap - 1] = 0;
}

static NSArray<AVCaptureDevice *> *all_cameras(void) {
    NSArray *types = @[AVCaptureDeviceTypeBuiltInWideAngleCamera, AVCaptureDeviceTypeExternal,
                       AVCaptureDeviceTypeContinuityCamera, AVCaptureDeviceTypeDeskViewCamera];
    AVCaptureDeviceDiscoverySession *s = [AVCaptureDeviceDiscoverySession
        discoverySessionWithDeviceTypes:types mediaType:AVMediaTypeVideo position:AVCaptureDevicePositionUnspecified];
    return s.devices;
}

int32_t bhc_cam_list(char *out, int32_t cap) {
    NSMutableString *s = [NSMutableString string];
    NSArray *devs = all_cameras();
    for (AVCaptureDevice *d in devs) {
        NSString *name = [d.localizedName stringByReplacingOccurrencesOfString:@"\t" withString:@" "];
        name = [name stringByReplacingOccurrencesOfString:@"\n" withString:@" "];
        [s appendFormat:@"%@\t%@\n", d.uniqueID, name];
    }
    if (out && cap > 0) {
        const char *u = s.UTF8String;
        strncpy(out, u, (size_t)cap - 1);
        out[cap - 1] = 0;
    }
    return (int32_t)devs.count;
}

int32_t bhc_cam_permission(void) {
    switch ([AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeVideo]) {
        case AVAuthorizationStatusAuthorized: return 1;
        case AVAuthorizationStatusNotDetermined: return 0;
        default: return 2;
    }
}

int32_t bhc_cam_request_permission(void) {
    if (bhc_cam_permission() != 0) return bhc_cam_permission() == 1;
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    __block BOOL ok = NO;
    [AVCaptureDevice requestAccessForMediaType:AVMediaTypeVideo completionHandler:^(BOOL granted) {
        ok = granted; dispatch_semaphore_signal(done);
    }];
    /* Until answered: timing out while the prompt is still up would report "denied" wrongly. */
    dispatch_semaphore_wait(done, DISPATCH_TIME_FOREVER);
    return ok ? 1 : 0;
}

@interface BhcCamDelegate : NSObject <AVCaptureVideoDataOutputSampleBufferDelegate>
@property (nonatomic) BhcCamFrameFn fn;
@property (nonatomic) void *ctx;
@property (atomic) int32_t segLevel;
@property (atomic) BOOL stopped;
@property (nonatomic, strong) VNGeneratePersonSegmentationRequest *seg;
@property (nonatomic, strong) VNSequenceRequestHandler *handler;
@end

@implementation BhcCamDelegate
- (void)captureOutput:(AVCaptureOutput *)output didOutputSampleBuffer:(CMSampleBufferRef)sb
       fromConnection:(AVCaptureConnection *)connection {
    if (self.stopped) return;
    CVPixelBufferRef px = CMSampleBufferGetImageBuffer(sb);
    if (!px) return;
    CVPixelBufferRef maskBuf = NULL;
    int32_t level = self.segLevel;
    if (level > 0) {
        if (!self.seg) {
            self.seg = [[VNGeneratePersonSegmentationRequest alloc] init];
            self.seg.outputPixelFormat = kCVPixelFormatType_OneComponent8;
            self.handler = [[VNSequenceRequestHandler alloc] init];
        }
        // Fast is plenty for a bubble a few hundred points wide; balanced has cleaner hair edges.
        self.seg.qualityLevel = level >= 2 ? VNGeneratePersonSegmentationRequestQualityLevelBalanced
                                           : VNGeneratePersonSegmentationRequestQualityLevelFast;
        NSError *e = nil;
        if ([self.handler performRequests:@[self.seg] onCVPixelBuffer:px error:&e]) {
            VNPixelBufferObservation *o = self.seg.results.firstObject;
            if (o) maskBuf = o.pixelBuffer;
        }
    }
    CVPixelBufferLockBaseAddress(px, kCVPixelBufferLock_ReadOnly);
    if (maskBuf) CVPixelBufferLockBaseAddress(maskBuf, kCVPixelBufferLock_ReadOnly);
    self.fn(self.ctx,
            CVPixelBufferGetBaseAddress(px), (int32_t)CVPixelBufferGetWidth(px), (int32_t)CVPixelBufferGetHeight(px),
            (int32_t)CVPixelBufferGetBytesPerRow(px),
            maskBuf ? CVPixelBufferGetBaseAddress(maskBuf) : NULL,
            maskBuf ? (int32_t)CVPixelBufferGetWidth(maskBuf) : 0,
            maskBuf ? (int32_t)CVPixelBufferGetHeight(maskBuf) : 0,
            maskBuf ? (int32_t)CVPixelBufferGetBytesPerRow(maskBuf) : 0);
    if (maskBuf) CVPixelBufferUnlockBaseAddress(maskBuf, kCVPixelBufferLock_ReadOnly);
    CVPixelBufferUnlockBaseAddress(px, kCVPixelBufferLock_ReadOnly);
}
@end

struct BhcCamera {
    void *session;   /* AVCaptureSession, retained */
    void *delegate;  /* BhcCamDelegate, retained */
    void *queue;     /* dispatch_queue_t, retained */
};

/* The largest format no taller than max_height (cameras list several; the default is often low). */
static AVCaptureDeviceFormat *best_format(AVCaptureDevice *d, int32_t max_height, int32_t fps) {
    AVCaptureDeviceFormat *best = nil;
    int32_t bestH = 0;
    for (AVCaptureDeviceFormat *f in d.formats) {
        CMVideoDimensions dim = CMVideoFormatDescriptionGetDimensions(f.formatDescription);
        if (dim.height > max_height) continue;
        BOOL rateOk = NO;
        for (AVFrameRateRange *r in f.videoSupportedFrameRateRanges) if (r.maxFrameRate >= fps - 0.5) rateOk = YES;
        if (!rateOk) continue;
        if (dim.height > bestH) { best = f; bestH = dim.height; }
    }
    return best;
}

BhcCamera *bhc_cam_start(const char *unique_id, int32_t max_height, int32_t fps,
                         BhcCamFrameFn fn, void *ctx, char *err, int32_t errcap) {
    @autoreleasepool {
        if (bhc_cam_permission() != 1) { copy_err(err, errcap, @"camera access is not allowed"); return NULL; }
        AVCaptureDevice *dev = nil;
        NSString *want = unique_id && unique_id[0] ? [NSString stringWithUTF8String:unique_id] : nil;
        if (want) dev = [AVCaptureDevice deviceWithUniqueID:want];
        if (!dev) dev = [AVCaptureDevice defaultDeviceWithMediaType:AVMediaTypeVideo];
        if (!dev) dev = all_cameras().firstObject;
        if (!dev) { copy_err(err, errcap, @"no camera found"); return NULL; }

        NSError *e = nil;
        AVCaptureDeviceInput *in = [AVCaptureDeviceInput deviceInputWithDevice:dev error:&e];
        if (!in) { copy_err(err, errcap, e.localizedDescription); return NULL; }
        AVCaptureSession *s = [[AVCaptureSession alloc] init];
        [s beginConfiguration];
        if (![s canAddInput:in]) { copy_err(err, errcap, @"the camera cannot be used (in use by another app?)"); return NULL; }
        [s addInput:in];
        AVCaptureVideoDataOutput *out = [[AVCaptureVideoDataOutput alloc] init];
        out.videoSettings = @{ (id)kCVPixelBufferPixelFormatTypeKey: @(kCVPixelFormatType_32BGRA) };
        out.alwaysDiscardsLateVideoFrames = YES;
        BhcCamDelegate *del = [[BhcCamDelegate alloc] init];
        del.fn = fn; del.ctx = ctx;
        dispatch_queue_t q = dispatch_queue_create("com.biswashost.bhcapture.camera", DISPATCH_QUEUE_SERIAL);
        [out setSampleBufferDelegate:del queue:q];
        if (![s canAddOutput:out]) { copy_err(err, errcap, @"the camera output cannot be added"); return NULL; }
        [s addOutput:out];
        [s commitConfiguration];
        // The format after the session is configured: setting it earlier is overridden by the preset.
        AVCaptureDeviceFormat *f = best_format(dev, max_height > 0 ? max_height : 1080, fps > 0 ? fps : 30);
        if (f && [dev lockForConfiguration:&e]) {
            dev.activeFormat = f;
            /* Both ends: without the max, the camera stretches its exposure in an ordinary room and
             * drops to ~17 fps (measured). A steady rate matters more in a recording; the bubble's
             * Enhance lifts the slightly darker picture. */
            dev.activeVideoMinFrameDuration = CMTimeMake(1, fps > 0 ? fps : 30);
            dev.activeVideoMaxFrameDuration = CMTimeMake(1, fps > 0 ? fps : 30);
            [dev unlockForConfiguration];
        }
        [s startRunning];
        if (!s.isRunning) { copy_err(err, errcap, @"the camera did not start"); return NULL; }

        BhcCamera *c = calloc(1, sizeof(BhcCamera));
        c->session = (__bridge_retained void *)s;
        c->delegate = (__bridge_retained void *)del;
        c->queue = (__bridge_retained void *)q;
        return c;
    }
}

void bhc_cam_set_segmentation(BhcCamera *c, int32_t level) {
    if (!c) return;
    ((__bridge BhcCamDelegate *)c->delegate).segLevel = level;
}

void bhc_cam_stop(BhcCamera *c) {
    if (!c) return;
    @autoreleasepool {
        AVCaptureSession *s = (__bridge_transfer AVCaptureSession *)c->session;
        BhcCamDelegate *del = (__bridge_transfer BhcCamDelegate *)c->delegate;
        dispatch_queue_t q = (__bridge_transfer dispatch_queue_t)c->queue;
        del.stopped = YES;
        [s stopRunning];
        // Drain: no callback may run after this returns (its context is about to be freed).
        dispatch_sync(q, ^{});
        (void)s; (void)del;
    }
    free(c);
}
