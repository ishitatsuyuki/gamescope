//! Atomic DRM/KMS backend.
//!
//! DRM ownership deliberately lives on a dedicated thread.  The Wayland/XWM
//! thread only publishes immutable frame snapshots and consumes page-flip
//! completion messages; it never waits for rendering, an atomic test, or a KMS
//! commit.

use std::{
    collections::HashSet,
    error::Error,
    fs,
    os::fd::OwnedFd,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use gamescope_core::control::{DisplayPowerOperation, RefreshCycleOverride, ScreenType};
use smithay::{
    backend::{
        allocator::{
            Format, Fourcc,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmEventTime, VrrSupport,
            compositor::{FrameError, FrameFlags, PrimaryPlaneElement},
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{EGLContext, EGLDisplay, context::ContextPriority},
        renderer::{
            Color32F, ImportDma,
            element::{
                Kind,
                surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
                utils::RescaleRenderElement,
            },
            gles::GlesRenderer,
            utils::on_commit_buffer_handler,
        },
    },
    output::{Mode as OutputMode, Output, PhysicalProperties},
    reexports::{
        calloop::EventLoop,
        drm::control::{Mode as DrmMode, connector, crtc},
    },
    utils::{DeviceFd, Physical, Size, Transform},
};
use smithay_drm_extras::drm_scanner::DrmScanner;
use tracing::{error, info, warn};

use crate::{CursorLayer, OutputConfig, RenderLayer, State};

const COLOR_FORMATS: [Fourcc; 4] = [
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];

/// Configuration copied into the DRM thread at startup.
pub struct HardwareConfig {
    pub device_path: PathBuf,
    pub device_fd: OwnedFd,
    pub logical_output: OutputConfig,
    pub mode_width: Option<u32>,
    pub mode_height: Option<u32>,
    pub mode_refresh_millihz: Option<i32>,
    pub direct_scanout: bool,
    pub adaptive_sync: bool,
    pub connector_priorities: Vec<String>,
}

/// Physical output and renderer capabilities discovered by the DRM thread.
#[derive(Clone, Debug)]
pub struct HardwareOutputInfo {
    pub connector: String,
    pub display_make: String,
    pub display_model: String,
    pub screen: ScreenType,
    pub width: i32,
    pub height: i32,
    pub refresh_millihz: i32,
    pub device_id: u64,
    pub render_formats: Vec<Format>,
    pub scanout_formats: Vec<Format>,
    pub syncobj_device: DrmDeviceFd,
    pub atomic: bool,
    pub vrr_capable: bool,
    pub vrr_enabled: bool,
    pub valid_refresh_rates_hz: Vec<u32>,
}

/// One immutable compositor snapshot. Only the newest unpublished snapshot is
/// retained, which bounds memory and prevents the protocol thread from waiting
/// behind obsolete frames.
#[derive(Clone, Debug)]
pub struct HardwareFrame {
    pub id: u64,
    pub layers: Vec<RenderLayer>,
    pub cursor: Option<CursorLayer>,
}

/// Events returned to the Wayland thread.
#[derive(Clone, Debug)]
pub enum HardwareEvent {
    Presented {
        frame_id: u64,
        at: Instant,
        monotonic_time: Option<Duration>,
        sequence: u32,
        direct_scanout: bool,
    },
    FrameDeferred {
        frame_id: u64,
    },
    OutputChanged(HardwareOutputInfo),
    OutputDisconnected,
    OutputPowerChanged {
        asleep: bool,
    },
    Error(String),
}

#[derive(Debug)]
struct SharedCommands {
    latest_frame: Mutex<Option<HardwareFrame>>,
    paused: AtomicBool,
    rescan: AtomicBool,
    force_modeset: AtomicBool,
    shutdown: AtomicBool,
    vrr_request: AtomicU8,
    refresh_internal: AtomicU64,
    refresh_external: AtomicU64,
    power_internal: AtomicU8,
    power_external: AtomicU8,
    force_internal: AtomicU8,
    dynamic_refresh_internal: AtomicU64,
    dynamic_refresh_external: AtomicU64,
    composite_force: AtomicU8,
}

impl Default for SharedCommands {
    fn default() -> Self {
        Self {
            latest_frame: Mutex::new(None),
            paused: AtomicBool::new(false),
            rescan: AtomicBool::new(false),
            force_modeset: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            vrr_request: AtomicU8::new(0),
            refresh_internal: AtomicU64::new(0),
            refresh_external: AtomicU64::new(0),
            power_internal: AtomicU8::new(0),
            power_external: AtomicU8::new(0),
            force_internal: AtomicU8::new(0),
            dynamic_refresh_internal: AtomicU64::new(0),
            dynamic_refresh_external: AtomicU64::new(0),
            composite_force: AtomicU8::new(0),
        }
    }
}

const REFRESH_REQUEST_VALID: u64 = 1 << 63;
const REFRESH_ALLOW_SWITCHING: u64 = 1 << 32;
const REFRESH_APPLY_LIMITER: u64 = 1 << 33;

fn refresh_mailbox(shared: &SharedCommands, screen: ScreenType) -> &AtomicU64 {
    match screen {
        ScreenType::Internal => &shared.refresh_internal,
        ScreenType::External => &shared.refresh_external,
    }
}

fn power_mailbox(shared: &SharedCommands, screen: ScreenType) -> &AtomicU8 {
    match screen {
        ScreenType::Internal => &shared.power_internal,
        ScreenType::External => &shared.power_external,
    }
}

fn dynamic_refresh_mailbox(shared: &SharedCommands, screen: ScreenType) -> &AtomicU64 {
    match screen {
        ScreenType::Internal => &shared.dynamic_refresh_internal,
        ScreenType::External => &shared.dynamic_refresh_external,
    }
}

fn pack_refresh(request: RefreshCycleOverride) -> u64 {
    REFRESH_REQUEST_VALID
        | u64::from(request.frames_per_second)
        | if request.allow_refresh_switching {
            REFRESH_ALLOW_SWITCHING
        } else {
            0
        }
        | if request.apply_frame_limiter {
            REFRESH_APPLY_LIMITER
        } else {
            0
        }
}

fn unpack_refresh(screen: ScreenType, packed: u64) -> Option<RefreshCycleOverride> {
    (packed & REFRESH_REQUEST_VALID != 0).then_some(RefreshCycleOverride {
        screen,
        frames_per_second: packed as u32,
        allow_refresh_switching: packed & REFRESH_ALLOW_SWITCHING != 0,
        apply_frame_limiter: packed & REFRESH_APPLY_LIMITER != 0,
    })
}

/// Cloneable, non-blocking control side of the DRM worker.
#[derive(Clone, Debug)]
pub struct HardwareControl {
    shared: Arc<SharedCommands>,
}

impl HardwareControl {
    /// Replace the unpublished frame. Returns the id of an older frame that was
    /// coalesced, if one existed.
    pub fn submit(&self, frame: HardwareFrame) -> Option<u64> {
        self.shared
            .latest_frame
            .lock()
            .expect("DRM frame mailbox poisoned")
            .replace(frame)
            .map(|old| old.id)
    }

    pub fn pause(&self) {
        self.shared.paused.store(true, Ordering::Release);
    }

    pub fn resume(&self) {
        self.shared.paused.store(false, Ordering::Release);
    }

    pub fn rescan(&self) {
        self.shared.rescan.store(true, Ordering::Release);
    }

    pub fn nudge_modeset(&self) {
        self.shared.force_modeset.store(true, Ordering::Release);
        self.rescan();
    }

    pub fn set_vrr(&self, enabled: bool) {
        self.shared
            .vrr_request
            .store(if enabled { 2 } else { 1 }, Ordering::Release);
    }

    pub fn set_refresh_cycle(&self, request: RefreshCycleOverride) {
        refresh_mailbox(&self.shared, request.screen)
            .store(pack_refresh(request), Ordering::Release);
    }

    pub fn set_display_power(&self, operation: DisplayPowerOperation) {
        power_mailbox(&self.shared, operation.screen)
            .store(if operation.sleep { 2 } else { 1 }, Ordering::Release);
    }

    pub fn set_force_internal(&self, force: bool) {
        self.shared
            .force_internal
            .store(if force { 2 } else { 1 }, Ordering::Release);
        self.rescan();
    }

    pub fn set_dynamic_refresh(&self, screen: ScreenType, refresh_hz: u32) {
        dynamic_refresh_mailbox(&self.shared, screen).store(
            REFRESH_REQUEST_VALID | u64::from(refresh_hz),
            Ordering::Release,
        );
    }

    pub fn set_composite_force(&self, force: bool) {
        self.shared
            .composite_force
            .store(if force { 2 } else { 1 }, Ordering::Release);
    }
}

/// Running DRM worker. Dropping it requests shutdown and joins the thread.
pub struct HardwareBackend {
    control: HardwareControl,
    events: Receiver<HardwareEvent>,
    thread: Option<JoinHandle<()>>,
}

impl HardwareBackend {
    /// Initialize DRM, GBM/EGL, a connected connector, and the initial atomic
    /// modeset before returning.
    pub fn spawn(config: HardwareConfig) -> Result<(Self, HardwareOutputInfo), String> {
        let shared = Arc::new(SharedCommands::default());
        let control = HardwareControl {
            shared: Arc::clone(&shared),
        };
        let (events_tx, events) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("gamescope-drm".into())
            .spawn(move || run_worker(config, shared, events_tx, ready_tx))
            .map_err(|error| format!("failed to start DRM thread: {error}"))?;

        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(info)) => Ok((
                Self {
                    control,
                    events,
                    thread: Some(thread),
                },
                info,
            )),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(error) => {
                control.shared.shutdown.store(true, Ordering::Release);
                let _ = thread.join();
                Err(format!("timed out initializing DRM backend: {error}"))
            }
        }
    }

    #[must_use]
    pub fn control(&self) -> HardwareControl {
        self.control.clone()
    }

    pub fn submit(&self, frame: HardwareFrame) -> Option<u64> {
        self.control.submit(frame)
    }

    pub fn try_event(&self) -> Option<HardwareEvent> {
        self.events.try_recv().ok()
    }
}

impl Drop for HardwareBackend {
    fn drop(&mut self) {
        self.control.shared.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct FrameToken {
    id: u64,
    direct_scanout: bool,
}

type OutputSurface = DrmOutput<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    FrameToken,
    DrmDeviceFd,
>;

type OutputManager = DrmOutputManager<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    FrameToken,
    DrmDeviceFd,
>;

type ScaledSurfaceElement = RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>;

struct ActiveOutput {
    crtc: crtc::Handle,
    connector: connector::Handle,
    output: Output,
    drm: OutputSurface,
    info: HardwareOutputInfo,
    modes: Vec<DrmMode>,
    base_mode: DrmMode,
    last_refresh_request: u64,
    last_dynamic_refresh: u64,
    asleep: bool,
}

struct DrmRuntime {
    device_path: PathBuf,
    device_id: u64,
    device_fd: DrmDeviceFd,
    logical_output: OutputConfig,
    mode_width: Option<u32>,
    mode_height: Option<u32>,
    mode_refresh_millihz: Option<i32>,
    direct_scanout: bool,
    composite_force: bool,
    adaptive_sync: bool,
    connector_priorities: Vec<String>,
    force_internal: bool,
    scanner: DrmScanner,
    manager: OutputManager,
    renderer: GlesRenderer,
    // EGLDisplay has to outlive the renderer's context on all EGL drivers.
    _egl_display: EGLDisplay,
    active_output: Option<ActiveOutput>,
    events: Sender<HardwareEvent>,
}

impl DrmRuntime {
    fn new(
        config: HardwareConfig,
        events: Sender<HardwareEvent>,
    ) -> Result<(Self, smithay::backend::drm::DrmDeviceNotifier), Box<dyn Error>> {
        let drm_fd = DrmDeviceFd::new(DeviceFd::from(config.device_fd));
        let (device, notifier) = DrmDevice::new(drm_fd.clone(), true)?;
        if !device.is_atomic() {
            return Err(format!(
                "{} does not support atomic modesetting",
                config.device_path.display()
            )
            .into());
        }

        let gbm = GbmDevice::new(drm_fd.clone())?;
        let (egl_display, renderer) = create_renderer(&gbm)?;
        let render_formats = renderer
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let allocator = GbmAllocator::new(
            gbm.clone(),
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );
        let exporter = GbmFramebufferExporter::new(gbm.clone(), None);
        let device_id = device.device_id();
        let manager = DrmOutputManager::new(
            device,
            allocator,
            exporter,
            Some(gbm),
            COLOR_FORMATS,
            render_formats.iter().copied(),
        );

        let mut runtime = Self {
            device_path: config.device_path,
            device_id,
            device_fd: drm_fd,
            logical_output: config.logical_output,
            mode_width: config.mode_width,
            mode_height: config.mode_height,
            mode_refresh_millihz: config.mode_refresh_millihz,
            direct_scanout: config.direct_scanout,
            composite_force: false,
            adaptive_sync: config.adaptive_sync,
            connector_priorities: config.connector_priorities,
            force_internal: false,
            scanner: DrmScanner::new(),
            manager,
            renderer,
            _egl_display: egl_display,
            active_output: None,
            events,
        };
        runtime.rescan_outputs(false)?;
        if runtime.active_output.is_none() {
            return Err(format!(
                "{} has no connected desktop connector with a usable CRTC",
                runtime.device_path.display()
            )
            .into());
        }
        Ok((runtime, notifier))
    }

    fn output_info(&self) -> HardwareOutputInfo {
        self.active_output
            .as_ref()
            .expect("output checked during DRM startup")
            .info
            .clone()
    }

    fn rescan_outputs(&mut self, force_modeset: bool) -> Result<(), Box<dyn Error>> {
        let _ = self.scanner.scan_connectors(self.manager.device())?;
        let mut candidates = self
            .scanner
            .crtcs()
            .filter(|(connector, _)| connector.state() == connector::State::Connected)
            .filter(|(connector, _)| {
                !self.force_internal
                    || connector_screen_type(connector.interface()) == ScreenType::Internal
            })
            .map(|(connector, crtc)| (connector.clone(), crtc))
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(connector, _)| {
            let name = connector.to_string();
            (connector_priority(&self.connector_priorities, &name), name)
        });
        let selected = candidates.into_iter().next();
        let unchanged = self.active_output.as_ref().is_some_and(|active| {
            selected.as_ref().is_some_and(|(connector, crtc)| {
                active.connector == connector.handle()
                    && active.crtc == *crtc
                    && active.modes == connector.modes()
            })
        });
        if unchanged && !force_modeset {
            return Ok(());
        }

        if let Some(active) = self.active_output.take() {
            let _ = active.drm.with_compositor(|compositor| compositor.clear());
            let _ = self.events.send(HardwareEvent::OutputDisconnected);
        }
        if let Some((connector, crtc)) = selected
            && let Err(error) = self.initialize_output(connector, crtc)
        {
            warn!(%error, ?crtc, "failed to initialize selected DRM output");
        }
        Ok(())
    }

    fn initialize_output(
        &mut self,
        connector: connector::Info,
        crtc: crtc::Handle,
    ) -> Result<(), Box<dyn Error>> {
        let mode = select_mode(
            connector.modes(),
            self.mode_width,
            self.mode_height,
            self.mode_refresh_millihz,
        )
        .ok_or("connector exposes no usable mode")?;
        let output_mode = OutputMode::from(mode);
        let screen = connector_screen_type(connector.interface());
        let connector_name = format!(
            "{}-{}",
            connector.interface().as_str(),
            connector.interface_id()
        );
        let (physical_width, physical_height) = connector.size().unwrap_or((0, 0));
        let output = Output::new(
            connector_name.clone(),
            PhysicalProperties {
                size: (
                    i32::try_from(physical_width).unwrap_or(i32::MAX),
                    i32::try_from(physical_height).unwrap_or(i32::MAX),
                )
                    .into(),
                subpixel: connector.subpixel().into(),
                make: "Unknown".into(),
                model: connector_name.clone(),
            },
        );
        output.set_preferred(output_mode);
        output.change_current_state(
            Some(output_mode),
            Some(Transform::Normal),
            None,
            Some((0, 0).into()),
        );

        let planes = self.manager.device().planes(&crtc)?;
        let render_formats = self.renderer.dmabuf_formats();
        let scanout_formats = planes
            .primary
            .iter()
            .chain(planes.overlay.iter())
            .flat_map(|plane| plane.formats.iter().copied())
            .filter(|format| render_formats.contains(format))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let empty: DrmOutputRenderElements<
            GlesRenderer,
            WaylandSurfaceRenderElement<GlesRenderer>,
        > = DrmOutputRenderElements::default();
        let drm = self.manager.initialize_output(
            crtc,
            mode,
            &[connector.handle()],
            &output,
            Some(planes),
            &mut self.renderer,
            &empty,
        )?;

        let vrr_support = drm
            .with_compositor(|compositor| compositor.vrr_supported(connector.handle()))
            .unwrap_or(VrrSupport::NotSupported);
        let vrr_capable = vrr_support != VrrSupport::NotSupported;
        if self.adaptive_sync && vrr_capable {
            if let Err(error) = drm.with_compositor(|compositor| compositor.use_vrr(true)) {
                warn!(%error, "failed to enable initial VRR; continuing at fixed refresh");
            }
        }
        let vrr_enabled = drm.with_compositor(|compositor| compositor.vrr_enabled());
        let valid_refresh_rates_hz = valid_refresh_rates(connector.modes(), mode, screen);
        let (display_make, display_model) = display_identity(&self.device_path, &connector_name)
            .unwrap_or_else(|| ("Unknown".into(), connector_name.clone()));
        let info = HardwareOutputInfo {
            connector: connector_name.clone(),
            display_make,
            display_model,
            screen,
            width: output_mode.size.w,
            height: output_mode.size.h,
            refresh_millihz: output_mode.refresh,
            device_id: self.device_id,
            render_formats: self.renderer.dmabuf_formats().iter().copied().collect(),
            scanout_formats,
            syncobj_device: self.device_fd.clone(),
            atomic: true,
            vrr_capable,
            vrr_enabled,
            valid_refresh_rates_hz,
        };
        info!(
            connector = info.connector,
            width = info.width,
            height = info.height,
            refresh_millihz = info.refresh_millihz,
            vrr_capable = info.vrr_capable,
            vrr_enabled = info.vrr_enabled,
            "atomic DRM output initialized"
        );
        self.active_output = Some(ActiveOutput {
            crtc,
            connector: connector.handle(),
            output,
            drm,
            info: info.clone(),
            modes: connector.modes().to_vec(),
            base_mode: mode,
            last_refresh_request: 0,
            last_dynamic_refresh: 0,
            asleep: false,
        });
        let _ = self.events.send(HardwareEvent::OutputChanged(info));
        Ok(())
    }

    fn render(&mut self, frame: HardwareFrame) {
        let Some(active) = self.active_output.as_mut() else {
            let _ = self
                .events
                .send(HardwareEvent::FrameDeferred { frame_id: frame.id });
            return;
        };
        if !self.manager.device().is_active() {
            let _ = self
                .events
                .send(HardwareEvent::FrameDeferred { frame_id: frame.id });
            return;
        }
        if active.asleep {
            let _ = self
                .events
                .send(HardwareEvent::FrameDeferred { frame_id: frame.id });
            return;
        }

        let physical_size = active.output.current_mode().map_or_else(
            || Size::from((active.info.width, active.info.height)),
            |mode| mode.size,
        );
        let logical_output = self
            .logical_output
            .resolved_for_output(physical_size.w, physical_size.h);
        let logical_size: Size<i32, Physical> =
            (logical_output.width, logical_output.height).into();
        let scale = (f64::from(physical_size.w) / f64::from(logical_size.w))
            .min(f64::from(physical_size.h) / f64::from(logical_size.h));
        let origin = centered_origin(physical_size, logical_size, scale);
        let mut elements = Vec::<ScaledSurfaceElement>::new();
        if let Some(cursor) = frame.cursor.as_ref() {
            on_commit_buffer_handler::<State>(&cursor.surface);
            let cursor_origin = (
                origin.0 + (cursor.location.x * scale).round() as i32,
                origin.1 + (cursor.location.y * scale).round() as i32,
            );
            elements.extend(
                render_elements_from_surface_tree(
                    &mut self.renderer,
                    &cursor.surface,
                    cursor_origin,
                    1.0,
                    1.0,
                    Kind::Cursor,
                )
                .into_iter()
                .map(|element| {
                    RescaleRenderElement::from_element(element, cursor_origin.into(), scale)
                }),
            );
        }
        for layer in frame.layers.iter().rev() {
            on_commit_buffer_handler::<State>(&layer.surface);
            elements.extend(
                render_elements_from_surface_tree(
                    &mut self.renderer,
                    &layer.surface,
                    origin,
                    1.0,
                    layer.alpha,
                    Kind::Unspecified,
                )
                .into_iter()
                .map(|element| RescaleRenderElement::from_element(element, origin.into(), scale)),
            );
        }

        let flags = if self.direct_scanout && !self.composite_force {
            FrameFlags::DEFAULT
        } else {
            FrameFlags::empty()
        };
        let result = active.drm.render_frame(
            &mut self.renderer,
            &elements,
            Color32F::new(0.0, 0.0, 0.0, 1.0),
            flags,
        );
        let render = match result {
            Ok(render) => render,
            Err(error) => {
                let _ = self.events.send(HardwareEvent::Error(format!(
                    "DRM frame {} preparation failed: {error}",
                    frame.id
                )));
                return;
            }
        };

        let is_empty = render.is_empty;
        let direct_scanout = matches!(render.primary_element, PrimaryPlaneElement::Element(_));
        if render.needs_sync()
            && let PrimaryPlaneElement::Swapchain(ref primary) = render.primary_element
            && let Err(error) = primary.sync.wait()
        {
            let _ = self.events.send(HardwareEvent::Error(format!(
                "DRM frame {} synchronization failed: {error}",
                frame.id
            )));
            return;
        }
        drop(render);

        if is_empty {
            let _ = self
                .events
                .send(HardwareEvent::FrameDeferred { frame_id: frame.id });
            return;
        }

        let token = FrameToken {
            id: frame.id,
            direct_scanout,
        };
        if let Err(error) = active.drm.queue_frame(token)
            && !matches!(error, FrameError::EmptyFrame)
        {
            let _ = self.events.send(HardwareEvent::Error(format!(
                "DRM frame {} atomic commit failed: {error}",
                frame.id
            )));
        }
    }

    fn page_flip(
        &mut self,
        crtc: crtc::Handle,
        metadata: Option<smithay::backend::drm::DrmEventMetadata>,
    ) {
        let Some(active) = self
            .active_output
            .as_mut()
            .filter(|output| output.crtc == crtc)
        else {
            return;
        };
        match active.drm.frame_submitted() {
            Ok(Some(token)) => {
                let _ = self.events.send(HardwareEvent::Presented {
                    frame_id: token.id,
                    at: Instant::now(),
                    monotonic_time: metadata.and_then(|metadata| match metadata.time {
                        DrmEventTime::Monotonic(time) => Some(time),
                        DrmEventTime::Realtime(_) => None,
                    }),
                    sequence: metadata.map_or(0, |metadata| metadata.sequence),
                    direct_scanout: token.direct_scanout,
                });
            }
            Ok(None) => {}
            Err(error) => {
                let _ = self.events.send(HardwareEvent::Error(format!(
                    "DRM page-flip completion failed: {error}"
                )));
            }
        }
    }

    fn pause(&mut self) {
        self.manager.pause();
    }

    fn resume(&mut self) {
        if let Err(error) = self.manager.activate(false) {
            let _ = self.events.send(HardwareEvent::Error(format!(
                "failed to reactivate DRM device: {error}"
            )));
        }
    }

    fn set_vrr(&mut self, enabled: bool) {
        self.adaptive_sync = enabled;
        let Some(active) = self.active_output.as_mut() else {
            return;
        };
        match active
            .drm
            .with_compositor(|compositor| compositor.use_vrr(enabled))
        {
            Ok(()) => {
                active.info.vrr_enabled = active
                    .drm
                    .with_compositor(|compositor| compositor.vrr_enabled());
                let _ = self
                    .events
                    .send(HardwareEvent::OutputChanged(active.info.clone()));
            }
            Err(error) => {
                let _ = self.events.send(HardwareEvent::Error(format!(
                    "failed to set DRM VRR to {enabled}: {error}"
                )));
            }
        }
    }

    fn apply_refresh_request(&mut self, packed: u64) {
        let Some(screen) = self.active_output.as_ref().map(|output| output.info.screen) else {
            return;
        };
        let Some(request) = unpack_refresh(screen, packed) else {
            return;
        };
        let Some(active) = self.active_output.as_mut() else {
            return;
        };
        if active.last_refresh_request == packed {
            return;
        }
        active.last_refresh_request = packed;

        let requested_mode = if request.allow_refresh_switching && request.frames_per_second != 0 {
            active
                .modes
                .iter()
                .copied()
                .filter(|mode| mode.size() == active.base_mode.size())
                .filter(|mode| mode.vrefresh() % request.frames_per_second == 0)
                .max_by_key(DrmMode::vrefresh)
                .unwrap_or(active.base_mode)
        } else {
            active.base_mode
        };
        let current_refresh = active.info.refresh_millihz;
        let requested_output_mode = OutputMode::from(requested_mode);
        if current_refresh == requested_output_mode.refresh {
            return;
        }

        let empty: DrmOutputRenderElements<
            GlesRenderer,
            WaylandSurfaceRenderElement<GlesRenderer>,
        > = DrmOutputRenderElements::default();
        match active
            .drm
            .use_mode(requested_mode, &mut self.renderer, &empty)
        {
            Ok(()) => {
                active
                    .output
                    .change_current_state(Some(requested_output_mode), None, None, None);
                active.info.refresh_millihz = requested_output_mode.refresh;
                let _ = self
                    .events
                    .send(HardwareEvent::OutputChanged(active.info.clone()));
            }
            Err(error) => {
                let _ = self.events.send(HardwareEvent::Error(format!(
                    "failed to switch DRM refresh rate for {} fps: {error}",
                    request.frames_per_second
                )));
            }
        }
    }

    fn apply_dynamic_refresh(&mut self, packed: u64) {
        if packed & REFRESH_REQUEST_VALID == 0 {
            return;
        }
        let Some(active) = self.active_output.as_mut() else {
            return;
        };
        if active.last_dynamic_refresh == packed {
            return;
        }
        active.last_dynamic_refresh = packed;
        let refresh_hz = packed as u32;
        let requested_mode =
            if refresh_hz == 0 {
                active.base_mode
            } else if let Some(mode) = active.modes.iter().copied().find(|mode| {
                mode.size() == active.base_mode.size() && mode.vrefresh() == refresh_hz
            }) {
                mode
            } else {
                let _ = self.events.send(HardwareEvent::Error(format!(
                    "DRM output has no same-resolution {refresh_hz} Hz mode"
                )));
                return;
            };
        let requested_output_mode = OutputMode::from(requested_mode);
        if active.info.refresh_millihz == requested_output_mode.refresh {
            return;
        }
        let empty: DrmOutputRenderElements<
            GlesRenderer,
            WaylandSurfaceRenderElement<GlesRenderer>,
        > = DrmOutputRenderElements::default();
        match active
            .drm
            .use_mode(requested_mode, &mut self.renderer, &empty)
        {
            Ok(()) => {
                active
                    .output
                    .change_current_state(Some(requested_output_mode), None, None, None);
                active.info.refresh_millihz = requested_output_mode.refresh;
                let _ = self
                    .events
                    .send(HardwareEvent::OutputChanged(active.info.clone()));
            }
            Err(error) => {
                let _ = self.events.send(HardwareEvent::Error(format!(
                    "failed to switch DRM output to {refresh_hz} Hz: {error}"
                )));
            }
        }
    }

    fn set_display_power(&mut self, asleep: bool) {
        let Some(active) = self.active_output.as_mut() else {
            return;
        };
        if active.asleep == asleep {
            return;
        }
        if asleep && let Err(error) = active.drm.with_compositor(|compositor| compositor.clear()) {
            let _ = self.events.send(HardwareEvent::Error(format!(
                "failed to power off DRM output: {error}"
            )));
            return;
        }
        active.asleep = asleep;
        let _ = self
            .events
            .send(HardwareEvent::OutputPowerChanged { asleep });
    }
}

fn run_worker(
    config: HardwareConfig,
    shared: Arc<SharedCommands>,
    events: Sender<HardwareEvent>,
    ready: mpsc::SyncSender<Result<HardwareOutputInfo, String>>,
) {
    let mut event_loop = match EventLoop::<DrmRuntime>::try_new() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            let _ = ready.send(Err(format!("failed to create DRM event loop: {error}")));
            return;
        }
    };
    let (mut runtime, notifier) = match DrmRuntime::new(config, events.clone()) {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            return;
        }
    };
    let info = runtime.output_info();
    if let Err(error) = event_loop
        .handle()
        .insert_source(notifier, |event, metadata, runtime| match event {
            DrmEvent::VBlank(crtc) => runtime.page_flip(crtc, *metadata),
            DrmEvent::Error(error) => {
                let _ = runtime
                    .events
                    .send(HardwareEvent::Error(format!("DRM event error: {error}")));
            }
        })
    {
        let _ = ready.send(Err(format!("failed to register DRM event source: {error}")));
        return;
    }
    if ready.send(Ok(info)).is_err() {
        return;
    }

    let mut was_paused = false;
    while !shared.shutdown.load(Ordering::Acquire) {
        if let Err(error) = event_loop.dispatch(Some(Duration::from_millis(2)), &mut runtime) {
            error!(%error, "DRM event loop failed");
            let _ = events.send(HardwareEvent::Error(format!(
                "DRM event loop failed: {error}"
            )));
            break;
        }

        let paused = shared.paused.load(Ordering::Acquire);
        if paused != was_paused {
            if paused {
                runtime.pause();
            } else {
                runtime.resume();
            }
            was_paused = paused;
        }
        let mut rescan = shared.rescan.swap(false, Ordering::AcqRel);
        match shared.force_internal.load(Ordering::Acquire) {
            1 if runtime.force_internal => {
                runtime.force_internal = false;
                rescan = true;
            }
            2 if !runtime.force_internal => {
                runtime.force_internal = true;
                rescan = true;
            }
            _ => {}
        }
        let force_modeset = shared.force_modeset.swap(false, Ordering::AcqRel);
        if force_modeset {
            rescan = true;
        }
        if rescan && let Err(error) = runtime.rescan_outputs(force_modeset) {
            let _ = events.send(HardwareEvent::Error(format!(
                "failed to rescan DRM connectors: {error}"
            )));
        }
        match shared.vrr_request.swap(0, Ordering::AcqRel) {
            1 => runtime.set_vrr(false),
            2 => runtime.set_vrr(true),
            _ => {}
        }
        match shared.composite_force.load(Ordering::Acquire) {
            1 => runtime.composite_force = false,
            2 => runtime.composite_force = true,
            _ => {}
        }
        if let Some(screen) = runtime
            .active_output
            .as_ref()
            .map(|output| output.info.screen)
        {
            let refresh = refresh_mailbox(&shared, screen).load(Ordering::Acquire);
            runtime.apply_refresh_request(refresh);
            let dynamic_refresh = dynamic_refresh_mailbox(&shared, screen).load(Ordering::Acquire);
            runtime.apply_dynamic_refresh(dynamic_refresh);
            match power_mailbox(&shared, screen).load(Ordering::Acquire) {
                1 => runtime.set_display_power(false),
                2 => runtime.set_display_power(true),
                _ => {}
            }
        }
        if !paused
            && let Some(frame) = shared
                .latest_frame
                .lock()
                .expect("DRM frame mailbox poisoned")
                .take()
        {
            runtime.render(frame);
        }
    }
    runtime.pause();
}

#[allow(unsafe_code)]
fn create_renderer(
    gbm: &GbmDevice<DrmDeviceFd>,
) -> Result<(EGLDisplay, GlesRenderer), Box<dyn Error>> {
    // SAFETY: `gbm` owns a valid GBM device for the lifetime of the returned
    // EGL display. EGLDisplay validates the native display before use.
    let display = unsafe { EGLDisplay::new(gbm.clone())? };
    let context = EGLContext::new_with_priority(&display, ContextPriority::High)?;
    // SAFETY: Smithay created and owns `context`; it is current on this worker
    // thread only and the renderer never crosses the thread boundary.
    let renderer = unsafe { GlesRenderer::new(context)? };
    Ok((display, renderer))
}

fn connector_screen_type(interface: connector::Interface) -> ScreenType {
    match interface {
        connector::Interface::EmbeddedDisplayPort
        | connector::Interface::LVDS
        | connector::Interface::DSI => ScreenType::Internal,
        _ => ScreenType::External,
    }
}

fn connector_priority(priorities: &[String], connector: &str) -> usize {
    priorities
        .iter()
        .position(|candidate| candidate == connector)
        .or_else(|| priorities.iter().position(|candidate| candidate == "*"))
        .unwrap_or(priorities.len())
}

fn display_identity(device_path: &std::path::Path, connector: &str) -> Option<(String, String)> {
    let card = device_path.file_name()?.to_str()?;
    let edid = fs::read(format!("/sys/class/drm/{card}-{connector}/edid")).ok()?;
    parse_edid_identity(&edid)
}

fn parse_edid_identity(edid: &[u8]) -> Option<(String, String)> {
    if edid.len() < 128 || edid.get(..8)? != [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00] {
        return None;
    }
    let manufacturer = u16::from_be_bytes([edid[8], edid[9]]);
    let make = [10_u16, 5, 0]
        .map(|shift| char::from_u32(u32::from((manufacturer >> shift) & 0x1f) + 64))
        .into_iter()
        .collect::<Option<String>>()?;
    let model = edid[54..126]
        .chunks_exact(18)
        .find(|descriptor| descriptor[..5] == [0, 0, 0, 0xfc, 0])
        .and_then(|descriptor| std::str::from_utf8(&descriptor[5..18]).ok())
        .map(|name| name.trim_matches(['\0', '\n', '\r', ' ']).to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("{:04x}", u16::from_le_bytes([edid[10], edid[11]])));
    Some((make, model))
}

fn valid_refresh_rates(modes: &[DrmMode], selected: DrmMode, screen: ScreenType) -> Vec<u32> {
    if screen == ScreenType::External {
        return vec![selected.vrefresh()];
    }
    let mut rates = modes
        .iter()
        .filter(|mode| mode.size() == selected.size())
        .map(DrmMode::vrefresh)
        .collect::<Vec<_>>();
    rates.sort_unstable();
    rates.dedup();
    rates
}

fn select_mode(
    modes: &[DrmMode],
    width: Option<u32>,
    height: Option<u32>,
    refresh_millihz: Option<i32>,
) -> Option<DrmMode> {
    let target_refresh_hz =
        refresh_millihz.and_then(|refresh| u32::try_from((refresh + 499) / 1000).ok());
    modes.iter().copied().find(|mode| {
        let (mode_width, mode_height) = mode.size();
        let blocked = mode_width == 4096 && mode_height == 2160;
        !blocked
            && width.is_none_or(|width| u32::from(mode_width) == width)
            && height.is_none_or(|height| u32::from(mode_height) == height)
            && target_refresh_hz.is_none_or(|refresh| mode.vrefresh() == refresh)
    })
}

fn centered_origin(
    target: Size<i32, Physical>,
    source: Size<i32, Physical>,
    scale: f64,
) -> (i32, i32) {
    (
        ((f64::from(target.w) - f64::from(source.w) * scale) / 2.0).round() as i32,
        ((f64::from(target.h) - f64::from(source.h) * scale) / 2.0).round() as i32,
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::Ordering};

    use gamescope_core::control::{RefreshCycleOverride, ScreenType};

    use super::{
        HardwareControl, HardwareFrame, SharedCommands, connector_priority, pack_refresh,
        parse_edid_identity, refresh_mailbox, unpack_refresh,
    };

    fn frame(id: u64) -> HardwareFrame {
        HardwareFrame {
            id,
            layers: Vec::new(),
            cursor: None,
        }
    }

    #[test]
    fn frame_mailbox_is_bounded_and_latest_wins() {
        let shared = Arc::new(SharedCommands::default());
        let control = HardwareControl {
            shared: Arc::clone(&shared),
        };
        assert_eq!(control.submit(frame(1)), None);
        assert_eq!(control.submit(frame(2)), Some(1));
        assert_eq!(
            shared
                .latest_frame
                .lock()
                .expect("mailbox")
                .as_ref()
                .map(|frame| frame.id),
            Some(2)
        );
    }

    #[test]
    fn refresh_requests_remain_isolated_per_screen() {
        let shared = Arc::new(SharedCommands::default());
        let control = HardwareControl {
            shared: Arc::clone(&shared),
        };
        let request = RefreshCycleOverride {
            screen: ScreenType::Internal,
            frames_per_second: 40,
            allow_refresh_switching: true,
            apply_frame_limiter: false,
        };
        control.set_refresh_cycle(request);
        let internal = refresh_mailbox(&shared, ScreenType::Internal).load(Ordering::Acquire);
        let external = refresh_mailbox(&shared, ScreenType::External).load(Ordering::Acquire);
        assert_eq!(
            unpack_refresh(ScreenType::Internal, internal),
            Some(request)
        );
        assert_eq!(external, 0);
        assert_eq!(pack_refresh(request), internal);
    }

    #[test]
    fn connector_priority_matches_gamescope_wildcard_fallback() {
        let priorities = ["DP-2".to_owned(), "*".to_owned(), "HDMI-A-1".to_owned()];
        assert_eq!(connector_priority(&priorities, "DP-2"), 0);
        assert_eq!(connector_priority(&priorities, "DP-1"), 1);
        assert_eq!(connector_priority(&priorities, "HDMI-A-1"), 2);
    }

    #[test]
    fn edid_identity_decodes_pnp_and_display_name() {
        let mut edid = vec![0_u8; 128];
        edid[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
        edid[8..10].copy_from_slice(&0x0443_u16.to_be_bytes());
        edid[54..59].copy_from_slice(&[0, 0, 0, 0xfc, 0]);
        edid[59..69].copy_from_slice(b"Steam Deck");
        assert_eq!(
            parse_edid_identity(&edid),
            Some(("ABC".into(), "Steam Deck".into()))
        );
    }
}
