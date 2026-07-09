use smithay::{
    backend::{
        allocator::{
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Fourcc,
        },
        drm::{
            compositor::FrameFlags,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
            DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmNode, NodeType,
        },
        egl::{context::ContextPriority, EGLDevice, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            damage::OutputDamageTracker,
            element::surface::WaylandSurfaceRenderElement,
            gles::GlesRenderer,
            multigpu::{gbm::GbmGlesBackend, GpuManager, MultiRenderer},
        },
        session::{libseat::LibSeatSession, Event as SessionEvent, Session},
        udev::{self, UdevBackend, UdevEvent},
    },
    desktop::{space::Space, utils::OutputPresentationFeedback, Window},
    output::{Mode as WlMode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{Dispatcher, EventLoop, LoopHandle, RegistrationToken},
        drm::control::{connector, crtc, Device, ModeTypeFlags},
        gbm::Modifier,
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::{backend::GlobalId, DisplayHandle},
    },
    utils::DeviceFd,
};

use std::{collections::HashMap, fmt::Write, mem::transmute, path::Path};

use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use tracing::{debug, error, info, warn, warn_span};

use crate::state::{enki::Enki, State};

type UdevRenderer<'a> = MultiRenderer<'a, 'a, GbmGlesBackend<GlesRenderer, DrmDeviceFd>, GbmGlesBackend<GlesRenderer, DrmDeviceFd>>;

type UdevOutputManager = DrmOutputManager<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, Option<OutputPresentationFeedback>, DrmDeviceFd>;

type UdevDrmOutput = DrmOutput<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, Option<OutputPresentationFeedback>, DrmDeviceFd>;

const SUPPORTED_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];
const SUPPORTED_FORMATS_8BIT_ONLY: &[Fourcc] = &[
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];

// represents a gpu
pub struct DeviceData {
    output_manager: UdevOutputManager,
    drm_scanner: DrmScanner,
    render_node: Option<DrmNode>,
    surfaces: HashMap<crtc::Handle, OutputData>,
    registration_token: RegistrationToken,
}

// represents a monitor
pub struct OutputData {
    output: Output,
    drm_output: UdevDrmOutput,
    global: GlobalId,
}

pub struct UdevData {
    pub(super) session: LibSeatSession,
    input: Libinput,

    gpu_manager: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    primary_gpu: DrmNode,
    devices: HashMap<DrmNode, DeviceData>,
}

impl UdevData {
    pub fn new(event_loop: &EventLoop<State>) -> Result<Self, Box<dyn std::error::Error>> {
        let (session, notifier) = LibSeatSession::new()?;

        let mut lib_input = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
        lib_input.udev_assign_seat(&session.seat()).unwrap();

        let input_backend = LibinputInputBackend::new(lib_input.clone());

        // insert the input sources
        event_loop.handle().insert_source(input_backend, |event, _, state| {
            state.process_input_event(event);
        })?;

        event_loop
            .handle()
            .insert_source(notifier, move |event, _, state| match event {
                // tty switching needs to be handled here.
                SessionEvent::ActivateSession => {
                    info!("Session activated, becomong the DRM master ");

                    state.backend.udev().input.resume().unwrap();

                    for device in state.backend.udev().devices.values_mut() {
                        // idk if this should be truoe or false.
                        if let Err(err) = device.output_manager.device_mut().activate(true) {
                            tracing::error!("Failed to acquire DRM master on activate: field {:?}", err);
                        }
                    }

                    state.enki.space.refresh();
                }
                SessionEvent::PauseSession => {
                    state.backend.udev().input.suspend();
                }
            })
            .unwrap();

        let api = GbmGlesBackend::with_context_priority(smithay::backend::egl::context::ContextPriority::High);

        let gpu_manager = GpuManager::new(api)?;

        let (primary_gpu, primary_render_node) = {
            let primary_gpu_path = udev::primary_gpu(&session.seat())?.unwrap();
            let primary_node = DrmNode::from_path(&primary_gpu_path)?;
            let primary_render_node = primary_node.node_with_type(smithay::backend::drm::NodeType::Render).and_then(Result::ok).unwrap_or_else(|| {
                warn_span!("error getting the render node for the primary GPU; proceeding anyway");
                primary_node
            });

            (primary_node, primary_render_node)
        };

        let mut node_path = String::new();
        if let Some(path) = primary_render_node.dev_path() {
            write!(node_path, "{path:?}").unwrap();
        } else {
            write!(node_path, "{primary_render_node}").unwrap();
        }

        info!("using as the render node: {node_path}");

        Ok(Self {
            session,
            input: lib_input,
            gpu_manager,
            primary_gpu,
            devices: HashMap::new(),
        })
    }

    pub fn init(&mut self, event_loop: &EventLoop<State>, enki: &mut Enki) -> Result<(), Box<dyn std::error::Error>> {
        let udev_backend = UdevBackend::new(self.session.seat())?;

        for (device_id, path) in udev_backend.device_list() {
            let Ok(node) = DrmNode::from_dev_id(device_id) else {
                continue;
            };
            if let Err(err) = self.device_added(node, path, enki) {
                warn!("failed to add device {node}: {err}");
                continue;
            }
            self.device_changed(node, enki);
        }

        event_loop.handle().insert_source(udev_backend, move |event, _, state| match event {
            UdevEvent::Added {
                device_id,
                path,
            } => {
                let Ok(node) = DrmNode::from_dev_id(device_id) else {
                    return;
                };
                let backend = state.backend.udev();
                if let Err(err) = backend.device_added(node, &path, &mut state.enki) {
                    warn!("failed to add device {node}: {err}");
                    return;
                }
                backend.device_changed(node, &mut state.enki);
            }
            UdevEvent::Changed { device_id } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    state.backend.udev().device_changed(node, &mut state.enki);
                }
            }
            UdevEvent::Removed { device_id } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    state.backend.udev().device_removed(node, &mut state.enki);
                }
            }
        })?;

        Ok(())
    }
}

// these are all udev event functions
impl UdevData {
    fn device_added(&mut self, node: DrmNode, path: &Path, enki: &mut Enki) -> Result<(), Box<dyn std::error::Error>> {
        // Try to open the device
        let fd = self.session.open(path, OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK)?;

        let fd = DrmDeviceFd::new(DeviceFd::from(fd));

        let (drm, notifier) = DrmDevice::new(fd.clone(), true)?;
        let gbm = GbmDevice::new(fd)?;

        let registration_token = enki
            .loop_handle
            .insert_source(notifier, move |event, metadata, data: &mut State| match event {
                DrmEvent::VBlank(crtc) => {
                    data.backend.udev().on_vblank(node, crtc, metadata, &data.enki.space);
                }
                DrmEvent::Error(error) => {
                    error!("{:?}", error);
                }
            })
            .unwrap();

        let try_initialize_gpu = || -> Result<DrmNode, Box<dyn std::error::Error>> {
            let display = unsafe { EGLDisplay::new(gbm.clone())? };
            let egl_device = EGLDevice::device_for_display(&display)?;
            if egl_device.is_software() {
                return Err("refusing to use a software EGL device as a render node".into());
            }
            Ok(egl_device.try_get_render_node().ok().flatten().unwrap_or(node))
        };

        let render_node = try_initialize_gpu()
            .inspect_err(|err| {
                warn!(?err, "failed to initialize gpu");
            })
            .ok();

        let allocator = render_node
            .is_some()
            .then(|| GbmAllocator::new(gbm.clone(), GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT))
            .or_else(|| {
                self.devices
                    .get(&self.primary_gpu)
                    .or_else(|| self.devices.values().find(|backend| backend.render_node == Some(self.primary_gpu)))
                    .map(|backend| backend.output_manager.allocator().clone())
            });

        let framebuffer_exporter = GbmFramebufferExporter::new(gbm.clone(), render_node.into());

        let color_formats = if std::env::var("ANVIL_DISABLE_10BIT").is_ok() {
            SUPPORTED_FORMATS_8BIT_ONLY
        } else {
            SUPPORTED_FORMATS
        };

        let mut renderer = self.gpu_manager.single_renderer(&render_node.unwrap_or(self.primary_gpu)).unwrap();

        let render_formats = renderer
            .as_mut()
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .filter(|format| render_node.is_some() || format.modifier == Modifier::Linear)
            .copied()
            .collect::<FormatSet>();

        let drm_output_manager = DrmOutputManager::new(drm, allocator.unwrap(), framebuffer_exporter, Some(gbm), color_formats.iter().copied(), render_formats);

        self.devices.insert(
            node,
            DeviceData {
                registration_token,
                output_manager: drm_output_manager,
                render_node: render_node,
                drm_scanner: DrmScanner::new(),
                surfaces: HashMap::new(),
            },
        );

        self.device_changed(node, enki);

        Ok(())
    }

    fn device_changed(&mut self, node: DrmNode, enki: &mut Enki) {
        let Some(device) = self.devices.get_mut(&node) else {
            return;
        };

        let scan_result = match device.drm_scanner.scan_connectors(device.output_manager.device()) {
            Ok(result) => result,
            Err(err) => {
                warn!("failed to scan connectors: {err}");
                return;
            }
        };

        for event in scan_result {
            match event {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => {
                    self.connector_connected(node, connector, crtc, enki);
                    self.render_surface(node, crtc, &enki.space);
                }
                DrmScanEvent::Disconnected {
                    connector,
                    crtc: Some(crtc),
                } => {
                    self.connector_disconnected(node, connector, crtc, enki);
                }
                _ => {}
            }
        }
    }

    fn device_removed(&mut self, node: DrmNode, enki: &mut Enki) {
        let Some(device) = self.devices.remove(&node) else {
            return;
        };

        for (_, surface) in device.surfaces {
            enki.space.unmap_output(&surface.output);
        }

        enki.loop_handle.remove(device.registration_token);

        if let Some(render_node) = device.render_node {
            self.gpu_manager.as_mut().remove_node(&render_node);
        }
    }

    fn connector_connected(&mut self, node: DrmNode, connector: connector::Info, crtc: crtc::Handle, enki: &mut Enki) {
        let Some(device) = self.devices.get_mut(&node) else {
            return;
        };

        let mode_id = connector.modes().iter().position(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED)).unwrap_or(0);
        let Some(drm_mode) = connector.modes().get(mode_id).copied() else {
            warn!("connector {:?} has no modes", connector.handle());
            return;
        };
        let wl_mode = WlMode::from(drm_mode);

        let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
        let output = Output::new(
            format!("{}-{}", connector.interface().as_str(), connector.interface_id()),
            PhysicalProperties {
                size: (phys_w as i32, phys_h as i32).into(),
                subpixel: connector.subpixel().into(),
                make: "Unknown".into(),
                model: "Unknown".into(),
                serial_number: "Unknown".into(),
            },
        );
        let global = output.create_global::<State>(&enki.display_handle);

        let x = enki.space.outputs().fold(0, |acc, o| acc + enki.space.output_geometry(o).unwrap().size.w);

        output.set_preferred(wl_mode);
        output.change_current_state(Some(wl_mode), None, None, Some((x, 0).into()));
        enki.space.map_output(&output, (x, 0));

        let render_node = device.render_node.unwrap_or(self.primary_gpu);
        let mut renderer = match self.gpu_manager.single_renderer(&render_node) {
            Ok(renderer) => renderer,
            Err(err) => {
                warn!("failed to get renderer: {err}");
                return;
            }
        };

        let drm_output = match device.output_manager.lock().initialize_output::<_, WaylandSurfaceRenderElement<UdevRenderer<'_>>>(
            crtc,
            drm_mode,
            &[connector.handle()],
            &output,
            None,
            &mut renderer,
            &DrmOutputRenderElements::default(),
        ) {
            Ok(drm_output) => drm_output,
            Err(err) => {
                warn!("failed to initialize drm output: {err}");
                return;
            }
        };

        device.surfaces.insert(
            crtc,
            OutputData {
                output,
                global,
                drm_output,
            },
        );
    }

    fn connector_disconnected(&mut self, node: DrmNode, _connector: connector::Info, crtc: crtc::Handle, enki: &mut Enki) {
        let Some(device) = self.devices.get_mut(&node) else {
            return;
        };
        if let Some(surface) = device.surfaces.remove(&crtc) {
            enki.space.unmap_output(&surface.output);
            enki.display_handle.remove_global::<State>(surface.global);
        }
    }
}

impl UdevData {
    fn render_surface(&mut self, node: DrmNode, crtc: crtc::Handle, space: &Space<Window>) {
        let Some(device) = self.devices.get_mut(&node) else {
            return;
        };
        let Some(surface) = device.surfaces.get_mut(&crtc) else {
            return;
        };

        let render_node = device.render_node.unwrap_or(self.primary_gpu);
        let mut renderer = match self.gpu_manager.single_renderer(&render_node) {
            Ok(renderer) => renderer,
            Err(err) => {
                warn!("failed to get renderer: {err}");
                return;
            }
        };

        let elements = match smithay::desktop::space::space_render_elements::<_, Window, _>(&mut renderer, [space], &surface.output, 1.0) {
            Ok(elements) => elements,
            Err(err) => {
                warn!("failed to collect render elements: {err:?}");
                return;
            }
        };

        match surface.drm_output.render_frame(&mut renderer, &elements, [0.1, 0.1, 0.1, 1.0], FrameFlags::DEFAULT) {
            Ok(result) => {
                if !result.is_empty {
                    if let Err(err) = surface.drm_output.queue_frame(None) {
                        warn!("failed to queue frame: {err:?}");
                    }
                }
            }
            Err(err) => warn!("render error: {err:?}"),
        }
    }

    fn on_vblank(&mut self, node: DrmNode, crtc: crtc::Handle, _metadata: &mut Option<DrmEventMetadata>, space: &Space<Window>) {
        if let Some(device) = self.devices.get_mut(&node) {
            if let Some(surface) = device.surfaces.get_mut(&crtc) {
                if let Err(err) = surface.drm_output.frame_submitted() {
                    warn!("frame_submitted error: {err:?}");
                }
            }
        }

        self.render_surface(node, crtc, space);
    }
}
