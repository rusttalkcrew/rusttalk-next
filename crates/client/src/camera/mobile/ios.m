#import <AVFoundation/AVFoundation.h>
#import <UIKit/UIKit.h>
#import <stdbool.h>

extern void rt_camera_bgra(const uint8_t *, uint32_t, uint32_t, uint32_t);

int rt_camera_permission(bool request) {
    switch ([AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeVideo]) {
        case AVAuthorizationStatusAuthorized:
            return UIApplication.sharedApplication.applicationState == UIApplicationStateActive ? 1 : 0;
        case AVAuthorizationStatusNotDetermined:
            if (request) [AVCaptureDevice requestAccessForMediaType:AVMediaTypeVideo
                completionHandler:^(BOOL granted) {}];
            return 0;
        default: return -1;
    }
}

static AVCaptureDevice *device(BOOL front) {
    return [AVCaptureDevice defaultDeviceWithDeviceType:AVCaptureDeviceTypeBuiltInWideAngleCamera
        mediaType:AVMediaTypeVideo position:front ? AVCaptureDevicePositionFront : AVCaptureDevicePositionBack];
}
int rt_camera_devices(void) { return (device(YES) ? 1 : 0) | (device(NO) ? 2 : 0); }

@interface RTCallCamera : NSObject <AVCaptureVideoDataOutputSampleBufferDelegate>
@property(strong) AVCaptureSession *session;
@property(strong) AVCaptureDeviceInput *input;
@property(strong) AVCaptureVideoDataOutput *output;
@property(strong) dispatch_queue_t frames;
@property(atomic) BOOL ended;
@property(atomic) AVCaptureVideoOrientation orientation;
@property uint32_t fps;
@end

static void lightOff(AVCaptureDevice *camera) {
    if (camera.hasTorch && [camera lockForConfiguration:NULL]) {
        camera.torchMode = AVCaptureTorchModeOff;
        [camera unlockForConfiguration];
    }
}

@implementation RTCallCamera
- (void)interrupted:(NSNotification *)note {
    self.ended = YES;
    AVCaptureSession *session = self.session;
    // An interrupted session would otherwise resume automatically.
    dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
        @synchronized (session) { lightOff(self.input.device); [session stopRunning]; }
    });
}
- (void)orient:(NSNotification *)note {
    UIInterfaceOrientation orientation = UIApplication.sharedApplication.statusBarOrientation;
    if (orientation != UIInterfaceOrientationUnknown) self.orientation = (AVCaptureVideoOrientation)orientation;
    @synchronized (self.session) { [self configureOrientation]; }
}
- (void)configureOrientation {
    AVCaptureConnection *video = [self.output connectionWithMediaType:AVMediaTypeVideo];
    if (video.isVideoMirroringSupported) {
        video.automaticallyAdjustsVideoMirroring = NO;
        video.videoMirrored = NO;
    }
    if (self.orientation && video.isVideoOrientationSupported)
        video.videoOrientation = self.orientation;
}
- (void)captureOutput:(AVCaptureOutput *)output didOutputSampleBuffer:(CMSampleBufferRef)sample
       fromConnection:(AVCaptureConnection *)connection {
    if (self.ended) return;
    CVPixelBufferRef b = CMSampleBufferGetImageBuffer(sample);
    if (!b || CVPixelBufferLockBaseAddress(b, kCVPixelBufferLock_ReadOnly) != kCVReturnSuccess) return;
    rt_camera_bgra(CVPixelBufferGetBaseAddress(b), (uint32_t)CVPixelBufferGetWidth(b),
                   (uint32_t)CVPixelBufferGetHeight(b), (uint32_t)CVPixelBufferGetBytesPerRow(b));
    CVPixelBufferUnlockBaseAddress(b, kCVPixelBufferLock_ReadOnly);
}
@end

// Rust serializes starts and joins the previous stop. The delegate and lifecycle
// notifications retain their own session so late callbacks cannot stop its replacement.
static RTCallCamera *current;

bool rt_camera_start(bool front, uint32_t height, uint32_t fps) {
    @autoreleasepool {
        AVCaptureDevice *camera = device(front);
        if (!camera) return false;
        AVCaptureDeviceInput *input = [AVCaptureDeviceInput deviceInputWithDevice:camera error:NULL];
        RTCallCamera *c = [RTCallCamera new];
        AVCaptureSession *s = c.session = [AVCaptureSession new];
        if (!input || ![s canAddInput:input]) return false;
        [s addInput:input];
        c.input = input;
        c.fps = fps;
        lightOff(camera);
        AVCaptureSessionPreset preset = height >= 1080 ? AVCaptureSessionPreset1920x1080 :
            height >= 720 ? AVCaptureSessionPreset1280x720 : AVCaptureSessionPreset640x480;
        if ([s canSetSessionPreset:preset]) s.sessionPreset = preset;
        for (AVFrameRateRange *range in camera.activeFormat.videoSupportedFrameRateRanges) {
            if (range.minFrameRate <= fps && fps <= range.maxFrameRate && [camera lockForConfiguration:NULL]) {
                camera.activeVideoMinFrameDuration = CMTimeMake(1, fps);
                camera.activeVideoMaxFrameDuration = CMTimeMake(1, fps);
                [camera unlockForConfiguration];
                break;
            }
        }
        AVCaptureVideoDataOutput *out = c.output = [AVCaptureVideoDataOutput new];
        out.videoSettings = @{(id)kCVPixelBufferPixelFormatTypeKey: @(kCVPixelFormatType_32BGRA)};
        out.alwaysDiscardsLateVideoFrames = YES;
        c.frames = dispatch_queue_create("io.github.rusttalkcrew.camera", DISPATCH_QUEUE_SERIAL);
        [out setSampleBufferDelegate:c queue:c.frames];
        if (![s canAddOutput:out]) return false;
        [s addOutput:out];
        NSNotificationCenter *nc = NSNotificationCenter.defaultCenter;
        dispatch_sync(dispatch_get_main_queue(), ^{
            if (UIApplication.sharedApplication.applicationState != UIApplicationStateActive) {
                c.ended = YES;
                return;
            }
            [c orient:nil];
            [nc addObserver:c selector:@selector(orient:) name:UIApplicationDidChangeStatusBarOrientationNotification object:nil];
            [nc addObserver:c selector:@selector(interrupted:) name:UIApplicationDidEnterBackgroundNotification object:nil];
            for (NSString *name in @[AVCaptureSessionWasInterruptedNotification, AVCaptureSessionRuntimeErrorNotification])
                [nc addObserver:c selector:@selector(interrupted:) name:name object:s];
        });
        current = c;
        @synchronized (s) { if (!c.ended) [s startRunning]; }
        return s.running && !c.ended;
    }
}

bool rt_camera_ended(void) { return current.ended; }
void rt_camera_stop(void) {
    @autoreleasepool {
        RTCallCamera *c = current;
        c.ended = YES;
        if (!c) return;
        [NSNotificationCenter.defaultCenter removeObserver:c];
        @synchronized (c.session) { lightOff(c.input.device); [c.session stopRunning]; }
        [c.output setSampleBufferDelegate:nil queue:NULL];
        dispatch_sync(c.frames, ^{});
        current = nil;
    }
}

int rt_camera_state(void) {
    RTCallCamera *c = current;
    @synchronized (c.session) {
        if (!c || c.ended || !c.session.running) return 0;
        AVCaptureDevice *d = c.input.device;
        BOOL front = d.position == AVCaptureDevicePositionFront;
        BOOL torch = !front && d.hasTorch && d.isTorchAvailable &&
            [d isTorchModeSupported:AVCaptureTorchModeOn];
        return (front ? 1 : 2) | (torch ? 4 : 0) | (d.isTorchActive ? 8 : 0);
    }
}
bool rt_camera_torch(bool on) {
    RTCallCamera *c = current;
    @synchronized (c.session) {
        if (!(rt_camera_state() & 4)) return false;
        AVCaptureDevice *d = c.input.device;
        if (![d lockForConfiguration:NULL]) return false;
        BOOL ok = YES;
        if (on) ok = [d setTorchModeOnWithLevel:AVCaptureMaxAvailableTorchLevel error:NULL];
        else d.torchMode = AVCaptureTorchModeOff;
        [d unlockForConfiguration];
        return ok;
    }
}
bool rt_camera_select(bool front) {
    RTCallCamera *c = current;
    @synchronized (c.session) {
        if (!c || c.ended || !c.session.running) return false;
        AVCaptureDevice *d = device(front);
        AVCaptureDeviceInput *next = d ? [AVCaptureDeviceInput deviceInputWithDevice:d error:NULL] : nil;
        if (!next) return false;
        AVCaptureDeviceInput *old = c.input;
        lightOff(old.device);
        [c.session beginConfiguration];
        [c.session removeInput:old];
        BOOL ok = [c.session canAddInput:next];
        if (ok) {
            [c.session addInput:next];
            c.input = next;
            lightOff(d);
        } else [c.session addInput:old];
        [c.session commitConfiguration];
        [c configureOrientation];
        AVCaptureDevice *active = c.input.device;
        for (AVFrameRateRange *range in active.activeFormat.videoSupportedFrameRateRanges) {
            if (range.minFrameRate <= c.fps && c.fps <= range.maxFrameRate && [active lockForConfiguration:NULL]) {
                active.activeVideoMinFrameDuration = CMTimeMake(1, c.fps);
                active.activeVideoMaxFrameDuration = CMTimeMake(1, c.fps);
                [active unlockForConfiguration];
                break;
            }
        }
        dispatch_sync(c.frames, ^{});
        return ok;
    }
}
