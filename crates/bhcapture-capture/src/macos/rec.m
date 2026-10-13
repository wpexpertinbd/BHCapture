#import <Foundation/Foundation.h>
#import <AppKit/AppKit.h>
#import <AVFoundation/AVFoundation.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#import <CoreMedia/CoreMedia.h>
#import <string.h>
#import "sck.h"
#import "rec.h"

static void rec_set_err(char *err, int32_t errcap, NSString *msg) {
    if (!err || errcap <= 0) return;
    const char *c = msg ? [msg UTF8String] : NULL;
    if (!c) c = "unknown error";
    strncpy(err, c, (size_t)errcap - 1);
    err[(size_t)errcap - 1] = '\0';
}

/* Same 5 s rule as sck.m: a missing grant can leave a completion handler un-called forever. */
static BOOL rec_wait(dispatch_semaphore_t s, double secs) {
    return dispatch_semaphore_wait(s, dispatch_time(DISPATCH_TIME_NOW, (int64_t)(secs * NSEC_PER_SEC))) == 0;
}

static CMTime rec_host_now(void) { return CMClockGetTime(CMClockGetHostTimeClock()); }

/* A copy of `sb` with every timestamp moved back by `off` -- how pauses are cut out. */
static CMSampleBufferRef rec_retime_dur(CMSampleBufferRef sb, CMTime off, CMTime dur) {
    CMItemCount n = 0;
    if (CMSampleBufferGetSampleTimingInfoArray(sb, 0, NULL, &n) != noErr || n <= 0) return NULL;
    CMSampleTimingInfo *t = calloc((size_t)n, sizeof(*t));
    if (!t) return NULL;
    CMSampleBufferGetSampleTimingInfoArray(sb, n, t, &n);
    for (CMItemCount i = 0; i < n; i++) {
        t[i].presentationTimeStamp = CMTimeSubtract(t[i].presentationTimeStamp, off);
        if (CMTIME_IS_VALID(t[i].decodeTimeStamp))
            t[i].decodeTimeStamp = CMTimeSubtract(t[i].decodeTimeStamp, off);
        if (CMTIME_IS_VALID(dur)) t[i].duration = dur;
    }
    CMSampleBufferRef out = NULL;
    CMSampleBufferCreateCopyWithNewTiming(kCFAllocatorDefault, sb, n, t, &out);
    free(t);
    return out;
}

static CMSampleBufferRef rec_retime(CMSampleBufferRef sb, CMTime off) { return rec_retime_dur(sb, off, kCMTimeInvalid); }

/* Peak level 0..1 of an audio buffer, for the HUD's meters. Handles float32 and int16 PCM. */
static float rec_peak(CMSampleBufferRef sb) {
    CMAudioFormatDescriptionRef fd = CMSampleBufferGetFormatDescription(sb);
    const AudioStreamBasicDescription *asbd = fd ? CMAudioFormatDescriptionGetStreamBasicDescription(fd) : NULL;
    if (!asbd) return 0;
    size_t need = 0;
    CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(sb, &need, NULL, 0, NULL, NULL, 0, NULL);
    if (need == 0) return 0;
    AudioBufferList *abl = malloc(need);
    if (!abl) return 0;
    CMBlockBufferRef bb = NULL;
    float peak = 0;
    if (CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(sb, NULL, abl, need, NULL, NULL,
            kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment, &bb) == noErr) {
        BOOL isFloat = (asbd->mFormatFlags & kAudioFormatFlagIsFloat) != 0;
        for (UInt32 b = 0; b < abl->mNumberBuffers; b++) {
            const AudioBuffer *ab = &abl->mBuffers[b];
            if (isFloat && asbd->mBitsPerChannel == 32) {
                const float *p = ab->mData; size_t n = ab->mDataByteSize / 4;
                for (size_t i = 0; i < n; i++) { float v = fabsf(p[i]); if (v > peak) peak = v; }
            } else if (!isFloat && asbd->mBitsPerChannel == 16) {
                const int16_t *p = ab->mData; size_t n = ab->mDataByteSize / 2;
                for (size_t i = 0; i < n; i++) { float v = fabsf((float)p[i] / 32768.0f); if (v > peak) peak = v; }
            }
        }
    }
    if (bb) CFRelease(bb);
    free(abl);
    return peak > 1 ? 1 : peak;
}

@interface BHCRec : NSObject <SCStreamOutput, SCStreamDelegate>
@end

@implementation BHCRec {
@public
    SCStream *_stream;
    AVAssetWriter *_writer;
    AVAssetWriterInput *_vin, *_sysIn, *_micIn;
    dispatch_queue_t _q;
    BOOL _started, _stopped, _paused;
    CMTime _first;            /* raw pts of the first frame written */
    CMTime _offset;           /* total paused time so far */
    CMTime _pauseAt;          /* host time the current pause began */
    CMTime _lastV, _lastSys, _lastMic;   /* last ADJUSTED pts appended per track */
    CMSampleBufferRef _lastFrame;        /* retained; re-stamped at stop so the video lasts to the end */
    int64_t _frames;
    int64_t _sysBufs, _micBufs;
    BOOL _denoise;            /* noise filter on the microphone */
    BOOL _level;              /* automatic voice level on the microphone */
    BOOL _eq;                 /* voice EQ on the microphone */
    void *_eqs[2];            /* one voice EQ per mic channel */
    void *_dn[2];             /* one RNNoise state per mic channel */
    void *_lv[2];             /* one auto-level state per mic channel */   /* audio buffers RECEIVED, for diagnosing "no sound" */
    float _lvlSys, _lvlMic;
    NSString *_failure;
    int32_t _w, _h, _fps;
}

- (void)dealloc {
    if (_lastFrame) CFRelease(_lastFrame);
    for (int i = 0; i < 2; i++) { if (_dn[i]) bhc_denoise_free(_dn[i]); if (_lv[i]) bhc_level_free(_lv[i]); if (_eqs[i]) bhc_eq_free(_eqs[i]); }
}

/* One mic channel: noise filter, then voice EQ, then auto level. The level comes last so it sets
 * the loudness of what is actually written, and follows the VOICE, not the fan the filter removed. */
- (void)processChannel:(UInt32)c samples:(float *)p count:(size_t)n rate:(Float64)rate {
    if (_denoise) { if (!_dn[c]) _dn[c] = bhc_denoise_new(); bhc_denoise_process(_dn[c], p, n); }
    if (_eq)      { if (!_eqs[c]) _eqs[c] = bhc_eq_new((uint32_t)rate); bhc_eq_process(_eqs[c], p, n); }
    if (_level)   { if (!_lv[c]) _lv[c] = bhc_level_new((uint32_t)rate); bhc_level_process(_lv[c], p, n); }
}

/* A processed COPY of a microphone buffer (the capture's own buffer is never written to). NULL when
 * the format is one the processing cannot take -- then it switches itself off for this recording
 * and says so, rather than corrupting the sound. On _q. */
- (CMSampleBufferRef)processedMic:(CMSampleBufferRef)sb {
    CMAudioFormatDescriptionRef fd = CMSampleBufferGetFormatDescription(sb);
    const AudioStreamBasicDescription *a = fd ? CMAudioFormatDescriptionGetStreamBasicDescription(fd) : NULL;
    if (!a || !(a->mFormatFlags & kAudioFormatFlagIsFloat) || a->mBitsPerChannel != 32 ||
        a->mChannelsPerFrame < 1 || a->mChannelsPerFrame > 2) {
        fprintf(stderr, "[rec] microphone processing off: format is %.0f Hz, %u ch, %u-bit%s\n",
                a ? a->mSampleRate : 0, a ? a->mChannelsPerFrame : 0, a ? a->mBitsPerChannel : 0,
                (a && (a->mFormatFlags & kAudioFormatFlagIsFloat)) ? " float" : "");
        _denoise = NO; _level = NO; _eq = NO;
        return NULL;
    }
    if (_denoise && (int)a->mSampleRate != 48000) {
        fprintf(stderr, "[rec] noise filter off: the microphone runs at %.0f Hz, the filter needs 48000\n", a->mSampleRate);
        _denoise = NO;
        if (!_level && !_eq) return NULL;
    }
    BOOL planar = (a->mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0;
    UInt32 ch = a->mChannelsPerFrame;
    CMItemCount frames = CMSampleBufferGetNumSamples(sb);
    size_t need = 0;
    CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(sb, &need, NULL, 0, NULL, NULL, 0, NULL);
    AudioBufferList *abl = need ? malloc(need) : NULL;
    if (!abl) return NULL;
    CMBlockBufferRef src = NULL;
    if (CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(sb, NULL, abl, need, NULL, NULL,
            kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment, &src) != noErr) { free(abl); return NULL; }
    size_t total = (size_t)frames * ch * sizeof(float);
    float *data = malloc(total);
    BOOL ok = data != NULL;
    if (ok && planar) {
        for (UInt32 c = 0; c < ch && c < abl->mNumberBuffers; c++) {
            float *dst = data + (size_t)c * frames;
            size_t bytes = MIN(abl->mBuffers[c].mDataByteSize, (UInt32)(frames * sizeof(float)));
            memcpy(dst, abl->mBuffers[c].mData, bytes);
            [self processChannel:c samples:dst count:(size_t)frames rate:a->mSampleRate];
        }
    } else if (ok) {
        /* Interleaved: split, filter each channel, interleave again. */
        const float *in = abl->mBuffers[0].mData;
        float *tmp = malloc((size_t)frames * sizeof(float));
        ok = tmp != NULL && abl->mBuffers[0].mDataByteSize >= total;
        for (UInt32 c = 0; ok && c < ch; c++) {
            for (CMItemCount i = 0; i < frames; i++) tmp[i] = in[i * ch + c];
            [self processChannel:c samples:tmp count:(size_t)frames rate:a->mSampleRate];
            for (CMItemCount i = 0; i < frames; i++) data[i * ch + c] = tmp[i];
        }
        free(tmp);
    }
    if (src) CFRelease(src);
    free(abl);
    if (!ok) { free(data); return NULL; }

    CMBlockBufferRef bb = NULL;
    if (CMBlockBufferCreateWithMemoryBlock(kCFAllocatorDefault, data, total, kCFAllocatorMalloc, NULL, 0, total, 0, &bb) != noErr) {
        free(data); return NULL;
    }
    CMSampleBufferRef out = NULL;
    OSStatus st = CMAudioSampleBufferCreateReadyWithPacketDescriptions(kCFAllocatorDefault, bb, fd, frames,
                      CMSampleBufferGetPresentationTimeStamp(sb), NULL, &out);
    CFRelease(bb);
    return st == noErr ? out : NULL;
}

- (void)failWith:(NSString *)why {          /* on _q */
    if (!_failure) _failure = why ?: @"recording failed";
}

- (void)append:(CMSampleBufferRef)sb to:(AVAssetWriterInput *)in last:(CMTime *)last {   /* on _q */
    CMSampleBufferRef adj = CMTimeCompare(_offset, kCMTimeZero) == 0 ? (CMSampleBufferRef)CFRetain(sb)
                                                                     : rec_retime(sb, _offset);
    if (!adj) return;
    CMTime pts = CMSampleBufferGetPresentationTimeStamp(adj);
    /* Samples captured during a pause can be DELIVERED after resume; after the offset moves they
     * would land before what is already written, which the writer rejects. Drop them. */
    if (CMTIME_IS_VALID(*last) && CMTimeCompare(pts, *last) <= 0) { CFRelease(adj); return; }
    if (in.readyForMoreMediaData) {
        if ([in appendSampleBuffer:adj]) {
            *last = pts;
        } else if (_writer.status == AVAssetWriterStatusFailed) {
            /* Typically the disk filled up. Stop taking samples; stop() finishes what we have. */
            [self failWith:_writer.error.localizedDescription];
        }
    }
    CFRelease(adj);
}

- (void)stream:(SCStream *)stream didOutputSampleBuffer:(CMSampleBufferRef)sb ofType:(SCStreamOutputType)type {
    if (_stopped || _failure || !CMSampleBufferIsValid(sb)) return;

    if (type == SCStreamOutputTypeScreen) {
        CFArrayRef atts = CMSampleBufferGetSampleAttachmentsArray(sb, NO);
        NSDictionary *a = atts && CFArrayGetCount(atts) > 0 ? (__bridge NSDictionary *)CFArrayGetValueAtIndex(atts, 0) : nil;
        /* SCK also sends "idle" buffers with no image when nothing on screen changed. */
        if ([a[SCStreamFrameInfoStatus] integerValue] != SCFrameStatusComplete) return;
        if (!CMSampleBufferGetImageBuffer(sb)) return;
        if (_paused) return;
        if (!_started) {
            _first = CMSampleBufferGetPresentationTimeStamp(sb);
            [_writer startSessionAtSourceTime:_first];
            _started = YES;
        }
        [self append:sb to:_vin last:&_lastV];
        if (_lastFrame) CFRelease(_lastFrame);
        _lastFrame = (CMSampleBufferRef)CFRetain(sb);
        _frames++;
        return;
    }

    /* Audio. Meter it even while paused, so the user can see the mic is live. */
    BOOL isMic = NO;
    if (@available(macOS 15.0, *)) { isMic = (type == SCStreamOutputTypeMicrophone); }
    /* Filter the mic first, so the meter and the file both carry what the filter let through --
     * the meter going quiet when only the fan is running is how the user sees it working. */
    CMSampleBufferRef filtered = (isMic && (_denoise || _level || _eq)) ? [self processedMic:sb] : NULL;
    if (filtered) sb = filtered;
    float pk = rec_peak(sb);
    if (isMic) { _micBufs++; if (pk > _lvlMic) _lvlMic = pk; } else { _sysBufs++; if (pk > _lvlSys) _lvlSys = pk; }
    if (_started && !_paused && CMTimeCompare(CMSampleBufferGetPresentationTimeStamp(sb), _first) >= 0) {
        /* (Audio that predates the first video frame would extend the movie backwards.) */
        if (isMic) { if (_micIn) [self append:sb to:_micIn last:&_lastMic]; }
        else       { if (_sysIn) [self append:sb to:_sysIn last:&_lastSys]; }
    }
    if (filtered) CFRelease(filtered);
}

- (void)stream:(SCStream *)stream didStopWithError:(NSError *)error {
    dispatch_async(_q, ^{ [self failWith:error.localizedDescription ?: @"capture stopped"]; });
}
@end

static SCShareableContent *rec_content(NSError **errOut) {
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    __block SCShareableContent *res = nil; __block NSError *be = nil;
    [SCShareableContent getShareableContentExcludingDesktopWindows:NO onScreenWindowsOnly:YES
                                                 completionHandler:^(SCShareableContent *c, NSError *e) {
        res = c; be = e; dispatch_semaphore_signal(sem);
    }];
    if (!rec_wait(sem, 5)) {
        if (errOut) *errOut = [NSError errorWithDomain:@"com.biswashost.bhcapture" code:-1000
                                              userInfo:@{NSLocalizedDescriptionKey: @"timed out listing displays (5s)"}];
        return nil;
    }
    if (errOut) *errOut = be;
    return res;
}

BhcRecorder *bhc_rec_start(const BhcRecordOptions *opt, int32_t *code, char *err, int32_t errcap) {
    @autoreleasepool {
        *code = BHC_ERR_GENERIC;
        if (!opt || !opt->path) { rec_set_err(err, errcap, @"bad arguments"); return NULL; }
        if (!CGPreflightScreenCaptureAccess()) {
            *code = BHC_ERR_PERMISSION; rec_set_err(err, errcap, @"Screen Recording not granted"); return NULL;
        }
        if (opt->microphone) {
            if (@available(macOS 15.0, *)) {} else {
                *code = BHC_ERR_UNSUPPORTED;
                rec_set_err(err, errcap, @"Recording the microphone needs macOS 15 or later");
                return NULL;
            }
        }
        if (opt->w < 2 || opt->h < 2) { rec_set_err(err, errcap, @"the region is too small to record"); return NULL; }

        NSError *e = nil;
        SCShareableContent *content = rec_content(&e);
        if (!content) {
            *code = (e.code == -3801) ? BHC_ERR_PERMISSION : BHC_ERR_GENERIC;
            rec_set_err(err, errcap, e.localizedDescription ?: @"could not list displays");
            return NULL;
        }
        SCDisplay *disp = nil;
        for (SCDisplay *d in content.displays) if ((uint32_t)d.displayID == opt->display_id) { disp = d; break; }
        if (!disp) { *code = BHC_ERR_NOT_FOUND; rec_set_err(err, errcap, @"display not found"); return NULL; }

        /* Leave OUR windows out (the control bar, the region frame) by excluding this app. */
        NSString *me = NSBundle.mainBundle.bundleIdentifier;
        NSMutableArray<SCRunningApplication *> *skip = [NSMutableArray array];
        for (SCRunningApplication *a in content.applications)
            if (me && [a.bundleIdentifier isEqualToString:me]) [skip addObject:a];
        /* ...except the one window asked for: the overlay that draws click rings, which must be IN
         * the video while the control bar stays out. */
        NSMutableArray<SCWindow *> *keep = [NSMutableArray array];
        if (opt->include_window) {
            for (SCWindow *w in content.windows) if ((uint32_t)w.windowID == opt->include_window) { [keep addObject:w]; break; }
            if (keep.count == 0) fprintf(stderr, "[rec] the click-highlight window %u is not on screen; clicks will not show\n", opt->include_window);
        }
        SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:disp excludingApplications:skip exceptingWindows:keep];

        /* Pixel size: the display's own scale, EVEN (H.264 4:2:0 needs it), long side at most 4096
         * (the H.264 hardware limit we can rely on). */
        CGDirectDisplayID did = disp.displayID;
        CGDisplayModeRef mode = CGDisplayCopyDisplayMode(did);
        double scale = 1.0;
        if (mode) {
            if (disp.width > 0) scale = (double)CGDisplayModeGetPixelWidth(mode) / (double)disp.width;
            CGDisplayModeRelease(mode);
        }
        double pw = opt->w * scale, ph = opt->h * scale;
        double longest = pw > ph ? pw : ph;
        if (longest > 4096.0) { pw *= 4096.0 / longest; ph *= 4096.0 / longest; }
        int32_t W = ((int32_t)pw) & ~1, H = ((int32_t)ph) & ~1;
        if (W < 2) W = 2;
        if (H < 2) H = 2;
        int32_t fps = opt->fps < 1 ? 30 : (opt->fps > 60 ? 60 : opt->fps);

        BHCRec *r = [BHCRec new];
        r->_q = dispatch_queue_create("com.biswashost.bhcapture.rec", DISPATCH_QUEUE_SERIAL);
        r->_offset = kCMTimeZero;
        r->_lastV = r->_lastSys = r->_lastMic = kCMTimeInvalid;
        r->_w = W; r->_h = H; r->_fps = fps;
        r->_denoise = opt->microphone && opt->noise_filter;
        r->_level = opt->microphone && opt->auto_level;
        r->_eq = opt->microphone && opt->voice_clarity;

        NSURL *url = [NSURL fileURLWithPath:[NSString stringWithUTF8String:opt->path]];
        if ([NSFileManager.defaultManager fileExistsAtPath:url.path]) {
            rec_set_err(err, errcap, @"the output file already exists"); return NULL;
        }
        r->_writer = [AVAssetWriter assetWriterWithURL:url fileType:AVFileTypeMPEG4 error:&e];
        if (!r->_writer) { rec_set_err(err, errcap, e.localizedDescription); return NULL; }
        /* Write a playable movie header every 2 s, so a crash or a full disk leaves a file that
         * plays up to that point instead of an unreadable one. */
        r->_writer.movieFragmentInterval = CMTimeMakeWithSeconds(2, 600);

        double bps = (double)W * H * fps * 0.1;
        if (bps < 2e6) bps = 2e6;
        if (bps > 50e6) bps = 50e6;
        NSDictionary *vs = @{
            AVVideoCodecKey: AVVideoCodecTypeH264,
            AVVideoWidthKey: @(W), AVVideoHeightKey: @(H),
            AVVideoCompressionPropertiesKey: @{
                AVVideoAverageBitRateKey: @((int64_t)bps),
                AVVideoExpectedSourceFrameRateKey: @(fps),
                AVVideoMaxKeyFrameIntervalKey: @(fps * 2),
                AVVideoProfileLevelKey: AVVideoProfileLevelH264HighAutoLevel,
                AVVideoAllowFrameReorderingKey: @NO,
            },
            AVVideoColorPropertiesKey: @{
                AVVideoColorPrimariesKey: AVVideoColorPrimaries_ITU_R_709_2,
                AVVideoTransferFunctionKey: AVVideoTransferFunction_ITU_R_709_2,
                AVVideoYCbCrMatrixKey: AVVideoYCbCrMatrix_ITU_R_709_2,
            },
        };
        r->_vin = [AVAssetWriterInput assetWriterInputWithMediaType:AVMediaTypeVideo outputSettings:vs];
        r->_vin.expectsMediaDataInRealTime = YES;
        if (![r->_writer canAddInput:r->_vin]) { rec_set_err(err, errcap, @"the video encoder refused these settings"); return NULL; }
        [r->_writer addInput:r->_vin];

        if (opt->system_audio) {
            r->_sysIn = [AVAssetWriterInput assetWriterInputWithMediaType:AVMediaTypeAudio outputSettings:@{
                AVFormatIDKey: @(kAudioFormatMPEG4AAC), AVSampleRateKey: @48000,
                AVNumberOfChannelsKey: @2, AVEncoderBitRateKey: @160000 }];
            r->_sysIn.expectsMediaDataInRealTime = YES;
            if ([r->_writer canAddInput:r->_sysIn]) [r->_writer addInput:r->_sysIn]; else r->_sysIn = nil;
        }
        if (opt->microphone) {
            r->_micIn = [AVAssetWriterInput assetWriterInputWithMediaType:AVMediaTypeAudio outputSettings:@{
                AVFormatIDKey: @(kAudioFormatMPEG4AAC), AVSampleRateKey: @48000,
                AVNumberOfChannelsKey: @1, AVEncoderBitRateKey: @96000 }];
            r->_micIn.expectsMediaDataInRealTime = YES;
            if ([r->_writer canAddInput:r->_micIn]) [r->_writer addInput:r->_micIn]; else r->_micIn = nil;
        }
        if (![r->_writer startWriting]) {
            rec_set_err(err, errcap, r->_writer.error.localizedDescription ?: @"could not start writing");
            return NULL;
        }

        SCStreamConfiguration *cfg = [SCStreamConfiguration new];
        cfg.width = (size_t)W; cfg.height = (size_t)H;
        cfg.sourceRect = CGRectMake(opt->x, opt->y, opt->w, opt->h);
        cfg.minimumFrameInterval = CMTimeMake(1, fps);
        cfg.pixelFormat = '420v';
        cfg.colorMatrix = kCGDisplayStreamYCbCrMatrix_ITU_R_709_2;
        cfg.colorSpaceName = kCGColorSpaceSRGB;
        cfg.showsCursor = opt->show_cursor ? YES : NO;
        cfg.queueDepth = 8;
        cfg.capturesAudio = opt->system_audio ? YES : NO;
        cfg.sampleRate = 48000;
        cfg.channelCount = 2;
        cfg.excludesCurrentProcessAudio = YES;
        if (opt->microphone) { if (@available(macOS 15.0, *)) { cfg.captureMicrophone = YES; } }

        r->_stream = [[SCStream alloc] initWithFilter:filter configuration:cfg delegate:r];
        if (![r->_stream addStreamOutput:r type:SCStreamOutputTypeScreen sampleHandlerQueue:r->_q error:&e]) {
            [r->_writer cancelWriting]; rec_set_err(err, errcap, e.localizedDescription); return NULL;
        }
        if (opt->system_audio && ![r->_stream addStreamOutput:r type:SCStreamOutputTypeAudio sampleHandlerQueue:r->_q error:&e])
            fprintf(stderr, "[rec] could not add the system audio output: %s\n", e.localizedDescription.UTF8String);
        if (opt->microphone) {
            if (@available(macOS 15.0, *)) {
                [r->_stream addStreamOutput:r type:SCStreamOutputTypeMicrophone sampleHandlerQueue:r->_q error:nil];
            }
        }

        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        __block NSError *startErr = nil;
        [r->_stream startCaptureWithCompletionHandler:^(NSError *se) { startErr = se; dispatch_semaphore_signal(sem); }];
        if (!rec_wait(sem, 5) || startErr) {
            [r->_writer cancelWriting];
            [NSFileManager.defaultManager removeItemAtURL:url error:nil];
            *code = (startErr.code == -3801) ? BHC_ERR_PERMISSION : BHC_ERR_GENERIC;
            rec_set_err(err, errcap, startErr ? startErr.localizedDescription : @"timed out starting the recording (5s)");
            return NULL;
        }
        *code = BHC_OK;
        return (__bridge_retained BhcRecorder *)r;
    }
}

void bhc_rec_set_paused(BhcRecorder *h, int32_t paused) {
    if (!h) return;
    BHCRec *r = (__bridge BHCRec *)h;
    dispatch_sync(r->_q, ^{
        if (paused && !r->_paused) {
            r->_paused = YES;
            r->_pauseAt = rec_host_now();
        } else if (!paused && r->_paused) {
            r->_offset = CMTimeAdd(r->_offset, CMTimeSubtract(rec_host_now(), r->_pauseAt));
            r->_paused = NO;
        }
    });
}

void bhc_rec_status(BhcRecorder *h, BhcRecordStatus *out, char *err, int32_t errcap) {
    memset(out, 0, sizeof(*out));
    if (!h) return;
    BHCRec *r = (__bridge BHCRec *)h;
    __block NSString *fail = nil;
    dispatch_sync(r->_q, ^{
        out->state = r->_failure ? BHC_REC_FAILED : (r->_paused ? BHC_REC_PAUSED : BHC_REC_RECORDING);
        if (r->_started) {
            CMTime end = r->_paused ? r->_pauseAt : rec_host_now();
            out->seconds = CMTimeGetSeconds(CMTimeSubtract(CMTimeSubtract(end, r->_offset), r->_first));
            if (out->seconds < 0) out->seconds = 0;
        }
        out->frames = r->_frames;
        out->width = r->_w; out->height = r->_h;
        out->level_system = r->_lvlSys; out->level_mic = r->_lvlMic;
        r->_lvlSys = 0; r->_lvlMic = 0;
        fail = r->_failure;
    });
    if (fail) rec_set_err(err, errcap, fail);
}

int32_t bhc_rec_stop(BhcRecorder *h, char *err, int32_t errcap) {
    if (!h) { rec_set_err(err, errcap, @"no recording"); return BHC_ERR_GENERIC; }
    @autoreleasepool {
        BHCRec *r = (__bridge_transfer BHCRec *)h;

        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        [r->_stream stopCaptureWithCompletionHandler:^(NSError *e) { (void)e; dispatch_semaphore_signal(sem); }];
        rec_wait(sem, 5);   /* already stopped by an error is fine */

        __block int32_t rc = BHC_OK;
        __block NSString *msg = nil;
        dispatch_sync(r->_q, ^{
            BOOL wasPaused = r->_paused;
            CMTime endHost = wasPaused ? r->_pauseAt : rec_host_now();
            r->_stopped = YES;
            fprintf(stderr, "[rec] audio buffers received: system %lld, microphone %lld (inputs: system %s, mic %s); video frames %lld\n",
                    r->_sysBufs, r->_micBufs, r->_sysIn ? "yes" : "no", r->_micIn ? "yes" : "no", r->_frames);
            if (!r->_started) {
                [r->_writer cancelWriting];
                [NSFileManager.defaultManager removeItemAtURL:r->_writer.outputURL error:nil];
                rc = BHC_ERR_GENERIC;
                msg = r->_failure ?: @"No video was captured";
                return;
            }
            /* SCK sends nothing while the screen is still, so stamp the last frame again at the
             * moment Stop was pressed. Without it a recording of a still screen would be one frame
             * long and its sound would run past the end of the picture. */
            if (r->_lastFrame && !r->_failure && r->_writer.status == AVAssetWriterStatusWriting) {
                CMTime rawLast = CMSampleBufferGetPresentationTimeStamp(r->_lastFrame);
                CMTime shift = CMTimeSubtract(rawLast, endHost);   /* negative: moves the frame forward to Stop */
                /* An explicit one-frame duration: left unset, the muxer reuses the previous gap, which on a
                 * still screen is the whole recording and doubles the file's length. */
                CMSampleBufferRef tail = rec_retime_dur(r->_lastFrame, CMTimeAdd(r->_offset, shift), CMTimeMake(1, r->_fps));
                if (tail) {
                    CMTime p = CMSampleBufferGetPresentationTimeStamp(tail);
                    if ((!CMTIME_IS_VALID(r->_lastV) || CMTimeCompare(p, r->_lastV) > 0) && r->_vin.readyForMoreMediaData)
                        [r->_vin appendSampleBuffer:tail];
                    CFRelease(tail);
                }
            }
        });
        if (rc != BHC_OK) { rec_set_err(err, errcap, msg); return rc; }

        if (r->_writer.status == AVAssetWriterStatusWriting) {
            [r->_vin markAsFinished];
            [r->_sysIn markAsFinished];
            [r->_micIn markAsFinished];
            dispatch_semaphore_t done = dispatch_semaphore_create(0);
            [r->_writer finishWritingWithCompletionHandler:^{ dispatch_semaphore_signal(done); }];
            if (!rec_wait(done, 30)) { rec_set_err(err, errcap, @"timed out finishing the file (30s)"); return BHC_ERR_GENERIC; }
        }
        if (r->_writer.status != AVAssetWriterStatusCompleted) {
            rec_set_err(err, errcap, r->_failure ?: (r->_writer.error.localizedDescription ?: @"the file could not be finished"));
            return BHC_ERR_GENERIC;
        }
        if (r->_failure) { rec_set_err(err, errcap, r->_failure); return BHC_ERR_GENERIC; }
        return BHC_OK;
    }
}

int32_t bhc_rec_mic_supported(void) {
    if (@available(macOS 15.0, *)) return 1;
    return 0;
}

/* Mix every audio track of `src` into ONE stereo AAC track, copying the video untouched, into `dst`.
 *
 * Why: with system audio and the microphone both on, the recording has two audio tracks. QuickTime
 * plays both, but browsers and most upload sites play only the FIRST -- the microphone would be lost
 * there without a word. AVAssetReaderAudioMixOutput does the mixing (sample-accurate, resampled);
 * the video is passed through, so this costs about one file copy. */
static BOOL rec_wait_load(AVAsset *asset, NSString *mediaType, NSArray<AVAssetTrack *> **out) {
    dispatch_semaphore_t s = dispatch_semaphore_create(0);
    __block NSArray<AVAssetTrack *> *res = nil;
    [asset loadTracksWithMediaType:mediaType completionHandler:^(NSArray<AVAssetTrack *> *t, NSError *e) {
        (void)e; res = t; dispatch_semaphore_signal(s);
    }];
    if (!rec_wait(s, 10)) return NO;
    *out = res;
    return YES;
}

int32_t bhc_rec_mix_audio(const char *src_path, const char *dst_path, char *err, int32_t errcap) {
    @autoreleasepool {
        NSURL *src = [NSURL fileURLWithPath:[NSString stringWithUTF8String:src_path]];
        NSURL *dst = [NSURL fileURLWithPath:[NSString stringWithUTF8String:dst_path]];
        [NSFileManager.defaultManager removeItemAtURL:dst error:nil];
        AVURLAsset *asset = [AVURLAsset URLAssetWithURL:src options:nil];
        NSArray<AVAssetTrack *> *vt = nil, *at = nil;
        if (!rec_wait_load(asset, AVMediaTypeVideo, &vt) || !rec_wait_load(asset, AVMediaTypeAudio, &at)) {
            rec_set_err(err, errcap, @"timed out reading the recording"); return BHC_ERR_GENERIC;
        }
        if (vt.count != 1 || at.count < 2) { rec_set_err(err, errcap, @"nothing to mix"); return BHC_ERR_GENERIC; }

        NSError *e = nil;
        AVAssetReader *reader = [AVAssetReader assetReaderWithAsset:asset error:&e];
        if (!reader) { rec_set_err(err, errcap, e.localizedDescription); return BHC_ERR_GENERIC; }
        AVAssetReaderTrackOutput *vout = [AVAssetReaderTrackOutput assetReaderTrackOutputWithTrack:vt[0] outputSettings:nil];
        NSDictionary *pcm = @{ AVFormatIDKey: @(kAudioFormatLinearPCM), AVSampleRateKey: @48000,
                               AVNumberOfChannelsKey: @2, AVLinearPCMBitDepthKey: @32, AVLinearPCMIsFloatKey: @YES,
                               AVLinearPCMIsNonInterleaved: @NO, AVLinearPCMIsBigEndianKey: @NO };
        AVAssetReaderAudioMixOutput *aout = [AVAssetReaderAudioMixOutput assetReaderAudioMixOutputWithAudioTracks:at audioSettings:pcm];
        if (![reader canAddOutput:vout] || ![reader canAddOutput:aout]) { rec_set_err(err, errcap, @"cannot read the recording"); return BHC_ERR_GENERIC; }
        [reader addOutput:vout]; [reader addOutput:aout];

        AVAssetWriter *writer = [AVAssetWriter assetWriterWithURL:dst fileType:AVFileTypeMPEG4 error:&e];
        if (!writer) { rec_set_err(err, errcap, e.localizedDescription); return BHC_ERR_GENERIC; }
        CMFormatDescriptionRef vfd = (__bridge CMFormatDescriptionRef)vt[0].formatDescriptions.firstObject;
        AVAssetWriterInput *vin = [AVAssetWriterInput assetWriterInputWithMediaType:AVMediaTypeVideo outputSettings:nil sourceFormatHint:vfd];
        AVAssetWriterInput *ain = [AVAssetWriterInput assetWriterInputWithMediaType:AVMediaTypeAudio outputSettings:@{
            AVFormatIDKey: @(kAudioFormatMPEG4AAC), AVSampleRateKey: @48000, AVNumberOfChannelsKey: @2, AVEncoderBitRateKey: @160000 }];
        if (![writer canAddInput:vin] || ![writer canAddInput:ain]) { rec_set_err(err, errcap, @"cannot write the mixed file"); return BHC_ERR_GENERIC; }
        [writer addInput:vin]; [writer addInput:ain];

        if (![reader startReading]) { rec_set_err(err, errcap, reader.error.localizedDescription ?: @"cannot read"); return BHC_ERR_GENERIC; }
        if (![writer startWriting]) { [reader cancelReading]; rec_set_err(err, errcap, writer.error.localizedDescription ?: @"cannot write"); return BHC_ERR_GENERIC; }
        [writer startSessionAtSourceTime:kCMTimeZero];

        dispatch_group_t g = dispatch_group_create();
        __block BOOL failed = NO;
        NSArray *pairs = @[ @[vin, vout], @[ain, aout] ];
        for (NSArray *pr in pairs) {
            AVAssetWriterInput *in = pr[0]; AVAssetReaderOutput *out = pr[1];
            dispatch_group_enter(g);
            dispatch_queue_t q = dispatch_queue_create("com.biswashost.bhcapture.mix", DISPATCH_QUEUE_SERIAL);
            __block BOOL done = NO;
            [in requestMediaDataWhenReadyOnQueue:q usingBlock:^{
                while (!done && in.readyForMoreMediaData) {
                    CMSampleBufferRef sb = [out copyNextSampleBuffer];
                    if (!sb) { done = YES; [in markAsFinished]; dispatch_group_leave(g); return; }
                    BOOL ok = [in appendSampleBuffer:sb];
                    CFRelease(sb);
                    if (!ok) { failed = YES; done = YES; [in markAsFinished]; dispatch_group_leave(g); return; }
                }
            }];
        }
        /* Generous: the video is only copied, so even an hour-long recording takes seconds. */
        if (dispatch_group_wait(g, dispatch_time(DISPATCH_TIME_NOW, (int64_t)600 * NSEC_PER_SEC)) != 0) {
            [reader cancelReading]; [writer cancelWriting];
            rec_set_err(err, errcap, @"timed out mixing the audio"); return BHC_ERR_GENERIC;
        }
        if (failed || reader.status == AVAssetReaderStatusFailed) {
            [reader cancelReading]; [writer cancelWriting];
            rec_set_err(err, errcap, writer.error.localizedDescription ?: reader.error.localizedDescription ?: @"mixing failed");
            return BHC_ERR_GENERIC;
        }
        dispatch_semaphore_t fin = dispatch_semaphore_create(0);
        [writer finishWritingWithCompletionHandler:^{ dispatch_semaphore_signal(fin); }];
        if (!rec_wait(fin, 60) || writer.status != AVAssetWriterStatusCompleted) {
            rec_set_err(err, errcap, writer.error.localizedDescription ?: @"could not finish the mixed file");
            return BHC_ERR_GENERIC;
        }
        return BHC_OK;
    }
}

static id gClickMonitor = nil;
static void (*gClickCb)(double, double, int32_t, void *) = NULL;
static void *gClickCtx = NULL;

void bhc_watch_clicks(void (*cb)(double, double, int32_t, void *), void *ctx) {
    bhc_unwatch_clicks();
    gClickCb = cb; gClickCtx = ctx;
    gClickMonitor = [NSEvent addGlobalMonitorForEventsMatchingMask:(NSEventMaskLeftMouseDown | NSEventMaskRightMouseDown)
                                                            handler:^(NSEvent *e) {
        /* mouseLocation is bottom-left based on the PRIMARY screen; the rest of BHCapture uses
         * top-left global points, whose origin is that same screen's top-left. */
        NSPoint p = [NSEvent mouseLocation];
        CGFloat h = NSScreen.screens.count ? NSScreen.screens[0].frame.size.height : 0;
        if (gClickCb) gClickCb(p.x, h - p.y, e.type == NSEventTypeRightMouseDown ? 1 : 0, gClickCtx);
    }];
}

void bhc_unwatch_clicks(void) {
    if (gClickMonitor) { [NSEvent removeMonitor:gClickMonitor]; gClickMonitor = nil; }
    gClickCb = NULL; gClickCtx = NULL;
}
