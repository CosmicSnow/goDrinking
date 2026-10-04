//! Thread-owned AVFoundation webcam capture.
//!
//! Discovery is non-authorizing. Capture authorization is requested only by
//! an explicit stream start, off the UI thread; the session, input, output,
//! delegate and serial callback queue are created and destroyed by one worker.
//! Only copied BGRA pixels and synchronization primitives cross threads.

use block2::RcBlock;
use dispatch2::DispatchQueue;
use golive_platform::{
    BgraFrame, CaptureConfig, CapturePacket, FrameStream, PixelFormat, PlatformError, SourceInfo,
    SourceKind,
};
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureDevice, AVCaptureDeviceDiscoverySession, AVCaptureDeviceInput,
    AVCaptureDevicePosition, AVCaptureDeviceType, AVCaptureOutput, AVCaptureSession,
    AVCaptureSessionPreset, AVCaptureVideoDataOutput, AVCaptureVideoDataOutputSampleBufferDelegate,
};
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, CVPixelBuffer, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{
    NSArray, NSDictionary, NSNotification, NSNotificationCenter, NSNotificationName, NSNumber,
    NSObject, NSObjectProtocol, NSString,
};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const AUTHORIZATION_BUDGET: Duration = Duration::from_secs(60);
const FIRST_FRAME_BUDGET: Duration = Duration::from_secs(8);
const STARTUP_BUDGET: Duration = Duration::from_secs(70);
const CANCEL_POLL: Duration = Duration::from_millis(50);
const CHANNEL_DEPTH: usize = 2;
const MAX_CAMERA_DIM: usize = 8192;
const MAC_CAMERA_PERMISSION_HINT: &str = "Sem permissão de Câmera — autorize em Ajustes → Privacidade e Segurança → Câmera e tente de novo.";

type ReadySender = mpsc::Sender<Result<(), PlatformError>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Authorization {
    NotDetermined,
    Restricted,
    Denied,
    Authorized,
}

fn map_authorization(status: AVAuthorizationStatus) -> Authorization {
    match status {
        AVAuthorizationStatus::Authorized => Authorization::Authorized,
        AVAuthorizationStatus::Denied => Authorization::Denied,
        AVAuthorizationStatus::Restricted => Authorization::Restricted,
        _ => Authorization::NotDetermined,
    }
}

fn permission_error() -> PlatformError {
    PlatformError::PermissionDenied {
        hint: MAC_CAMERA_PERMISSION_HINT,
    }
}

fn canceled_error() -> PlatformError {
    PlatformError::Internal("captura cancelada".into())
}

fn wait_for_authorization(
    cancel: &AtomicBool,
    budget: Duration,
    status: Authorization,
    request: impl FnOnce(mpsc::Sender<bool>),
) -> Result<(), PlatformError> {
    if cancel.load(Ordering::Acquire) {
        return Err(canceled_error());
    }
    match status {
        Authorization::Authorized => Ok(()),
        Authorization::Denied | Authorization::Restricted => Err(permission_error()),
        Authorization::NotDetermined => {
            let (tx, rx) = mpsc::channel();
            request(tx);
            let deadline = Instant::now() + budget;
            loop {
                if cancel.load(Ordering::Acquire) {
                    return Err(canceled_error());
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(PlatformError::Internal(
                        "tempo de autorização da webcam excedido".into(),
                    ));
                }
                match rx.recv_timeout(remaining.min(CANCEL_POLL)) {
                    Ok(true) => {
                        if cancel.load(Ordering::Acquire) {
                            return Err(canceled_error());
                        }
                        return Ok(());
                    }
                    Ok(false) => return Err(permission_error()),
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(PlatformError::Internal(
                            "autorização da webcam indisponível".into(),
                        ));
                    }
                }
            }
        }
    }
}

fn current_authorization() -> Authorization {
    autoreleasepool(|_| unsafe {
        let media_type = objc2_av_foundation::AVMediaTypeVideo
            .expect("AVMediaTypeVideo is exported by AVFoundation");
        map_authorization(AVCaptureDevice::authorizationStatusForMediaType(media_type))
    })
}

fn request_authorization(tx: mpsc::Sender<bool>) {
    autoreleasepool(|_| unsafe {
        let media_type = objc2_av_foundation::AVMediaTypeVideo
            .expect("AVMediaTypeVideo is exported by AVFoundation");
        let block: RcBlock<dyn Fn(objc2::runtime::Bool)> =
            RcBlock::new(move |granted: objc2::runtime::Bool| {
                let _ = tx.send(granted.as_bool());
            });
        AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &block);
    });
}

fn authorize_capture(cancel: &AtomicBool) -> Result<(), PlatformError> {
    // Consult on every start. In particular, do not cache NotDetermined or a
    // prior denial: Settings changes and the first request must be observed.
    wait_for_authorization(
        cancel,
        AUTHORIZATION_BUDGET,
        current_authorization(),
        request_authorization,
    )
}

#[derive(Default)]
struct CameraIdRegistry {
    next_id: u64,
    by_unique: HashMap<String, String>,
    by_local: HashMap<String, String>,
}

impl CameraIdRegistry {
    fn local_id(&mut self, unique_id: &str) -> String {
        if let Some(id) = self.by_unique.get(unique_id) {
            return id.clone();
        }
        self.next_id = self.next_id.saturating_add(1).max(1);
        let id = self.next_id.to_string();
        self.by_unique.insert(unique_id.to_owned(), id.clone());
        self.by_local.insert(id.clone(), unique_id.to_owned());
        id
    }

    fn unique_id(&self, local_id: &str) -> Option<String> {
        self.by_local.get(local_id).cloned()
    }
}

fn id_registry() -> &'static Mutex<CameraIdRegistry> {
    static IDS: OnceLock<Mutex<CameraIdRegistry>> = OnceLock::new();
    IDS.get_or_init(|| Mutex::new(CameraIdRegistry::default()))
}

#[allow(deprecated)]
fn camera_device_types() -> Retained<NSArray<AVCaptureDeviceType>> {
    unsafe {
        NSArray::from_slice(&[
            objc2_av_foundation::AVCaptureDeviceTypeBuiltInWideAngleCamera,
            objc2_av_foundation::AVCaptureDeviceTypeExternalUnknown,
        ])
    }
}

fn discover_devices() -> Result<Vec<Retained<AVCaptureDevice>>, PlatformError> {
    autoreleasepool(|_| unsafe {
        let device_types = camera_device_types();
        let media_type = objc2_av_foundation::AVMediaTypeVideo
            .expect("AVMediaTypeVideo is exported by AVFoundation");
        let discovery =
            AVCaptureDeviceDiscoverySession::discoverySessionWithDeviceTypes_mediaType_position(
                &device_types,
                Some(media_type),
                AVCaptureDevicePosition::Unspecified,
            );
        Ok(discovery.devices().iter().collect())
    })
}

/// Lists devices without asking for camera authorization or opening a device.
pub fn enumerate_cameras() -> Vec<SourceInfo> {
    let Ok(devices) = discover_devices() else {
        return Vec::new();
    };
    let Ok(mut registry) = id_registry().lock() else {
        return Vec::new();
    };
    devices
        .iter()
        .map(|device| {
            let unique_id = unsafe { device.uniqueID() }.to_string();
            let id = registry.local_id(&unique_id);
            let name = unsafe { device.localizedName() }.to_string();
            SourceInfo {
                kind: SourceKind::Camera,
                id,
                name: if name.trim().is_empty() {
                    "Webcam".into()
                } else {
                    name
                },
                w: 1280,
                h: 720,
            }
        })
        .collect()
}

fn registered_unique_id(local_id: &str) -> Result<String, PlatformError> {
    if local_id.is_empty() || !local_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PlatformError::InvalidSource {
            reason: "id de webcam inválido",
        });
    }
    id_registry()
        .lock()
        .map_err(|_| PlatformError::Internal("registro de webcam indisponível".into()))?
        .unique_id(local_id)
        .ok_or_else(|| PlatformError::SourceGone {
            id: local_id.to_owned(),
        })
}

fn resolve_device(unique_id: &str) -> Result<Retained<AVCaptureDevice>, PlatformError> {
    discover_devices()?
        .into_iter()
        .find(|device| unsafe { device.uniqueID() }.to_string() == unique_id)
        .ok_or_else(|| PlatformError::SourceGone {
            id: "camera".into(),
        })
}

#[derive(Default)]
struct CameraLeases {
    active: Mutex<HashSet<String>>,
}

impl CameraLeases {
    fn acquire<'a>(&'a self, unique_id: String) -> Result<CameraLease<'a>, PlatformError> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| PlatformError::Internal("controle de webcam indisponível".into()))?;
        if !active.insert(unique_id.clone()) {
            return Err(PlatformError::Internal("webcam já está em uso".into()));
        }
        Ok(CameraLease {
            owner: self,
            unique_id: Some(unique_id),
        })
    }
}

struct CameraLease<'a> {
    owner: &'a CameraLeases,
    unique_id: Option<String>,
}

impl Drop for CameraLease<'_> {
    fn drop(&mut self) {
        if let Some(unique_id) = self.unique_id.take() {
            if let Ok(mut active) = self.owner.active.lock() {
                active.remove(&unique_id);
            }
        }
    }
}

fn camera_leases() -> &'static CameraLeases {
    static LEASES: OnceLock<CameraLeases> = OnceLock::new();
    LEASES.get_or_init(CameraLeases::default)
}

#[derive(Clone, Copy)]
struct PixelMetadata {
    width: usize,
    height: usize,
    stride: usize,
    format: u32,
}

trait PixelBufferAccess {
    fn metadata(&self) -> PixelMetadata;
    fn lock_read_only(&self) -> bool;
    fn base_address(&self) -> *const u8;
    fn available_len(&self) -> usize;
    fn unlock_read_only(&self);
}

struct UnlockGuard<'a, T: PixelBufferAccess>(&'a T);

impl<T: PixelBufferAccess> Drop for UnlockGuard<'_, T> {
    fn drop(&mut self) {
        self.0.unlock_read_only();
    }
}

fn copy_locked_bgra<T: PixelBufferAccess>(pixel: &T) -> Option<BgraFrame> {
    let meta = pixel.metadata();
    if meta.format != kCVPixelFormatType_32BGRA
        || meta.width == 0
        || meta.height == 0
        || meta.width > MAX_CAMERA_DIM
        || meta.height > MAX_CAMERA_DIM
        || meta.stride < meta.width.checked_mul(4)?
    {
        return None;
    }
    if !pixel.lock_read_only() {
        return None;
    }
    let _unlock = UnlockGuard(pixel);
    let required = meta
        .stride
        .checked_mul(meta.height - 1)?
        .checked_add(meta.width.checked_mul(4)?)?;
    let address = pixel.base_address();
    if address.is_null() || required > pixel.available_len() {
        return None;
    }
    // SAFETY: AVFoundation holds the CVPixelBuffer for this callback, the
    // base address is locked above, and the validated extent includes the
    // complete last row even when that row is stride-padded.
    let bytes = unsafe { std::slice::from_raw_parts(address, required) };
    copy_bgra_rows(meta.width, meta.height, meta.stride, bytes)
}

fn copy_bgra_rows(width: usize, height: usize, stride: usize, bytes: &[u8]) -> Option<BgraFrame> {
    let row_bytes = width.checked_mul(4)?;
    let required = stride
        .checked_mul(height.checked_sub(1)?)?
        .checked_add(row_bytes)?;
    if width == 0
        || height == 0
        || width > MAX_CAMERA_DIM
        || height > MAX_CAMERA_DIM
        || stride < row_bytes
        || bytes.len() < required
    {
        return None;
    }
    let mut packed = vec![0; row_bytes.checked_mul(height)?];
    for row in 0..height {
        let source = row.checked_mul(stride)?;
        let destination = row.checked_mul(row_bytes)?;
        packed[destination..destination + row_bytes]
            .copy_from_slice(&bytes[source..source + row_bytes]);
    }
    Some(BgraFrame {
        w: width as u32,
        h: height as u32,
        stride: row_bytes,
        format: PixelFormat::Bgra8888,
        data: packed,
    })
}

struct CvPixelBufferAccess<'a>(&'a CVPixelBuffer);

impl PixelBufferAccess for CvPixelBufferAccess<'_> {
    fn metadata(&self) -> PixelMetadata {
        PixelMetadata {
            width: CVPixelBufferGetWidth(self.0),
            height: CVPixelBufferGetHeight(self.0),
            stride: CVPixelBufferGetBytesPerRow(self.0),
            format: CVPixelBufferGetPixelFormatType(self.0),
        }
    }

    fn lock_read_only(&self) -> bool {
        unsafe { CVPixelBufferLockBaseAddress(self.0, CVPixelBufferLockFlags::ReadOnly) == 0 }
    }

    fn base_address(&self) -> *const u8 {
        CVPixelBufferGetBaseAddress(self.0).cast()
    }

    fn available_len(&self) -> usize {
        self.metadata()
            .stride
            .saturating_mul(self.metadata().height)
    }

    fn unlock_read_only(&self) {
        unsafe {
            CVPixelBufferUnlockBaseAddress(self.0, CVPixelBufferLockFlags::ReadOnly);
        }
    }
}

struct CallbackIvars {
    tx: mpsc::SyncSender<CapturePacket>,
    ready: ReadySender,
    first_valid: AtomicBool,
    stop: Arc<AtomicBool>,
    last_frame_ns: AtomicU64,
    interval_ns: u64,
}

struct RuntimeObservers {
    center: Retained<NSNotificationCenter>,
    tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl RuntimeObservers {
    fn new(
        session: &AVCaptureSession,
        device: &AVCaptureDevice,
        signals: mpsc::Sender<RuntimeSignal>,
    ) -> Self {
        let center = NSNotificationCenter::defaultCenter();
        let session_object: &objc2::runtime::AnyObject = session.as_ref();
        let device_object: &objc2::runtime::AnyObject = device.as_ref();
        let tokens = unsafe {
            vec![
                add_runtime_observer(
                    &center,
                    objc2_av_foundation::AVCaptureSessionRuntimeErrorNotification,
                    session_object,
                    RuntimeSignal::SessionRuntimeError,
                    signals.clone(),
                ),
                add_runtime_observer(
                    &center,
                    objc2_av_foundation::AVCaptureSessionWasInterruptedNotification,
                    session_object,
                    RuntimeSignal::SessionInterrupted,
                    signals.clone(),
                ),
                add_runtime_observer(
                    &center,
                    objc2_av_foundation::AVCaptureDeviceWasDisconnectedNotification,
                    device_object,
                    RuntimeSignal::DeviceDisconnected,
                    signals,
                ),
            ]
        };
        Self { center, tokens }
    }

    fn unregister(&mut self) {
        for token in self.tokens.drain(..) {
            let object: &objc2::runtime::AnyObject = token.as_ref();
            unsafe { self.center.removeObserver(object) };
        }
    }
}

unsafe fn add_runtime_observer(
    center: &NSNotificationCenter,
    name: &'static NSNotificationName,
    object: &objc2::runtime::AnyObject,
    signal: RuntimeSignal,
    signals: mpsc::Sender<RuntimeSignal>,
) -> Retained<ProtocolObject<dyn NSObjectProtocol>> {
    let block: RcBlock<dyn Fn(std::ptr::NonNull<NSNotification>)> = RcBlock::new(move |_| {
        let _ = signals.send(signal);
    });
    center.addObserverForName_object_queue_usingBlock(Some(name), Some(object), None, &block)
}

struct AvfSessionControl {
    session: Retained<AVCaptureSession>,
    output: Retained<AVCaptureVideoDataOutput>,
    _delegate: Retained<ProtocolObject<dyn AVCaptureVideoDataOutputSampleBufferDelegate>>,
    queue: dispatch2::DispatchRetained<DispatchQueue>,
    observers: RuntimeObservers,
}

impl SessionControl for AvfSessionControl {
    fn start(&mut self) {
        unsafe { self.session.startRunning() };
    }

    fn disable_delivery(&mut self) {
        unsafe { self.output.setSampleBufferDelegate_queue(None, None) };
    }

    fn unregister_observers(&mut self) {
        self.observers.unregister();
    }

    fn stop_session(&mut self) {
        unsafe { self.session.stopRunning() };
    }

    fn fence_callbacks(&mut self) {
        self.queue.exec_sync(|| {});
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuntimeSignal {
    SessionRuntimeError,
    SessionInterrupted,
    DeviceDisconnected,
}

fn runtime_error(signal: RuntimeSignal, local_id: &str) -> PlatformError {
    match signal {
        RuntimeSignal::DeviceDisconnected => PlatformError::SourceGone {
            id: local_id.to_owned(),
        },
        RuntimeSignal::SessionRuntimeError => {
            PlatformError::Internal("falha durante captura da webcam".into())
        }
        RuntimeSignal::SessionInterrupted => {
            PlatformError::Internal("captura da webcam interrompida".into())
        }
    }
}

trait SessionControl {
    fn start(&mut self);
    fn disable_delivery(&mut self);
    fn unregister_observers(&mut self);
    fn stop_session(&mut self);
    fn fence_callbacks(&mut self);
}

fn owner_teardown(control: &mut impl SessionControl) {
    control.disable_delivery();
    control.unregister_observers();
    control.stop_session();
    control.fence_callbacks();
}

fn run_owner_lifecycle(
    control: &mut impl SessionControl,
    caller_ready: &ReadySender,
    first_frame: &mpsc::Receiver<Result<(), PlatformError>>,
    runtime: &mpsc::Receiver<RuntimeSignal>,
    cancel: &AtomicBool,
    error: &Mutex<Option<PlatformError>>,
    local_id: &str,
    first_frame_budget: Duration,
) {
    control.start();
    let deadline = Instant::now() + first_frame_budget;
    let started = loop {
        if cancel.load(Ordering::Acquire) {
            break false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            signal_start_error(
                caller_ready,
                PlatformError::Internal("tempo para iniciar a webcam excedido".into()),
            );
            break false;
        }
        match first_frame.recv_timeout(remaining.min(CANCEL_POLL)) {
            Ok(Ok(())) => {
                if cancel.load(Ordering::Acquire) {
                    break false;
                }
                let _ = caller_ready.send(Ok(()));
                break true;
            }
            Ok(Err(failure)) => {
                set_stream_error(error, failure.clone());
                signal_start_error(caller_ready, failure);
                break false;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if !cancel.load(Ordering::Acquire) {
                    signal_start_error(
                        caller_ready,
                        PlatformError::Internal(
                            "a webcam encerrou antes do primeiro quadro".into(),
                        ),
                    );
                }
                break false;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(signal) = runtime.try_recv() {
                    let failure = runtime_error(signal, local_id);
                    set_stream_error(error, failure.clone());
                    signal_start_error(caller_ready, failure);
                    break false;
                }
            }
        }
    };

    if started {
        while !cancel.load(Ordering::Acquire) {
            match runtime.recv_timeout(CANCEL_POLL) {
                Ok(signal) => {
                    set_stream_error(error, runtime_error(signal, local_id));
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }
    // Fence the delegate before teardown even for timeout/runtime errors.
    cancel.store(true, Ordering::Release);
    // This production-used seam is also the test boundary for exactly-once
    // teardown: no caller releases the lease until the owner returns.
    owner_teardown(control);
}

fn set_stream_error(error: &Mutex<Option<PlatformError>>, failure: PlatformError) {
    if let Ok(mut slot) = error.lock() {
        *slot = Some(failure);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeliveryOutcome {
    Stopped,
    CadenceDropped,
    Invalid,
    Queued,
    Full,
    Disconnected,
}

fn deliver_sample(
    ivars: &CallbackIvars,
    now: u64,
    copy: impl FnOnce() -> Option<BgraFrame>,
) -> DeliveryOutcome {
    if ivars.stop.load(Ordering::Acquire) {
        return DeliveryOutcome::Stopped;
    }
    let previous = ivars.last_frame_ns.load(Ordering::Relaxed);
    if previous != 0 && now.wrapping_sub(previous) < ivars.interval_ns {
        return DeliveryOutcome::CadenceDropped;
    }
    let Some(frame) = copy() else {
        return DeliveryOutcome::Invalid;
    };
    if ivars.stop.load(Ordering::Acquire) {
        return DeliveryOutcome::Stopped;
    }
    ivars.last_frame_ns.store(now, Ordering::Relaxed);
    if !ivars.first_valid.swap(true, Ordering::AcqRel) {
        let _ = ivars.ready.send(Ok(()));
    }
    match ivars.tx.try_send(CapturePacket::Cpu(frame)) {
        Ok(()) => DeliveryOutcome::Queued,
        Err(mpsc::TrySendError::Full(_)) => DeliveryOutcome::Full,
        Err(mpsc::TrySendError::Disconnected(_)) => {
            ivars.stop.store(true, Ordering::Release);
            DeliveryOutcome::Disconnected
        }
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = CallbackIvars]
    struct CameraDelegate;

    unsafe impl NSObjectProtocol for CameraDelegate {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for CameraDelegate {
        #[allow(non_snake_case)]
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        unsafe fn captureOutput_didOutputSampleBuffer_fromConnection(
            &self,
            _output: &AVCaptureOutput,
            sample_buffer: &objc2_core_media::CMSampleBuffer,
            _connection: &objc2_av_foundation::AVCaptureConnection,
        ) {
            autoreleasepool(|_| {
                let ivars = self.ivars();
                let now = monotonic_ns();
                let _ = deliver_sample(ivars, now, || {
                    // SAFETY: AVFoundation supplies the sample buffer for the
                    // lifetime of this callback. The retained image buffer
                    // keeps its pixel data alive through the locked CPU copy.
                    let image = unsafe { sample_buffer.image_buffer() }?;
                    let image_ptr = objc2_core_foundation::CFRetained::as_ptr(&image).as_ptr();
                    let pixel = unsafe { &*(image_ptr as *const CVPixelBuffer) };
                    copy_locked_bgra(&CvPixelBufferAccess(pixel))
                });
            });
        }
    }
);

fn monotonic_ns() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

unsafe fn selected_preset(config: &CaptureConfig) -> &'static AVCaptureSessionPreset {
    if config.width >= 1280 && config.height >= 720 {
        objc2_av_foundation::AVCaptureSessionPreset1280x720
    } else {
        objc2_av_foundation::AVCaptureSessionPreset640x480
    }
}

fn make_session(
    device: &AVCaptureDevice,
    config: &CaptureConfig,
    tx: mpsc::SyncSender<CapturePacket>,
    ready: ReadySender,
    stop: Arc<AtomicBool>,
    signals: mpsc::Sender<RuntimeSignal>,
) -> Result<AvfSessionControl, PlatformError> {
    unsafe {
        let session = AVCaptureSession::new();
        let input = AVCaptureDeviceInput::deviceInputWithDevice_error(device)
            .map_err(|_| map_input_error(current_authorization()))?;
        let output = AVCaptureVideoDataOutput::new();
        let preset = selected_preset(config);
        let queue = DispatchQueue::new("dev.golive.camera.frames", None);
        let interval_ns = 1_000_000_000u64 / config.fps.clamp(1, 60) as u64;
        let delegate = CameraDelegate::alloc().set_ivars(CallbackIvars {
            tx,
            ready,
            first_valid: AtomicBool::new(false),
            stop,
            last_frame_ns: AtomicU64::new(0),
            interval_ns,
        });
        let delegate: Retained<CameraDelegate> = msg_send![super(delegate), init];
        let delegate =
            ProtocolObject::<dyn AVCaptureVideoDataOutputSampleBufferDelegate>::from_retained(
                delegate,
            );

        session.beginConfiguration();
        let setup = (|| {
            if session.canSetSessionPreset(preset) {
                session.setSessionPreset(preset);
            }
            if !session.canAddInput(&input) {
                return Err(PlatformError::Internal("webcam indisponível".into()));
            }
            session.addInput(&input);
            output.setAlwaysDiscardsLateVideoFrames(true);
            let value = NSNumber::new_u32(kCVPixelFormatType_32BGRA);
            let settings: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_slices(
                &[objc2_foundation::ns_string!("PixelFormatType")],
                &[&*value],
            );
            output.setVideoSettings(Some(&settings));
            if !session.canAddOutput(&output) {
                return Err(PlatformError::Internal("webcam indisponível".into()));
            }
            session.addOutput(&output);
            output.setSampleBufferDelegate_queue(Some(&delegate), Some(&queue));
            Ok(())
        })();
        session.commitConfiguration();
        setup?;
        let observers = RuntimeObservers::new(&session, device, signals);
        Ok(AvfSessionControl {
            session,
            output,
            _delegate: delegate,
            queue,
            observers,
        })
    }
}

fn map_input_error(authorization: Authorization) -> PlatformError {
    match authorization {
        Authorization::Denied | Authorization::Restricted => permission_error(),
        Authorization::Authorized | Authorization::NotDetermined => {
            PlatformError::Internal("webcam indisponível".into())
        }
    }
}

fn signal_start_error(ready: &ReadySender, error: PlatformError) {
    let _ = ready.send(Err(error));
}

fn wait_for_ready(
    ready: &mpsc::Receiver<Result<(), PlatformError>>,
    cancel: &AtomicBool,
    budget: Duration,
) -> Result<(), PlatformError> {
    let deadline = Instant::now() + budget;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(canceled_error());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(PlatformError::Internal(
                "tempo para iniciar a webcam excedido".into(),
            ));
        }
        match ready.recv_timeout(remaining.min(CANCEL_POLL)) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(PlatformError::Internal(
                    "a webcam encerrou antes do primeiro quadro".into(),
                ));
            }
        }
    }
}

fn camera_worker(
    local_id: String,
    config: CaptureConfig,
    tx: mpsc::SyncSender<CapturePacket>,
    ready: ReadySender,
    first_frame_rx: mpsc::Receiver<Result<(), PlatformError>>,
    first_frame_tx: ReadySender,
    runtime_rx: mpsc::Receiver<RuntimeSignal>,
    runtime_tx: mpsc::Sender<RuntimeSignal>,
    error: Arc<Mutex<Option<PlatformError>>>,
    cancel: Arc<AtomicBool>,
) {
    if let Err(error) = authorize_capture(&cancel) {
        if !cancel.load(Ordering::Acquire) {
            signal_start_error(&ready, error);
        }
        return;
    }
    // A delayed grant after caller cancellation must never proceed to device
    // lookup, session construction, or startRunning.
    if cancel.load(Ordering::Acquire) {
        return;
    }
    let unique_id = match registered_unique_id(&local_id) {
        Ok(unique_id) => unique_id,
        Err(error) => {
            signal_start_error(&ready, error);
            return;
        }
    };
    let device = match resolve_device(&unique_id) {
        Ok(device) => device,
        Err(error) => {
            signal_start_error(&ready, error);
            return;
        }
    };
    let _lease = match camera_leases().acquire(unique_id) {
        Ok(lease) => lease,
        Err(error) => {
            signal_start_error(&ready, error);
            return;
        }
    };
    if cancel.load(Ordering::Acquire) {
        return;
    }
    let setup = autoreleasepool(|_| {
        make_session(
            &device,
            &config,
            tx,
            first_frame_tx,
            Arc::clone(&cancel),
            runtime_tx,
        )
    });
    let mut session = match setup {
        Ok(session) => session,
        Err(error) => {
            signal_start_error(&ready, error);
            return;
        }
    };
    if cancel.load(Ordering::Acquire) {
        return;
    }
    run_owner_lifecycle(
        &mut session,
        &ready,
        &first_frame_rx,
        &runtime_rx,
        &cancel,
        &error,
        &local_id,
        FIRST_FRAME_BUDGET,
    );
    drop(session);
}

pub fn start_camera(
    info: &SourceInfo,
    config: &CaptureConfig,
    cancel: Arc<AtomicBool>,
) -> Result<FrameStream, PlatformError> {
    if cancel.load(Ordering::Acquire) {
        return Err(canceled_error());
    }
    if info.id.is_empty() || !info.id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PlatformError::InvalidSource {
            reason: "id de webcam inválido",
        });
    }
    let (frame_tx, frame_rx) = mpsc::sync_channel(CHANNEL_DEPTH);
    let (caller_ready_tx, caller_ready_rx) = mpsc::channel();
    let (worker_ready_tx, worker_ready_rx) = mpsc::channel();
    let (runtime_tx, runtime_rx) = mpsc::channel();
    let error = Arc::new(Mutex::new(None));
    let worker_error = Arc::clone(&error);
    let cancel_worker = Arc::clone(&cancel);
    let local_id_worker = info.id.clone();
    let config = *config;
    let worker = std::thread::Builder::new()
        .name("golive-avfoundation-camera".into())
        .spawn(move || {
            camera_worker(
                local_id_worker,
                config,
                frame_tx,
                caller_ready_tx.clone(),
                worker_ready_rx,
                worker_ready_tx,
                runtime_rx,
                runtime_tx,
                worker_error,
                cancel_worker,
            )
        })
        .map_err(|error| PlatformError::Internal(format!("thread de captura: {error}")))?;
    match wait_for_ready(&caller_ready_rx, &cancel, STARTUP_BUDGET) {
        Ok(()) => Ok(FrameStream::new(frame_rx, error, cancel, worker)),
        Err(error) => {
            cancel.store(true, Ordering::Release);
            // Never join on this caller: AVFoundation start/stop may block.
            // The detached owner keeps the device lease until teardown really
            // completes, so a retry cannot race a still-live session.
            drop(worker);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn authorization_statuses_and_async_results_are_typed() {
        let cancel = AtomicBool::new(false);
        assert!(wait_for_authorization(
            &cancel,
            Duration::from_secs(1),
            Authorization::Authorized,
            |_| unreachable!()
        )
        .is_ok());
        assert!(matches!(
            wait_for_authorization(
                &cancel,
                Duration::from_secs(1),
                Authorization::Denied,
                |_| unreachable!()
            ),
            Err(PlatformError::PermissionDenied { .. })
        ));
        assert!(matches!(
            wait_for_authorization(
                &cancel,
                Duration::from_secs(1),
                Authorization::Restricted,
                |_| unreachable!()
            ),
            Err(PlatformError::PermissionDenied { .. })
        ));
        assert!(wait_for_authorization(
            &cancel,
            Duration::from_secs(1),
            Authorization::NotDetermined,
            |tx| {
                let _ = tx.send(true);
            }
        )
        .is_ok());
        assert!(matches!(
            wait_for_authorization(
                &cancel,
                Duration::from_secs(1),
                Authorization::NotDetermined,
                |tx| {
                    let _ = tx.send(false);
                }
            ),
            Err(PlatformError::PermissionDenied { .. })
        ));
    }

    #[test]
    fn late_grant_after_cancel_never_reaches_capture_start() {
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_worker = Arc::clone(&cancel);
        let (request_tx, request_rx) = mpsc::channel();
        let starter_called = Arc::new(AtomicBool::new(false));
        let starter = Arc::clone(&starter_called);
        let worker = std::thread::spawn(move || {
            let result = wait_for_authorization(
                &cancel_worker,
                Duration::from_secs(2),
                Authorization::NotDetermined,
                move |tx| {
                    let _ = request_tx.send(tx);
                },
            );
            if result.is_ok() && !cancel_worker.load(Ordering::Acquire) {
                starter.store(true, Ordering::Release);
            }
            result
        });
        let late_grant = request_rx.recv().unwrap();
        cancel.store(true, Ordering::Release);
        let _ = late_grant.send(true);
        assert!(worker.join().unwrap().is_err());
        assert!(!starter_called.load(Ordering::Acquire));
    }

    #[test]
    fn first_frame_readiness_times_out_or_cancels_without_a_join() {
        let (_tx, rx) = mpsc::channel();
        let cancel = AtomicBool::new(false);
        assert!(wait_for_ready(&rx, &cancel, Duration::from_millis(1)).is_err());
        cancel.store(true, Ordering::Release);
        assert!(matches!(
            wait_for_ready(&rx, &cancel, Duration::from_secs(1)),
            Err(PlatformError::Internal(_))
        ));
    }

    #[test]
    fn copies_all_rows_including_padded_last_row() {
        let source = [
            1, 2, 3, 4, 5, 6, 7, 8, 90, 91, 92, 93, 9, 10, 11, 12, 13, 14, 15, 16,
        ];
        let frame = copy_bgra_rows(2, 2, 12, &source).unwrap();
        assert_eq!(frame.stride, 8);
        assert_eq!(
            frame.data,
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
        assert!(copy_bgra_rows(2, 2, 12, &source[..19]).is_none());
    }

    struct FakePixels {
        meta: PixelMetadata,
        bytes: Vec<u8>,
        locked: Cell<bool>,
        unlocked: Cell<bool>,
    }

    impl PixelBufferAccess for FakePixels {
        fn metadata(&self) -> PixelMetadata {
            self.meta
        }
        fn lock_read_only(&self) -> bool {
            self.locked.set(true);
            true
        }
        fn base_address(&self) -> *const u8 {
            self.bytes.as_ptr()
        }
        fn available_len(&self) -> usize {
            self.bytes.len()
        }
        fn unlock_read_only(&self) {
            self.unlocked.set(true);
        }
    }

    #[test]
    fn locked_copy_unlocks_on_success_and_invalid_extent() {
        let valid = FakePixels {
            meta: PixelMetadata {
                width: 1,
                height: 2,
                stride: 8,
                format: kCVPixelFormatType_32BGRA,
            },
            bytes: vec![1, 2, 3, 4, 99, 99, 99, 99, 5, 6, 7, 8],
            locked: Cell::new(false),
            unlocked: Cell::new(false),
        };
        assert!(copy_locked_bgra(&valid).is_some());
        assert!(valid.locked.get() && valid.unlocked.get());

        let short = FakePixels {
            meta: PixelMetadata {
                width: 1,
                height: 2,
                stride: 8,
                format: kCVPixelFormatType_32BGRA,
            },
            bytes: vec![1, 2, 3, 4],
            locked: Cell::new(false),
            unlocked: Cell::new(false),
        };
        assert!(copy_locked_bgra(&short).is_none());
        assert!(short.locked.get() && short.unlocked.get());
    }

    #[test]
    fn camera_lease_is_held_until_owner_teardown() {
        let leases = CameraLeases::default();
        let lease = leases.acquire("stable-device".into()).unwrap();
        assert!(leases.acquire("stable-device".into()).is_err());
        drop(lease);
        assert!(leases.acquire("stable-device".into()).is_ok());
    }

    #[test]
    fn local_ids_keep_stable_device_identity_and_are_not_recycled() {
        let mut ids = CameraIdRegistry::default();
        let first = ids.local_id("device-a");
        assert_eq!(ids.local_id("device-a"), first);
        assert_ne!(ids.local_id("device-b"), first);
        assert_eq!(ids.unique_id(&first).as_deref(), Some("device-a"));
    }

    fn test_frame() -> BgraFrame {
        BgraFrame {
            w: 1,
            h: 1,
            stride: 4,
            format: PixelFormat::Bgra8888,
            data: vec![1, 2, 3, 255],
        }
    }

    #[test]
    fn invalid_sample_never_completes_first_frame_readiness() {
        let (tx, _rx) = mpsc::sync_channel(1);
        let (ready, ready_rx) = mpsc::channel();
        let ivars = CallbackIvars {
            tx,
            ready,
            first_valid: AtomicBool::new(false),
            stop: Arc::new(AtomicBool::new(false)),
            last_frame_ns: AtomicU64::new(0),
            interval_ns: 1,
        };
        assert_eq!(
            deliver_sample(&ivars, 10, || None),
            DeliveryOutcome::Invalid
        );
        assert!(!ivars.first_valid.load(Ordering::Acquire));
        assert!(ready_rx.try_recv().is_err());
    }

    #[test]
    fn stop_and_cadence_gates_precede_pixel_copy() {
        let (tx, _rx) = mpsc::sync_channel(1);
        let (ready, _ready_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(true));
        let ivars = CallbackIvars {
            tx,
            ready,
            first_valid: AtomicBool::new(false),
            stop: Arc::clone(&stop),
            last_frame_ns: AtomicU64::new(8),
            interval_ns: 5,
        };
        let copied = Cell::new(false);
        assert_eq!(
            deliver_sample(&ivars, 20, || {
                copied.set(true);
                Some(test_frame())
            }),
            DeliveryOutcome::Stopped
        );
        assert!(!copied.get());
        stop.store(false, Ordering::Release);
        assert_eq!(
            deliver_sample(&ivars, 10, || {
                copied.set(true);
                Some(test_frame())
            }),
            DeliveryOutcome::CadenceDropped
        );
        assert!(!copied.get());
    }

    #[test]
    fn callback_is_bounded_and_distinguishes_full_from_disconnected() {
        let (tx, _rx) = mpsc::sync_channel(1);
        let (ready, ready_rx) = mpsc::channel();
        let ivars = CallbackIvars {
            tx,
            ready,
            first_valid: AtomicBool::new(false),
            stop: Arc::new(AtomicBool::new(false)),
            last_frame_ns: AtomicU64::new(0),
            interval_ns: 1,
        };
        assert_eq!(
            deliver_sample(&ivars, 10, || Some(test_frame())),
            DeliveryOutcome::Queued
        );
        assert_eq!(
            deliver_sample(&ivars, 12, || Some(test_frame())),
            DeliveryOutcome::Full
        );
        assert!(matches!(ready_rx.try_recv(), Ok(Ok(()))));
        assert!(ivars.tx.try_send(CapturePacket::Cpu(test_frame())).is_err());

        let (tx, rx) = mpsc::sync_channel(1);
        drop(rx);
        let (ready, _ready_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let disconnected = CallbackIvars {
            tx,
            ready,
            first_valid: AtomicBool::new(false),
            stop: Arc::clone(&stop),
            last_frame_ns: AtomicU64::new(0),
            interval_ns: 1,
        };
        assert_eq!(
            deliver_sample(&disconnected, 10, || Some(test_frame())),
            DeliveryOutcome::Disconnected
        );
        assert!(stop.load(Ordering::Acquire));
    }

    #[test]
    fn input_failure_rechecks_authorization_without_exposing_os_text() {
        assert!(matches!(
            map_input_error(Authorization::Denied),
            PlatformError::PermissionDenied {
                hint: MAC_CAMERA_PERMISSION_HINT
            }
        ));
        assert!(matches!(
            map_input_error(Authorization::Restricted),
            PlatformError::PermissionDenied {
                hint: MAC_CAMERA_PERMISSION_HINT
            }
        ));
        assert_eq!(
            map_input_error(Authorization::Authorized),
            PlatformError::Internal("webcam indisponível".into())
        );
    }

    struct TestSessionControl {
        events: Arc<Mutex<Vec<&'static str>>>,
        first_frame: Option<ReadySender>,
        stop_gate: Option<mpsc::Receiver<()>>,
        stopped: Option<mpsc::Sender<()>>,
        late_callback: Option<Arc<Mutex<Option<DeliveryOutcome>>>>,
        callback_stop: Option<Arc<AtomicBool>>,
        _frame_sink: Option<mpsc::SyncSender<CapturePacket>>,
        fence_gate: Option<mpsc::Receiver<()>>,
        fence_entered: Option<mpsc::Sender<()>>,
    }

    impl TestSessionControl {
        fn ready(events: Arc<Mutex<Vec<&'static str>>>, first_frame: ReadySender) -> Self {
            Self {
                events,
                first_frame: Some(first_frame),
                stop_gate: None,
                stopped: None,
                late_callback: None,
                callback_stop: None,
                _frame_sink: None,
                fence_gate: None,
                fence_entered: None,
            }
        }

        fn waiting(events: Arc<Mutex<Vec<&'static str>>>) -> Self {
            Self {
                events,
                first_frame: None,
                stop_gate: None,
                stopped: None,
                late_callback: None,
                callback_stop: None,
                _frame_sink: None,
                fence_gate: None,
                fence_entered: None,
            }
        }

        fn record(&self, event: &'static str) {
            self.events.lock().unwrap().push(event);
        }
    }

    impl SessionControl for TestSessionControl {
        fn start(&mut self) {
            self.record("start");
            if let Some(sender) = self.first_frame.take() {
                let _ = sender.send(Ok(()));
            }
        }

        fn disable_delivery(&mut self) {
            self.record("disable");
            if let (Some(stop), Some(result)) = (&self.callback_stop, &self.late_callback) {
                let (tx, _rx) = mpsc::sync_channel(1);
                let (ready, _ready_rx) = mpsc::channel();
                let callback = CallbackIvars {
                    tx,
                    ready,
                    first_valid: AtomicBool::new(true),
                    stop: Arc::clone(stop),
                    last_frame_ns: AtomicU64::new(0),
                    interval_ns: 1,
                };
                *result.lock().unwrap() = Some(deliver_sample(&callback, 1, || Some(test_frame())));
            }
        }

        fn unregister_observers(&mut self) {
            self.record("unregister");
        }

        fn stop_session(&mut self) {
            self.record("stop-enter");
            if let Some(gate) = self.stop_gate.take() {
                let _ = gate.recv();
            }
            self.record("stop-exit");
            if let Some(stopped) = self.stopped.take() {
                let _ = stopped.send(());
            }
        }

        fn fence_callbacks(&mut self) {
            self.record("fence");
            if let Some(entered) = self.fence_entered.take() {
                let _ = entered.send(());
            }
            if let Some(gate) = self.fence_gate.take() {
                let _ = gate.recv();
            }
        }
    }

    fn spawn_test_owner(
        control: TestSessionControl,
        cancel: Arc<AtomicBool>,
        first_frame: mpsc::Receiver<Result<(), PlatformError>>,
        runtime: mpsc::Receiver<RuntimeSignal>,
        error: Arc<Mutex<Option<PlatformError>>>,
        budget: Duration,
    ) -> (
        mpsc::Receiver<Result<(), PlatformError>>,
        std::thread::JoinHandle<()>,
    ) {
        let (ready_rx, _done_rx, worker) =
            spawn_test_owner_with_lease(control, cancel, first_frame, runtime, error, budget, None);
        (ready_rx, worker)
    }

    fn spawn_test_owner_with_lease(
        mut control: TestSessionControl,
        cancel: Arc<AtomicBool>,
        first_frame: mpsc::Receiver<Result<(), PlatformError>>,
        runtime: mpsc::Receiver<RuntimeSignal>,
        error: Arc<Mutex<Option<PlatformError>>>,
        budget: Duration,
        lease: Option<CameraLease<'static>>,
    ) -> (
        mpsc::Receiver<Result<(), PlatformError>>,
        mpsc::Receiver<()>,
        std::thread::JoinHandle<()>,
    ) {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker_cancel = Arc::clone(&cancel);
        let worker = std::thread::spawn(move || {
            run_owner_lifecycle(
                &mut control,
                &ready_tx,
                &first_frame,
                &runtime,
                &worker_cancel,
                &error,
                "9",
                budget,
            );
            drop(control);
            drop(lease);
            let _ = done_tx.send(());
        });
        (ready_rx, done_rx, worker)
    }

    fn teardown_events(events: &Arc<Mutex<Vec<&'static str>>>) -> Vec<&'static str> {
        events.lock().unwrap().clone()
    }

    #[test]
    fn production_owner_timeout_tears_down_once() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let control = TestSessionControl::waiting(events.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let (_first_tx, first_rx) = mpsc::channel();
        let (_runtime_tx, runtime_rx) = mpsc::channel();
        let (ready_rx, worker) = spawn_test_owner(
            control,
            Arc::clone(&cancel),
            first_rx,
            runtime_rx,
            Arc::new(Mutex::new(None)),
            Duration::from_millis(5),
        );
        assert!(matches!(
            ready_rx.recv().unwrap(),
            Err(PlatformError::Internal(_))
        ));
        worker.join().unwrap();
        assert_eq!(
            teardown_events(&events),
            vec![
                "start",
                "disable",
                "unregister",
                "stop-enter",
                "stop-exit",
                "fence"
            ]
        );
        assert!(cancel.load(Ordering::Acquire));
    }

    #[test]
    fn production_owner_cancel_after_ready_fences_late_callback_before_release() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let cancel = Arc::new(AtomicBool::new(false));
        let late_result = Arc::new(Mutex::new(None));
        let (first_tx, first_rx) = mpsc::channel();
        let mut control = TestSessionControl::ready(events.clone(), first_tx);
        control.callback_stop = Some(Arc::clone(&cancel));
        control.late_callback = Some(Arc::clone(&late_result));
        let (_runtime_tx, runtime_rx) = mpsc::channel();
        let (ready_rx, worker) = spawn_test_owner(
            control,
            Arc::clone(&cancel),
            first_rx,
            runtime_rx,
            Arc::new(Mutex::new(None)),
            Duration::from_secs(1),
        );
        assert!(ready_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .is_ok());
        cancel.store(true, Ordering::Release);
        worker.join().unwrap();
        assert_eq!(*late_result.lock().unwrap(), Some(DeliveryOutcome::Stopped));
        assert_eq!(
            teardown_events(&events),
            vec![
                "start",
                "disable",
                "unregister",
                "stop-enter",
                "stop-exit",
                "fence"
            ]
        );
    }

    #[test]
    fn callback_in_flight_is_fenced_before_control_and_lease_release() {
        let device_key = format!("test-callback-fence-{}", std::process::id());
        let lease = camera_leases().acquire(device_key.clone()).unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let (first_tx, first_rx) = mpsc::channel();
        let mut control = TestSessionControl::ready(events.clone(), first_tx);
        let (fence_release_tx, fence_release_rx) = mpsc::channel();
        let (fence_entered_tx, fence_entered_rx) = mpsc::channel();
        control.fence_gate = Some(fence_release_rx);
        control.fence_entered = Some(fence_entered_tx);
        let cancel = Arc::new(AtomicBool::new(false));
        let (runtime_tx, runtime_rx) = mpsc::channel();
        let (ready_rx, done_rx, owner) = spawn_test_owner_with_lease(
            control,
            Arc::clone(&cancel),
            first_rx,
            runtime_rx,
            Arc::new(Mutex::new(None)),
            Duration::from_secs(1),
            Some(lease),
        );
        assert!(ready_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .is_ok());

        let (frame_tx, frame_rx) = mpsc::sync_channel(1);
        let (callback_ready, _callback_ready_rx) = mpsc::channel();
        let callback = CallbackIvars {
            tx: frame_tx,
            ready: callback_ready,
            first_valid: AtomicBool::new(true),
            stop: Arc::clone(&cancel),
            last_frame_ns: AtomicU64::new(0),
            interval_ns: 1,
        };
        let (copy_started_tx, copy_started_rx) = mpsc::channel();
        let (copy_release_tx, copy_release_rx) = mpsc::channel();
        let callback_thread = std::thread::spawn(move || {
            deliver_sample(&callback, 10, || {
                let _ = copy_started_tx.send(());
                let _ = copy_release_rx.recv();
                Some(test_frame())
            })
        });
        copy_started_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        cancel.store(true, Ordering::Release);
        runtime_tx
            .send(RuntimeSignal::SessionInterrupted)
            .unwrap_or(());
        fence_entered_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(done_rx.try_recv().is_err());
        assert!(camera_leases().acquire(device_key.clone()).is_err());
        copy_release_tx.send(()).unwrap();
        assert_eq!(callback_thread.join().unwrap(), DeliveryOutcome::Stopped);
        assert!(frame_rx.try_recv().is_err());
        fence_release_tx.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        owner.join().unwrap();
        assert_eq!(
            teardown_events(&events),
            vec![
                "start",
                "disable",
                "unregister",
                "stop-enter",
                "stop-exit",
                "fence"
            ]
        );
        assert!(camera_leases().acquire(device_key).is_ok());
    }

    #[test]
    fn stalled_owner_teardown_times_out_caller_and_retains_device_lease() {
        let device_key = format!("test-stalled-{}", std::process::id());
        let lease = camera_leases().acquire(device_key.clone()).unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let (gate_tx, gate_rx) = mpsc::channel();
        let (stopped_tx, stopped_rx) = mpsc::channel();
        let (first_tx, first_rx) = mpsc::channel();
        let mut control = TestSessionControl::ready(events, first_tx);
        control.stop_gate = Some(gate_rx);
        control.stopped = Some(stopped_tx);
        let cancel = Arc::new(AtomicBool::new(false));
        let (ready_rx, done_rx, worker) = spawn_test_owner_with_lease(
            control,
            Arc::clone(&cancel),
            first_rx,
            mpsc::channel().1,
            Arc::new(Mutex::new(None)),
            Duration::from_secs(1),
            Some(lease),
        );
        let (frame_tx, frame_rx) = mpsc::sync_channel(1);
        let mut stream = FrameStream::new(
            frame_rx,
            Arc::new(Mutex::new(None)),
            Arc::clone(&cancel),
            worker,
        );
        // Readiness is returned by the owner before it blocks in teardown.
        // A caller with its own deadline remains responsive while the native
        // owner retains the per-device lease.
        assert!(ready_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .is_ok());
        assert!(stream.stop(Duration::from_millis(20)).is_err());
        assert!(camera_leases().acquire(device_key.clone()).is_err());
        let _ = gate_tx.send(());
        assert!(stopped_rx.recv_timeout(Duration::from_secs(1)).is_ok());
        drop(stream);
        drop(frame_tx);
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(camera_leases().acquire(device_key).is_ok());
    }

    #[test]
    fn runtime_failure_after_ready_is_reported_and_runs_owner_cleanup() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (first_tx, first_rx) = mpsc::channel();
        let mut control = TestSessionControl::ready(events.clone(), first_tx);
        let (frame_tx, frame_rx) = mpsc::sync_channel(1);
        control._frame_sink = Some(frame_tx);
        let cancel = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let (runtime_tx, runtime_rx) = mpsc::channel();
        let (ready_rx, worker) = spawn_test_owner(
            control,
            Arc::clone(&cancel),
            first_rx,
            runtime_rx,
            Arc::clone(&error),
            Duration::from_secs(1),
        );
        assert!(ready_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .is_ok());
        runtime_tx.send(RuntimeSignal::SessionRuntimeError).unwrap();
        let stream_error = Arc::clone(&error);
        let mut stream = FrameStream::new(frame_rx, stream_error, Arc::clone(&cancel), worker);
        assert!(matches!(
            stream.next_frame(Duration::from_secs(1)),
            Err(golive_platform::NextError::Failed(PlatformError::Internal(message)))
                if message == "falha durante captura da webcam"
        ));
        let _ = stream.stop(Duration::from_secs(1));
        assert_eq!(
            *error.lock().unwrap(),
            Some(runtime_error(RuntimeSignal::SessionRuntimeError, "9"))
        );
        assert_eq!(
            teardown_events(&events),
            vec![
                "start",
                "disable",
                "unregister",
                "stop-enter",
                "stop-exit",
                "fence"
            ]
        );
    }
}
