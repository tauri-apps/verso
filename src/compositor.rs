use std::cell::Cell;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::rendering::RenderingContext;
use crate::screenshot::ScreenshotTaker;
use crate::touch::{TouchAction, TouchHandler};
use crate::window::Window;
use base::Epoch;
use base::cross_process_instant::CrossProcessInstant;
use base::generic_channel::RoutedReceiver;
use base::id::{PipelineId, WebViewId};
use bitflags::bitflags;
use compositing_traits::display_list::{CompositorDisplayListInfo, ScrollTree, ScrollType};
use compositing_traits::{
    CompositionPipeline, CompositorMsg, CompositorProxy, ImageUpdate, PipelineExitSource,
    SendableFrameTree,
};
use constellation_traits::{EmbedderToConstellationMessage, PaintMetricEvent, WindowSizeType};
use crossbeam_channel::Sender;
use dpi::PhysicalSize;
use embedder_traits::{
    AnimationState, CompositorHitTestResult, InputEvent, MouseButton, MouseButtonAction,
    MouseButtonEvent, MouseMoveEvent, ScreenshotCaptureError, TouchEvent, TouchEventType,
    ViewportDetails,
};
use euclid::{Point2D, Scale, Size2D, Transform3D, Vector2D, vec2};
use gleam::gl;
use image::RgbaImage;
use ipc_channel::ipc::{self, IpcSharedMemory};
use log::{debug, trace, warn};
use profile_traits::mem::{ProcessReports, Report, ReportKind};
use profile_traits::time::{self as profile_time, ProfilerCategory};
use profile_traits::{mem, path, time, time_profile};
use rustc_hash::FxHashMap;
use servo_config::pref;
use servo_geometry::DeviceIndependentPixel;
use style_traits::CSSPixel;
use webrender::{RenderApi, Transaction};
use webrender_api::units::{
    DeviceIntPoint, DeviceIntRect, DevicePixel, DevicePoint, DeviceRect, DeviceSize, LayoutPoint,
    LayoutRect, LayoutSize, LayoutVector2D, WorldPoint,
};
use webrender_api::{
    BorderRadius, BoxShadowClipMode, BuiltDisplayList, ClipMode, ColorF, CommonItemProperties,
    ComplexClipRegion, DirtyRect, DisplayListPayload, DocumentId, Epoch as WebRenderEpoch,
    ExternalScrollId, FontInstanceFlags, FontInstanceKey, FontInstanceOptions, FontKey,
    FontVariation, HitTestFlags, ImageKey, PipelineId as WebRenderPipelineId, PropertyBinding,
    ReferenceFrameKind, RenderReasons, SampledScrollOffset, ScrollLocation, SpaceAndClipInfo,
    SpatialId, SpatialTreeItemKey, TransformStyle,
};
use winit::window::WindowId;

/// Data used to construct a compositor.
pub struct InitialCompositorState {
    /// A channel to the compositor.
    pub sender: CompositorProxy,
    /// A port on which messages inbound to the compositor can be received.
    pub receiver: RoutedReceiver<CompositorMsg>,
    /// A channel to the constellation.
    pub constellation_chan: Sender<EmbedderToConstellationMessage>,
    /// A channel to the time profiler thread.
    pub time_profiler_chan: time::ProfilerChan,
    /// A channel to the memory profiler thread.
    pub mem_profiler_chan: mem::ProfilerChan,
    /// Instance of webrender API
    pub webrender: webrender::Renderer,
    /// Webrender document ID
    pub webrender_document: DocumentId,
    /// Webrender API
    pub webrender_api: RenderApi,
    /// Servo's rendering context
    pub rendering_context: RenderingContext,
    /// Webrender GL handle
    pub webrender_gl: Rc<dyn gl::Gl>,
}

/// Various debug and profiling flags that WebRender supports.
#[derive(Clone)]
pub enum WebRenderDebugOption {
    /// Set profiler flags to webrender.
    Profiler,
    /// Set texture cache flags to webrender.
    TextureCacheDebug,
    /// Set render target flags to webrender.
    RenderTargetDebug,
}

/// Mouse event for the compositor.
#[derive(Clone)]
pub enum MouseWindowEvent {
    /// Mouse click event
    Click(MouseButton, DevicePoint),
    /// Mouse down event
    MouseDown(MouseButton, DevicePoint),
    /// Mouse up event
    MouseUp(MouseButton, DevicePoint),
}

// NB: Never block on the Constellation, because sometimes the Constellation blocks on us.
/// The Verso compositor contains a GL rendering context with a WebRender instance.
/// The compositor will communicate with Servo using messages from the Constellation,
/// then composite the WebRender frames and present the surface to the window.
pub struct IOCompositor {
    /// The current window that Compositor is handling.
    pub current_window: WindowId,

    /// Size of current viewport that Compositor is handling.
    viewport: DeviceSize,

    /// The pixel density of the display.
    scale_factor: Scale<f32, DeviceIndependentPixel, DevicePixel>,

    /// The active webrender document.
    webrender_document: DocumentId,

    /// The port on which we receive messages.
    pub(crate) compositor_receiver: RoutedReceiver<CompositorMsg>,

    /// Tracks each webview and its current pipeline
    pub(crate) webviews: HashMap<WebViewId, PipelineId>,

    /// Tracks details about each active pipeline that the compositor knows about.
    pub(crate) pipeline_details: HashMap<PipelineId, PipelineDetails>,

    /// Tracks whether or not the view needs to be repainted.
    needs_repaint: Cell<RepaintReason>,

    /// Tracks whether we are in the process of shutting down, or have shut down and should close
    /// the compositor.
    pub shutdown_state: ShutdownState,

    /// The current frame tree ID (used to reject old paint buffers)
    frame_tree_id: FrameTreeId,

    /// The channel on which messages can be sent to the constellation.
    pub constellation_sender: Sender<EmbedderToConstellationMessage>,

    /// The channel on which messages can be sent to the time profiler.
    time_profiler_chan: profile_time::ProfilerChan,

    /// Touch input state machine
    touch_handler: TouchHandler,

    /// Pending scroll/zoom events.
    pending_scroll_zoom_events: Vec<ScrollZoomEvent>,

    /// The webrender renderer.
    webrender: Option<webrender::Renderer>,

    /// The webrender interface, if enabled.
    pub webrender_api: RenderApi,

    /// The glutin instance that webrender targets
    pub rendering_context: RenderingContext,

    /// The GL bindings for webrender
    webrender_gl: Rc<dyn gl::Gl>,

    /// The last position in the rendered view that the mouse moved over. This becomes `None`
    /// when the mouse leaves the rendered view.
    last_mouse_move_position: Option<DevicePoint>,

    /// A [`FrameDelayer`] which is used to wait for canvas image updates to
    /// arrive before requesting a new frame, as these happen asynchronously with
    /// `ScriptThread` display list construction.
    frame_delayer: FrameDelayer,

    /// The number of frames pending to receive from WebRender.
    pending_frames: usize,

    /// A [`ScreenshotTaker`] responsible for handling all screenshot requests.
    screenshot_taker: ScreenshotTaker,

    /// The [`Instant`] of the last animation tick, used to avoid flooding the Constellation and
    /// ScriptThread with a deluge of animation ticks.
    last_animation_tick: Instant,

    /// Whether the application is currently animating.
    /// Typically, when animations are active, the window
    /// will want to avoid blocking on UI events, and just
    /// run the event loop at the vsync interval.
    pub is_animating: bool,
}

#[derive(Clone, Copy)]
struct ScrollEvent {
    /// Scroll by this offset, or to Start or End
    scroll_location: ScrollLocation,
    /// Apply changes to the frame at this location
    cursor: DeviceIntPoint,
    /// The number of OS events that have been coalesced together into this one event.
    event_count: u32,
}

#[derive(Clone, Copy)]
enum ScrollZoomEvent {
    /// A pinch zoom event that magnifies the view by the given factor.
    PinchZoom(f32),
    /// A zoom event that magnifies the view by the factor parsed from meta tag.
    ViewportZoom(f32),
    /// A scroll event that scrolls the scroll node at the given location by the
    /// given amount.
    Scroll(ScrollEvent),
}

#[derive(Clone, Debug)]
pub(crate) struct ScrollResult {
    pub hit_test_result: CompositorHitTestResult,
    pub external_scroll_id: ExternalScrollId,
    pub offset: LayoutVector2D,
}

/// Why we need to be repainted. This is used for debugging.
#[derive(Clone, Copy, Default)]
pub(crate) struct RepaintReason(u8);

bitflags! {
    impl RepaintReason: u8 {
        /// We're performing the single repaint in headless mode.
        const ReadyForScreenshot = 1 << 0;
        /// We're performing a repaint to run an animation.
        const ChangedAnimationState = 1 << 1;
        /// A new WebRender frame has arrived.
        const NewWebRenderFrame = 1 << 2;
        /// The window has been resized and will need to be synchronously repainted.
        const Resize = 1 << 3;
    }
}

/// Shutdown State of the compositor
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ShutdownState {
    /// Compositor is still running.
    NotShuttingDown,
    /// Compositor is shutting down.
    ShuttingDown,
    /// Compositor has shut down.
    FinishedShuttingDown,
}

/// The paint status of a particular pipeline in the Servo renderer. This is used to trigger metrics
/// in script (via the constellation) when display lists are received.
///
/// See <https://w3c.github.io/paint-timing/#first-contentful-paint>.
#[derive(PartialEq)]
pub(crate) enum PaintMetricState {
    /// The renderer is still waiting to process a display list which triggers this metric.
    Waiting,
    /// The renderer has processed the display list which will trigger this event, marked the Servo
    /// instance ready to paint, and is waiting for the given epoch to actually be rendered.
    Seen(WebRenderEpoch, bool /* first_reflow */),
    /// The metric has been sent to the constellation and no more work needs to be done.
    Sent,
}

struct PipelineDetails {
    /// The pipeline associated with this PipelineDetails object.
    pipeline: Option<CompositionPipeline>,

    /// The id of the parent pipeline, if any.
    parent_pipeline_id: Option<PipelineId>,

    /// Whether animations are running
    animations_running: bool,

    /// Whether there are animation callbacks
    animation_callbacks_running: bool,

    /// Whether to use less resources by stopping animations.
    throttled: bool,

    /// The compositor-side [ScrollTree]. This is used to allow finding and scrolling
    /// nodes in the compositor before forwarding new offsets to WebRender.
    scroll_tree: ScrollTree,

    /// The paint metric status of the first paint.
    pub first_paint_metric: PaintMetricState,

    /// The paint metric status of the first contentful paint.
    pub first_contentful_paint_metric: PaintMetricState,

    /// Which parts of Servo have reported that this `Pipeline` has exited. Only when allAdd commentMore actions
    /// have done so will it be discarded.
    pub exited: PipelineExitSource,

    /// The [`Epoch`] of the latest display list received for this `Pipeline` or `None` if no
    /// display list has been received.
    pub display_list_epoch: Option<Epoch>,
}

impl PipelineDetails {
    fn new() -> PipelineDetails {
        PipelineDetails {
            pipeline: None,
            parent_pipeline_id: None,
            animations_running: false,
            animation_callbacks_running: false,
            throttled: false,
            scroll_tree: ScrollTree::default(),
            first_paint_metric: PaintMetricState::Waiting,
            first_contentful_paint_metric: PaintMetricState::Waiting,
            exited: PipelineExitSource::empty(),
            display_list_epoch: None,
        }
    }

    fn install_new_scroll_tree(&mut self, new_scroll_tree: ScrollTree) {
        let old_scroll_offsets = self.scroll_tree.scroll_offsets();
        self.scroll_tree = new_scroll_tree;
        self.scroll_tree.set_all_scroll_offsets(&old_scroll_offsets);
    }

    pub(crate) fn animation_callbacks_running(&self) -> bool {
        self.animation_callbacks_running
    }

    pub(crate) fn animating(&self) -> bool {
        !self.throttled && (self.animation_callbacks_running || self.animations_running)
    }
}

impl IOCompositor {
    /// Create a new compositor.
    pub fn new(
        current_window: WindowId,
        viewport: DeviceSize,
        scale_factor: Scale<f32, DeviceIndependentPixel, DevicePixel>,
        state: InitialCompositorState,
        wait_for_stable_image: bool,
    ) -> Self {
        let compositor = IOCompositor {
            current_window,
            viewport,
            compositor_receiver: state.receiver,
            webviews: HashMap::new(),
            pipeline_details: HashMap::new(),
            scale_factor,
            needs_repaint: Cell::default(),
            touch_handler: TouchHandler::new(),
            pending_scroll_zoom_events: Vec::new(),
            shutdown_state: ShutdownState::NotShuttingDown,
            frame_tree_id: FrameTreeId(0),
            constellation_sender: state.constellation_chan,
            time_profiler_chan: state.time_profiler_chan,
            webrender: Some(state.webrender),
            webrender_document: state.webrender_document,
            webrender_api: state.webrender_api,
            rendering_context: state.rendering_context,
            webrender_gl: state.webrender_gl,
            last_mouse_move_position: None,
            frame_delayer: FrameDelayer::default(),
            pending_frames: 0,
            screenshot_taker: ScreenshotTaker::default(),
            last_animation_tick: Instant::now(),
            is_animating: false,
        };

        // Make sure the GL state is OK
        compositor.assert_gl_framebuffer_complete();
        compositor
    }

    /// Consume compositor itself and deinit webrender.
    pub fn deinit(&mut self) {
        if let Some(webrender) = self.webrender.take() {
            webrender.deinit();
        }
    }

    pub(crate) fn rendering_context(
        &self,
    ) -> &dyn compositing_traits::rendering_context::RenderingContext {
        &*self.rendering_context
    }

    pub(crate) fn set_needs_repaint(&self, reason: RepaintReason) {
        let mut needs_repaint = self.needs_repaint.get();
        needs_repaint.insert(reason);
        self.needs_repaint.set(needs_repaint);
    }

    /// Whether or not the view needs to be repainted.
    pub fn needs_repaint(&self) -> bool {
        !self.needs_repaint.get().is_empty()
    }

    /// Get the current size of the rendering context.
    pub fn rendering_context_size(&self) -> Size2D<u32, DevicePixel> {
        self.rendering_context.size2d()
    }

    /// Tell compositor to start shutting down.
    pub fn start_shutting_down(&mut self) {
        if self.shutdown_state != ShutdownState::NotShuttingDown {
            warn!("Requested shutdown while already shutting down");
            return;
        }

        debug!("Compositor sending Exit message to Constellation");
        if let Err(e) = self
            .constellation_sender
            .send(EmbedderToConstellationMessage::Exit)
        {
            warn!("Sending exit message to constellation failed ({:?}).", e);
        }

        self.shutdown_state = ShutdownState::ShuttingDown;
    }

    /// Finish shutting down when we receive `EmbedderMsg::ShutdownComplete`
    pub fn finish_shutting_down(&mut self) {
        debug!("Compositor received message that constellation shutdown is complete");

        // Drain compositor port, sometimes messages contain channels that are blocking
        // another thread from finishing (i.e. SetFrameTree).
        while self.compositor_receiver.try_recv().is_ok() {}

        // Tell the profiler, memory profiler, and scrolling timer to shut down.
        if let Ok((sender, receiver)) = ipc::channel() {
            self.time_profiler_chan
                .send(profile_time::ProfilerMsg::Exit(sender));
            let _ = receiver.recv();
        }

        self.shutdown_state = ShutdownState::FinishedShuttingDown;
    }

    fn handle_browser_message(
        &mut self,
        msg: CompositorMsg,
        windows: &mut HashMap<WindowId, (Window, DocumentId)>,
    ) {
        match self.shutdown_state {
            ShutdownState::NotShuttingDown => {}
            ShutdownState::ShuttingDown => {
                return self.handle_browser_message_while_shutting_down(msg);
            }
            ShutdownState::FinishedShuttingDown => {
                // Messages to the compositor are ignored after shutdown is complete.
                return;
            }
        }

        match msg {
            CompositorMsg::CollectMemoryReport(sender) => {
                let ops =
                    wr_malloc_size_of::MallocSizeOfOps::new(servo_allocator::usable_size, None);
                let report = self.webrender_api.report_memory(ops);
                let reports = vec![
                    Report {
                        path: path!["webrender", "fonts"],
                        kind: ReportKind::ExplicitJemallocHeapSize,
                        size: report.fonts,
                    },
                    Report {
                        path: path!["webrender", "images"],
                        kind: ReportKind::ExplicitJemallocHeapSize,
                        size: report.images,
                    },
                    Report {
                        path: path!["webrender", "display-list"],
                        kind: ReportKind::ExplicitJemallocHeapSize,
                        size: report.display_list,
                    },
                ];
                sender.send(ProcessReports::new(reports));
            }

            CompositorMsg::ChangeRunningAnimationsState(
                _webview_id,
                pipeline_id,
                animation_state,
            ) => {
                if self.change_pipeline_running_animations_state(pipeline_id, animation_state) {
                    // These operations should eventually happen per-WebView, but they are
                    // global now as rendering is still global to all WebViews.
                    self.process_animations(true);
                }
            }

            CompositorMsg::CreateOrUpdateWebView(frame_tree) => {
                self.create_or_update_webview(&frame_tree, windows);
                self.send_scroll_positions_to_layout_for_pipeline(frame_tree.pipeline.id);
            }

            CompositorMsg::RemoveWebView(webview_id) => {
                self.remove_webview(webview_id, windows);
            }

            CompositorMsg::TouchEventProcessed(_webview_id, result) => {
                self.touch_handler.on_event_processed(result);
            }

            CompositorMsg::SetThrottled(_webview_id, pipeline_id, throttled) => {
                if self.set_throttled(pipeline_id, throttled) {
                    self.process_animations(true);
                }
            }

            CompositorMsg::PipelineExited(_webview_id, pipeline_id, pipeline_exit_source) => {
                debug!("Compositor got pipeline exited: {:?}", pipeline_id);
                self.pipeline_exited(pipeline_id, pipeline_exit_source);
            }

            CompositorMsg::NewWebRenderFrameReady(_document_id, recomposite_needed) => {
                unreachable!("New WebRender frames should be handled in the caller.");
            }

            CompositorMsg::SendInitialTransaction(webview_id, pipeline_id) => {
                let Some(pipeline_details) = self.pipeline_details.get_mut(&pipeline_id.into())
                else {
                    return warn!("Could not find pipeline details for incoming display list");
                };

                let starting_epoch = Epoch(0);
                pipeline_details.display_list_epoch = Some(starting_epoch);

                let mut txn = Transaction::new();
                txn.set_display_list(starting_epoch.into(), (pipeline_id, Default::default()));
                self.generate_frame(&mut txn, RenderReasons::SCENE);
                self.send_transaction(txn);
            }

            CompositorMsg::SendScrollNode(_webview_id, pipeline_id, offset, external_scroll_id) => {
                let pipeline_id = pipeline_id.into();
                let Some(pipeline_details) = self.pipeline_details.get_mut(&pipeline_id) else {
                    return;
                };

                let Some(offset) = pipeline_details
                    .scroll_tree
                    .set_scroll_offset_for_node_with_external_scroll_id(
                        external_scroll_id,
                        offset,
                        ScrollType::Script,
                    )
                else {
                    // The renderer should be fully up-to-date with script at this point and script
                    // should never try to scroll to an invalid location.
                    warn!("Could not scroll node with id: {external_scroll_id:?}");
                    return;
                };

                let mut txn = Transaction::new();
                txn.set_scroll_offsets(
                    external_scroll_id,
                    vec![SampledScrollOffset {
                        offset: -offset,
                        generation: 0,
                    }],
                );
                self.generate_frame(&mut txn, RenderReasons::APZ);
                self.send_transaction(txn);
            }

            CompositorMsg::SendDisplayList {
                webview_id: _,
                display_list_descriptor,
                display_list_receiver,
            } => {
                // This must match the order from the sender, currently in `shared/script/lib.rs`.
                let display_list_info = match display_list_receiver.recv() {
                    Ok(display_list_info) => display_list_info,
                    Err(error) => {
                        return warn!("Could not receive display list info: {error}");
                    }
                };
                let display_list_info: CompositorDisplayListInfo =
                    match bincode::deserialize(&display_list_info) {
                        Ok(display_list_info) => display_list_info,
                        Err(error) => {
                            return warn!("Could not deserialize display list info: {error}");
                        }
                    };
                let items_data = match display_list_receiver.recv() {
                    Ok(display_list_data) => display_list_data,
                    Err(error) => {
                        return warn!(
                            "Could not receive WebRender display list items data: {error}"
                        );
                    }
                };
                let cache_data = match display_list_receiver.recv() {
                    Ok(display_list_data) => display_list_data,
                    Err(error) => {
                        return warn!(
                            "Could not receive WebRender display list cache data: {error}"
                        );
                    }
                };
                let spatial_tree = match display_list_receiver.recv() {
                    Ok(display_list_data) => display_list_data,
                    Err(error) => {
                        return warn!(
                            "Could not receive WebRender display list spatial tree: {error}."
                        );
                    }
                };
                let built_display_list = BuiltDisplayList::from_data(
                    DisplayListPayload {
                        items_data,
                        cache_data,
                        spatial_tree,
                    },
                    display_list_descriptor,
                );

                let pipeline_id = display_list_info.pipeline_id;
                let details = self.pipeline_details(pipeline_id.into());
                details.install_new_scroll_tree(display_list_info.scroll_tree);

                let epoch = display_list_info.epoch;
                details.display_list_epoch = Some(Epoch(epoch.0));

                let first_reflow = display_list_info.first_reflow;
                if details.first_paint_metric == PaintMetricState::Waiting {
                    details.first_paint_metric = PaintMetricState::Seen(epoch.into(), first_reflow);
                }
                if details.first_contentful_paint_metric == PaintMetricState::Waiting
                    && display_list_info.is_contentful
                {
                    details.first_contentful_paint_metric =
                        PaintMetricState::Seen(epoch.into(), first_reflow);
                }

                let mut transaction = Transaction::new();
                transaction.set_display_list(
                    display_list_info.epoch.into(),
                    (pipeline_id, built_display_list),
                );
                self.update_transaction_with_all_scroll_offsets(&mut transaction);
                self.send_transaction(transaction);
            }

            CompositorMsg::GenerateFrame => {
                self.frame_delayer.set_pending_frame(true);

                if !self.frame_delayer.needs_new_frame() {
                    return;
                }

                let mut transaction = Transaction::new();
                self.generate_frame(&mut transaction, RenderReasons::SCENE);
                self.send_transaction(transaction);

                let waiting_pipelines = self.frame_delayer.take_waiting_pipelines();
                let _ = self.constellation_sender.send(
                    EmbedderToConstellationMessage::NoLongerWaitingOnAsynchronousImageUpdates(
                        waiting_pipelines,
                    ),
                );
                self.frame_delayer.set_pending_frame(false);
                self.screenshot_taker
                    .prepare_screenshot_requests_for_render(self)
            }

            CompositorMsg::GenerateImageKey(sender) => {
                let _ = sender.send(self.webrender_api.generate_image_key());
            }

            CompositorMsg::GenerateImageKeysForPipeline(pipeline_id) => {
                let image_keys = (0..pref!(image_key_batch_size))
                    .map(|_| self.webrender_api.generate_image_key())
                    .collect();
                if let Err(error) = self.constellation_sender.send(
                    EmbedderToConstellationMessage::SendImageKeysForPipeline(
                        pipeline_id,
                        image_keys,
                    ),
                ) {
                    warn!("Sending Image Keys to Constellation failed with({error:?}).");
                }
            }

            CompositorMsg::UpdateImages(updates) => {
                let mut txn = Transaction::new();
                for update in updates {
                    match update {
                        ImageUpdate::AddImage(key, desc, data) => {
                            txn.add_image(key, desc, data.into(), None)
                        }
                        ImageUpdate::DeleteImage(key) => {
                            txn.delete_image(key);
                            self.frame_delayer.delete_image(key);
                        }
                        ImageUpdate::UpdateImage(key, desc, data, epoch) => {
                            if let Some(epoch) = epoch {
                                self.frame_delayer.update_image(key, epoch);
                            }
                            txn.update_image(key, desc, data.into(), &DirtyRect::All)
                        }
                    }
                }

                if self.frame_delayer.needs_new_frame() {
                    self.frame_delayer.set_pending_frame(false);
                    self.generate_frame(&mut txn, RenderReasons::SCENE);
                    let waiting_pipelines = self.frame_delayer.take_waiting_pipelines();
                    let _ = self.constellation_sender.send(
                        EmbedderToConstellationMessage::NoLongerWaitingOnAsynchronousImageUpdates(
                            waiting_pipelines,
                        ),
                    );
                    self.screenshot_taker
                        .prepare_screenshot_requests_for_render(self);
                }

                self.send_transaction(txn);
            }

            CompositorMsg::DelayNewFrameForCanvas(pipeline_id, canvas_epoch, image_keys) => self
                .frame_delayer
                .add_delay(pipeline_id, canvas_epoch, image_keys),

            CompositorMsg::AddFont(font_key, data, index) => {
                self.add_font(font_key, index, data);
            }

            CompositorMsg::AddSystemFont(font_key, native_handle) => {
                let mut transaction = Transaction::new();
                transaction.add_native_font(font_key, native_handle);
                self.send_transaction(transaction);
            }

            CompositorMsg::AddFontInstance(
                font_instance_key,
                font_key,
                size,
                flags,
                variations,
            ) => {
                self.add_font_instance(font_instance_key, font_key, size, flags, variations);
            }

            CompositorMsg::RemoveFonts(keys, instance_keys) => {
                let mut transaction = Transaction::new();

                for instance in instance_keys.into_iter() {
                    transaction.delete_font_instance(instance);
                }
                for key in keys.into_iter() {
                    transaction.delete_font(key);
                }

                self.send_transaction(transaction);
            }

            CompositorMsg::GenerateFontKeys(
                number_of_font_keys,
                number_of_font_instance_keys,
                result_sender,
            ) => {
                let font_keys = (0..number_of_font_keys)
                    .map(|_| self.webrender_api.generate_font_key())
                    .collect();
                let font_instance_keys = (0..number_of_font_instance_keys)
                    .map(|_| self.webrender_api.generate_font_instance_key())
                    .collect();
                let _ = result_sender.send((font_keys, font_instance_keys));
            }

            CompositorMsg::Viewport(_webview_id, viewport_description) => {
                self.pending_scroll_zoom_events
                    .push(ScrollZoomEvent::ViewportZoom(
                        viewport_description
                            .clone()
                            .clamp_zoom(viewport_description.initial_scale.get()),
                    ));
                // TODO: This is tied to per-webview and we don't currently have it yet
                // self.viewport_description = Some(viewport_description);
            }

            CompositorMsg::ScreenshotReadinessReponse(webview_id, pipelines_and_epochs) => {
                self.screenshot_taker.handle_screenshot_readiness_reply(
                    webview_id,
                    pipelines_and_epochs,
                    self,
                );
            }
        }
    }

    /// Handle messages sent to the compositor during the shutdown process. In general,
    /// the things the compositor can do in this state are limited. It's very important to
    /// answer any synchronous messages though as other threads might be waiting on the
    /// results to finish their own shut down process. We try to do as little as possible
    /// during this time.
    ///
    /// When that involves generating WebRender ids, our approach here is to simply
    /// generate them, but assume they will never be used, since once shutting down the
    /// compositor no longer does any WebRender frame generation.
    fn handle_browser_message_while_shutting_down(&mut self, msg: CompositorMsg) {
        match msg {
            CompositorMsg::PipelineExited(_webview_id, pipeline_id, pipeline_exit_source) => {
                debug!("Compositor got pipeline exited: {:?}", pipeline_id);
                self.pipeline_exited(pipeline_id, pipeline_exit_source);
            }
            CompositorMsg::GenerateImageKey(sender) => {
                let _ = sender.send(self.webrender_api.generate_image_key());
            }
            CompositorMsg::GenerateFontKeys(
                number_of_font_keys,
                number_of_font_instance_keys,
                result_sender,
            ) => {
                let font_keys = (0..number_of_font_keys)
                    .map(|_| self.webrender_api.generate_font_key())
                    .collect();
                let font_instance_keys = (0..number_of_font_instance_keys)
                    .map(|_| self.webrender_api.generate_font_instance_key())
                    .collect();
                let _ = result_sender.send((font_keys, font_instance_keys));
            }
            _ => {
                debug!("Ignoring message ({:?} while shutting down", msg);
            }
        }
    }

    /// Queue a new frame in the transaction and increase the pending frames count.
    fn generate_frame(&mut self, transaction: &mut Transaction, reason: RenderReasons) {
        transaction.generate_frame(0, true /* present */, reason);
        self.pending_frames += 1;
    }

    /// Sets or unsets the animations-running flag for the given pipeline. Returns
    /// true if the pipeline has started animating.
    pub(crate) fn change_pipeline_running_animations_state(
        &mut self,
        pipeline_id: PipelineId,
        animation_state: AnimationState,
    ) -> bool {
        let pipeline_details = self.pipeline_details(pipeline_id);
        let was_animating = pipeline_details.animating();
        match animation_state {
            AnimationState::AnimationsPresent => {
                pipeline_details.animations_running = true;
            }
            AnimationState::AnimationCallbacksPresent => {
                pipeline_details.animation_callbacks_running = true;
            }
            AnimationState::NoAnimationsPresent => {
                pipeline_details.animations_running = false;
            }
            AnimationState::NoAnimationCallbacksPresent => {
                pipeline_details.animation_callbacks_running = false;
            }
        }
        let started_animating = !was_animating && pipeline_details.animating();

        self.update_animation_state();

        // It's important that an animation tick is triggered even if the
        // WebViewRenderer's overall animation state hasn't changed. It's possible that
        // the WebView was animating, but not producing new display lists. In that case,
        // no repaint will happen and thus no repaint will trigger the next animation tick.
        started_animating
    }

    /// Sets or unsets the throttled flag for the given pipeline. Returns
    /// true if the pipeline has started animating.
    pub(crate) fn set_throttled(&mut self, pipeline_id: PipelineId, throttled: bool) -> bool {
        let pipeline_details = self.pipeline_details(pipeline_id);
        let was_animating = pipeline_details.animating();
        pipeline_details.throttled = throttled;
        let started_animating = !was_animating && pipeline_details.animating();

        // Throttling a pipeline can cause it to be taken into the "not-animating" state.
        self.update_animation_state();

        // It's important that an animation tick is triggered even if the
        // WebViewRenderer's overall animation state hasn't changed. It's possible that
        // the WebView was animating, but not producing new display lists. In that case,
        // no repaint will happen and thus no repaint will trigger the next animation tick.
        started_animating
    }

    pub(crate) fn update_animation_state(&mut self) {
        self.is_animating = self
            .pipeline_details
            .values()
            .any(PipelineDetails::animating);
    }

    fn pipeline_details(&mut self, pipeline_id: PipelineId) -> &mut PipelineDetails {
        self.pipeline_details
            .entry(pipeline_id)
            .or_insert_with(PipelineDetails::new);
        self.pipeline_details
            .get_mut(&pipeline_id)
            .expect("Insert then get failed!")
    }

    /// Set the root pipeline for our WebRender scene to a display list that consists of an iframe
    /// for each visible top-level browsing context, applying a transformation on the root for
    /// pinch zoom, page zoom, and HiDPI scaling.
    pub fn send_root_pipeline_display_list(&mut self, window: &Window) {
        let mut transaction = Transaction::new();
        self.send_root_pipeline_display_list_in_transaction(&mut transaction, window);
        self.generate_frame(&mut transaction, RenderReasons::SCENE);
        self.send_transaction(transaction);
    }

    /// Set the root pipeline for our WebRender scene to a display list that consists of an iframe
    /// for each visible top-level browsing context, applying a transformation on the root for
    /// pinch zoom, page zoom, and HiDPI scaling.
    fn send_root_pipeline_display_list_in_transaction(
        &self,
        transaction: &mut Transaction,
        window: &Window,
    ) {
        // Every display list needs a pipeline, but we'd like to choose one that is unlikely
        // to conflict with our content pipelines, which start at (1, 1). (0, 0) is WebRender's
        // dummy pipeline, so we choose (0, 1).
        let root_pipeline = WebRenderPipelineId(u64::from(self.current_window) as u32, 1);
        transaction.set_root_pipeline(root_pipeline);

        let mut builder = webrender::api::DisplayListBuilder::new(root_pipeline);
        builder.begin();

        let zoom_factor = self.device_pixels_per_page_pixel().0;
        let zoom_reference_frame = builder.push_reference_frame(
            LayoutPoint::zero(),
            SpatialId::root_reference_frame(root_pipeline),
            TransformStyle::Flat,
            PropertyBinding::Value(Transform3D::scale(zoom_factor, zoom_factor, 1.)),
            ReferenceFrameKind::Transform {
                is_2d_scale_translation: true,
                should_snap: true,
                paired_with_perspective: false,
            },
            SpatialTreeItemKey::new(0, 0),
        );

        let viewport_size = self.rendering_context.size2d().to_f32().to_untyped();
        let viewport_rect = LayoutRect::from_origin_and_size(
            LayoutPoint::zero(),
            LayoutSize::from_untyped(viewport_size),
        );

        let root_clip_id = builder.define_clip_rect(zoom_reference_frame, viewport_rect);
        let root_clip_chain_id = builder.define_clip_chain(None, [root_clip_id]);
        // Only decorate the webviews if we're in the browser mode
        let should_decorate = window.panel.is_some();
        for webview in window.painting_order() {
            if let Some(pipeline_id) = self.webviews.get(&webview.webview_id) {
                let scaled_webview_rect =
                    LayoutRect::from_untyped(&(webview.rect.to_f32() / zoom_factor).to_untyped());
                let root_space_and_clip = if should_decorate {
                    let complex = ComplexClipRegion::new(
                        scaled_webview_rect,
                        BorderRadius::uniform(10.), // TODO: add fields to webview
                        ClipMode::Clip,
                    );
                    let clip_id = builder.define_clip_rounded_rect(zoom_reference_frame, complex);
                    let clip_chain_id =
                        builder.define_clip_chain(Some(root_clip_chain_id), [clip_id]);
                    SpaceAndClipInfo {
                        spatial_id: zoom_reference_frame,
                        clip_chain_id,
                    }
                } else {
                    SpaceAndClipInfo {
                        spatial_id: zoom_reference_frame,
                        clip_chain_id: root_clip_chain_id,
                    }
                };

                builder.push_iframe(
                    scaled_webview_rect,
                    scaled_webview_rect,
                    &root_space_and_clip,
                    pipeline_id.into(),
                    true,
                );

                if should_decorate {
                    let root_space = SpaceAndClipInfo {
                        spatial_id: zoom_reference_frame,
                        clip_chain_id: root_clip_chain_id,
                    };
                    let offset = vec2(0., 0.);
                    let color = ColorF::new(0.0, 0.0, 0.0, 0.4);
                    let blur_radius = 5.0;
                    let spread_radius = 0.0;
                    let box_shadow_type = BoxShadowClipMode::Outset;

                    builder.push_box_shadow(
                        &CommonItemProperties::new(viewport_rect, root_space),
                        scaled_webview_rect,
                        offset,
                        color,
                        blur_radius,
                        spread_radius,
                        BorderRadius::uniform(10.),
                        box_shadow_type,
                    );
                }
            }
        }

        let built_display_list = builder.end();

        // NB: We are always passing 0 as the epoch here, but this doesn't seem to
        // be an issue. WebRender will still update the scene and generate a new
        // frame even though the epoch hasn't changed.
        transaction.set_display_list(WebRenderEpoch(0), built_display_list);
        self.update_transaction_with_all_scroll_offsets(transaction);
    }

    /// Update the given transaction with the scroll offsets of all active scroll nodes in
    /// the WebRender scene. This is necessary because WebRender does not preserve scroll
    /// offsets between scroll tree modifications. If a display list could potentially
    /// modify a scroll tree branch, WebRender needs to have scroll offsets for that
    /// branch.
    ///
    /// TODO(mrobinson): Could we only send offsets for the branch being modified
    /// and not the entire scene?
    fn update_transaction_with_all_scroll_offsets(&self, transaction: &mut Transaction) {
        for details in self.pipeline_details.values() {
            for node in details.scroll_tree.nodes.iter() {
                let (Some(offset), Some(external_id)) = (node.offset(), node.external_id()) else {
                    continue;
                };

                let offset = LayoutVector2D::new(-offset.x, -offset.y);
                transaction.set_scroll_offsets(
                    external_id,
                    vec![SampledScrollOffset {
                        offset,
                        generation: 0,
                    }],
                );
            }
        }
    }

    fn create_or_update_webview(
        &mut self,
        frame_tree: &SendableFrameTree,
        windows: &mut HashMap<WindowId, (Window, DocumentId)>,
    ) {
        let pipeline_id = frame_tree.pipeline.id;
        let webview_id = frame_tree.pipeline.webview_id;
        debug!(
            "Verso Compositor is setting frame tree with pipeline {} for webview {}",
            pipeline_id, webview_id
        );
        if let Some(old_pipeline) = self.webviews.insert(webview_id, pipeline_id) {
            debug!("{webview_id}'s pipeline has changed from {old_pipeline} to {pipeline_id}");
        }

        if let Some((window, _)) = windows.get(&self.current_window) {
            self.send_root_pipeline_display_list(window);
        }
        self.create_or_update_pipeline_details_with_frame_tree(frame_tree, None);
        self.reset_scroll_tree_for_unattached_pipelines(frame_tree);

        self.frame_tree_id.next();
    }

    fn remove_webview(
        &mut self,
        webview_id: WebViewId,
        windows: &mut HashMap<WindowId, (Window, DocumentId)>,
    ) {
        debug!("Verso Compositor is removing webview {}", webview_id);
        let mut window_id = None;
        for (window, _) in windows.values_mut() {
            let (webview, close_window) = window.remove_webview(webview_id, self);
            if let Some(webview) = webview {
                if let Some(pipeline_id) = self.webviews.remove(&webview.webview_id) {
                    self.remove_pipeline_details_recursively(pipeline_id);
                }

                if close_window {
                    window_id = Some(window.id());
                } else {
                    // if the window is not closed, we need to update the display list
                    // to remove the webview from viewport
                    self.send_root_pipeline_display_list(window);
                }

                self.frame_tree_id.next();
                break;
            }
        }

        if let Some(id) = window_id {
            windows.remove(&id);
        }
    }

    /// Notify compositor the provided webview is resized. The compositor will tell constellation and update the display list.
    pub fn on_resize_webview_event(&mut self, webview_id: WebViewId, rect: DeviceRect) {
        self.send_window_size_message_for_top_level_browser_context(rect, webview_id);
    }

    fn send_window_size_message_for_top_level_browser_context(
        &self,
        rect: DeviceRect,
        webview_id: WebViewId,
    ) {
        // The device pixel ratio used by the style system should include the scale from page pixels
        // to device pixels, but not including any pinch zoom.
        let hidpi_scale_factor = self.device_pixels_per_page_pixel_not_including_page_zoom();
        let size = rect.size().to_f32() / hidpi_scale_factor;
        let msg = EmbedderToConstellationMessage::ChangeViewportDetails(
            webview_id,
            ViewportDetails {
                size,
                hidpi_scale_factor,
            },
            WindowSizeType::Resize,
        );
        if let Err(e) = self.constellation_sender.send(msg) {
            warn!("Sending window resize to constellation failed ({:?}).", e);
        }
    }

    fn reset_scroll_tree_for_unattached_pipelines(&mut self, frame_tree: &SendableFrameTree) {
        // TODO(mrobinson): Eventually this can selectively preserve the scroll trees
        // state for some unattached pipelines in order to preserve scroll position when
        // navigating backward and forward.
        fn collect_pipelines(pipelines: &mut HashSet<PipelineId>, frame_tree: &SendableFrameTree) {
            pipelines.insert(frame_tree.pipeline.id);
            for kid in &frame_tree.children {
                collect_pipelines(pipelines, kid);
            }
        }

        let mut attached_pipelines = HashSet::default();
        collect_pipelines(&mut attached_pipelines, frame_tree);

        self.pipeline_details
            .iter_mut()
            .filter(|(id, _)| !attached_pipelines.contains(id))
            .for_each(|(_, details)| details.scroll_tree.reset_all_scroll_offsets());
    }

    fn create_or_update_pipeline_details_with_frame_tree(
        &mut self,
        frame_tree: &SendableFrameTree,
        parent_pipeline_id: Option<PipelineId>,
    ) {
        let pipeline_id = frame_tree.pipeline.id;
        let pipeline_details = self.pipeline_details(pipeline_id);
        pipeline_details.pipeline = Some(frame_tree.pipeline.clone());
        pipeline_details.parent_pipeline_id = parent_pipeline_id;

        for kid in &frame_tree.children {
            self.create_or_update_pipeline_details_with_frame_tree(kid, Some(pipeline_id));
        }
    }

    fn remove_pipeline_details_recursively(&mut self, pipeline_id: PipelineId) {
        self.pipeline_details.remove(&pipeline_id);

        let children = self
            .pipeline_details
            .iter()
            .filter(|(_, pipeline_details)| {
                pipeline_details.parent_pipeline_id == Some(pipeline_id)
            })
            .map(|(&pipeline_id, _)| pipeline_id)
            .collect::<Vec<_>>();

        for kid in children {
            self.remove_pipeline_details_recursively(kid);
        }
    }

    fn pipeline_exited(&mut self, pipeline_id: PipelineId, source: PipelineExitSource) {
        let pipeline = self.pipeline_details.entry(pipeline_id);
        let Entry::Occupied(mut pipeline) = pipeline else {
            return;
        };

        pipeline.get_mut().exited.insert(source);

        // Do not remove pipeline details until both the Constellation and Script have
        // finished processing the pipeline shutdown. This prevents any followup messges
        // from re-adding the pipeline details and creating a zombie.
        if !pipeline.get().exited.is_all() {
            return;
        }

        pipeline.remove_entry();
        self.pipeline_details.remove(&pipeline_id);
    }

    /// Change the current window of the compositor should display.
    pub fn swap_current_window(&mut self, window: &mut Window) {
        if window.id() != self.current_window {
            debug!(
                "Verso Compositor swap current window from {:?} to {:?}",
                self.current_window,
                window.id()
            );
            self.current_window = window.id();
            self.scale_factor = Scale::new(window.scale_factor() as f32);
            self.resize(window.size(), window);
        }
    }

    /// Resize the rendering context and all web views.
    pub fn resize(&mut self, size: Size2D<f32, DevicePixel>, window: &mut Window) {
        if size.height == 0.0 || size.width == 0.0 {
            return;
        }

        self.on_resize_window_event(size, window);

        if let Some(panel) = &mut window.panel {
            let rect = DeviceRect::from_size(size);
            panel.webview.rect = rect;
            self.on_resize_webview_event(panel.webview.webview_id, rect);
        }

        let rect = DeviceRect::from_size(size);
        let show_tab_bar = window.tab_manager.count() > 1;
        let content_size = window.get_content_size(rect, show_tab_bar, window.show_bookmark);
        if let Some(tab_id) = window.tab_manager.current_tab_id() {
            let (tab_id, prompt_id) = window.tab_manager.set_size(tab_id, content_size);
            if let Some(tab_id) = tab_id {
                self.on_resize_webview_event(tab_id, content_size);
            }
            if let Some(prompt_id) = prompt_id {
                self.on_resize_webview_event(prompt_id, content_size);
            }
        }
        #[cfg(linux)]
        if let Some(webview_menu) = &mut window.webview_menu {
            let rect = DeviceRect::from_size(size);
            webview_menu.set_webview_rect(rect);
            self.on_resize_webview_event(webview_menu.webview().webview_id, rect);
        }

        self.send_root_pipeline_display_list(window);
    }

    /// Handle the window resize event.
    pub fn on_resize_window_event(&mut self, new_viewport: DeviceSize, window: &Window) {
        if self.shutdown_state != ShutdownState::NotShuttingDown {
            return;
        }

        self.rendering_context.resize(
            &window.surface,
            PhysicalSize {
                width: new_viewport.width as u32,
                height: new_viewport.height as u32,
            },
        );
        self.viewport = new_viewport;
        let mut transaction = Transaction::new();
        transaction.set_document_view(DeviceIntRect::from_size(self.viewport.to_i32()));
        self.send_transaction(transaction);
        self.set_needs_repaint(RepaintReason::Resize);
    }

    /// Handle the window scale factor event and return a boolean to tell embedder if it should further
    /// handle the scale factor event.
    pub fn on_scale_factor_event(&mut self, scale_factor: f32, window: &Window) -> bool {
        if self.shutdown_state != ShutdownState::NotShuttingDown {
            return false;
        }

        self.scale_factor = Scale::new(scale_factor);
        self.update_after_zoom_or_hidpi_change(window);
        self.set_needs_repaint(RepaintReason::Resize);
        true
    }

    /// Dispatch input event to constellation.
    fn dispatch_input_event_with_hit_testing(
        &mut self,
        webview_id: WebViewId,
        mut event: InputEvent,
    ) -> bool {
        let event_point = event.point();
        let hit_test_result = match event_point {
            Some(point) => {
                let hit_test_result = self.hit_test_at_point(point).into_iter().nth(0);
                if hit_test_result.is_none() {
                    warn!("Empty hit test result for input event, ignoring.");
                    return false;
                }
                hit_test_result
            }
            None => None,
        };

        match event {
            InputEvent::Touch(ref mut _touch_event) => {
                // FIXME
                // touch_event.init_sequence_id(self.touch_handler.current_sequence_id);
            }
            InputEvent::MouseMove(_) => {
                self.last_mouse_move_position = event_point;
            }
            InputEvent::MouseLeftViewport(_) => {
                self.last_mouse_move_position = None;
            }
            InputEvent::MouseButton(_) | InputEvent::Wheel(_) => {}
            _ => unreachable!("Unexpected input event type: {event:?}"),
        }

        if let Err(error) =
            self.constellation_sender
                .send(EmbedderToConstellationMessage::ForwardInputEvent(
                    webview_id,
                    event,
                    hit_test_result,
                ))
        {
            warn!("Sending event to constellation failed ({error:?}).");
            false
        } else {
            true
        }
    }

    /// Handle the input event in the window.
    pub fn on_input_event(&mut self, webview_id: WebViewId, event: InputEvent) {
        if self.shutdown_state != ShutdownState::NotShuttingDown {
            return;
        }

        if let InputEvent::Touch(event) = event {
            self.on_touch_event(webview_id, event);
            return;
        }
        self.dispatch_input_event_with_hit_testing(webview_id, event);
    }

    fn hit_test_at_point(&self, point: DevicePoint) -> Vec<CompositorHitTestResult> {
        self.hit_test_at_point_with_flags(point, HitTestFlags::empty())
    }

    fn hit_test_at_point_with_flags(
        &self,
        point: DevicePoint,
        flags: HitTestFlags,
    ) -> Vec<CompositorHitTestResult> {
        // DevicePoint and WorldPoint are the same for us.
        let world_point = WorldPoint::from_untyped(point.to_untyped());
        let results = self.webrender_api.hit_test(
            self.webrender_document,
            None, /* pipeline_id */
            world_point,
            flags,
        );

        results
            .items
            .iter()
            .map(|item| {
                let pipeline_id = item.pipeline.into();
                let external_scroll_id = ExternalScrollId(item.tag.0, item.pipeline);
                CompositorHitTestResult {
                    pipeline_id,
                    point_in_viewport: Point2D::from_untyped(item.point_in_viewport.to_untyped()),
                    external_scroll_id,
                }
            })
            .collect()
    }

    fn send_transaction(&mut self, transaction: Transaction) {
        self.webrender_api
            .send_transaction(self.webrender_document, transaction);
    }

    fn send_touch_event(&mut self, webview_id: WebViewId, event: TouchEvent) {
        self.dispatch_input_event_with_hit_testing(webview_id, InputEvent::Touch(event));
    }

    /// Handle touch event.
    pub fn on_touch_event(&mut self, webview_id: WebViewId, event: TouchEvent) {
        if self.shutdown_state != ShutdownState::NotShuttingDown {
            return;
        }

        match event.event_type {
            TouchEventType::Down => self.on_touch_down(webview_id, event),
            TouchEventType::Move => self.on_touch_move(webview_id, event),
            TouchEventType::Up => self.on_touch_up(webview_id, event),
            TouchEventType::Cancel => self.on_touch_cancel(webview_id, event),
        }
    }

    fn on_touch_down(&mut self, webview_id: WebViewId, event: TouchEvent) {
        self.touch_handler.on_touch_down(event.id, event.point);
        self.send_touch_event(webview_id, event);
    }

    fn on_touch_move(&mut self, webview_id: WebViewId, event: TouchEvent) {
        match self.touch_handler.on_touch_move(event.id, event.point) {
            TouchAction::Scroll(delta) => self.on_scroll_window_event(
                ScrollLocation::Delta(LayoutVector2D::from_untyped(delta.to_untyped())),
                event.point.cast(),
            ),
            TouchAction::Zoom(magnification, scroll_delta) => {
                let cursor = Point2D::new(-1, -1); // Make sure this hits the base layer.

                // The order of these events doesn't matter, because zoom is handled by
                // a root display list and the scroll event here is handled by the scroll
                // applied to the content display list.
                self.pending_scroll_zoom_events
                    .push(ScrollZoomEvent::PinchZoom(magnification));
                self.pending_scroll_zoom_events
                    .push(ScrollZoomEvent::Scroll(ScrollEvent {
                        scroll_location: ScrollLocation::Delta(LayoutVector2D::from_untyped(
                            scroll_delta.to_untyped(),
                        )),
                        cursor,
                        event_count: 1,
                    }));
            }
            TouchAction::DispatchEvent => self.send_touch_event(webview_id, event),
            _ => {}
        }
    }

    fn on_touch_up(&mut self, webview_id: WebViewId, event: TouchEvent) {
        self.send_touch_event(webview_id, event);

        if let TouchAction::Click = self.touch_handler.on_touch_up(event.id, event.point) {
            self.simulate_mouse_click(webview_id, event.point);
        }
    }

    fn on_touch_cancel(&mut self, webview_id: WebViewId, event: TouchEvent) {
        // Send the event to script.
        self.touch_handler.on_touch_cancel(event.id, event.point);
        self.send_touch_event(webview_id, event);
    }

    /// <http://w3c.github.io/touch-events/#mouse-events>
    fn simulate_mouse_click(&mut self, webview_id: WebViewId, point: DevicePoint) {
        let button = MouseButton::Left;
        self.dispatch_input_event_with_hit_testing(
            webview_id,
            InputEvent::MouseMove(MouseMoveEvent::new(point)),
        );
        self.dispatch_input_event_with_hit_testing(
            webview_id,
            InputEvent::MouseButton(MouseButtonEvent::new(
                MouseButtonAction::Down,
                button,
                point,
            )),
        );
        self.dispatch_input_event_with_hit_testing(
            webview_id,
            InputEvent::MouseButton(MouseButtonEvent::new(MouseButtonAction::Up, button, point)),
        );
        self.dispatch_input_event_with_hit_testing(
            webview_id,
            InputEvent::MouseButton(MouseButtonEvent::new(
                MouseButtonAction::Click,
                button,
                point,
            )),
        );
    }

    /// Handle scroll event.
    pub fn on_scroll_event(
        &mut self,
        scroll_location: ScrollLocation,
        cursor: DeviceIntPoint,
        action: TouchEventType,
    ) {
        if self.shutdown_state != ShutdownState::NotShuttingDown {
            return;
        }

        match action {
            TouchEventType::Move => self.on_scroll_window_event(scroll_location, cursor),
            TouchEventType::Up | TouchEventType::Cancel => {
                self.on_scroll_window_event(scroll_location, cursor);
            }
            TouchEventType::Down => {
                self.on_scroll_window_event(scroll_location, cursor);
            }
        }
    }

    fn on_scroll_window_event(&mut self, scroll_location: ScrollLocation, cursor: DeviceIntPoint) {
        self.pending_scroll_zoom_events
            .push(ScrollZoomEvent::Scroll(ScrollEvent {
                scroll_location,
                cursor,
                event_count: 1,
            }));
    }

    fn process_pending_scroll_events(&mut self, _window: &Window) {
        // Batch up all scroll events into one, or else we'll do way too much painting.
        let mut combined_scroll_event: Option<ScrollEvent> = None;
        let mut _combined_magnification = 1.0;
        for scroll_event in self.pending_scroll_zoom_events.drain(..) {
            match scroll_event {
                ScrollZoomEvent::PinchZoom(magnification)
                | ScrollZoomEvent::ViewportZoom(magnification) => {
                    _combined_magnification *= magnification
                }
                ScrollZoomEvent::Scroll(scroll_event_info) => {
                    let combined_event = match combined_scroll_event.as_mut() {
                        None => {
                            combined_scroll_event = Some(scroll_event_info);
                            continue;
                        }
                        Some(combined_event) => combined_event,
                    };

                    match (
                        combined_event.scroll_location,
                        scroll_event_info.scroll_location,
                    ) {
                        (ScrollLocation::Delta(old_delta), ScrollLocation::Delta(new_delta)) => {
                            // Mac OS X sometimes delivers scroll events out of vsync during a
                            // fling. This causes events to get bunched up occasionally, causing
                            // nasty-looking "pops". To mitigate this, during a fling we average
                            // deltas instead of summing them.
                            let old_event_count = Scale::new(combined_event.event_count as f32);
                            combined_event.event_count += 1;
                            let new_event_count = Scale::new(combined_event.event_count as f32);
                            combined_event.scroll_location = ScrollLocation::Delta(
                                (old_delta * old_event_count + new_delta) / new_event_count,
                            );
                        }
                        (ScrollLocation::Start, _) | (ScrollLocation::End, _) => {
                            // Once we see Start or End, we shouldn't process any more events.
                            break;
                        }
                        (_, ScrollLocation::Start) | (_, ScrollLocation::End) => {
                            // If this is an event which is scrolling to the start or end of the page,
                            // disregard other pending events and exit the loop.
                            *combined_event = scroll_event_info;
                            break;
                        }
                    }
                }
            }
        }

        let scroll_result = combined_scroll_event.and_then(|combined_event| {
            self.scroll_node_at_device_point(
                combined_event.cursor.to_f32(),
                combined_event.scroll_location,
            )
        });

        if let Some(ScrollResult {
            hit_test_result,
            external_scroll_id,
            offset,
            ..
        }) = scroll_result
        {
            self.send_scroll_positions_to_layout_for_pipeline(hit_test_result.pipeline_id);

            let mut transaction = Transaction::new();
            transaction.set_scroll_offsets(
                external_scroll_id,
                vec![SampledScrollOffset {
                    offset,
                    generation: 0,
                }],
            );

            self.generate_frame(&mut transaction, RenderReasons::APZ);
            self.send_transaction(transaction);
        }
    }

    /// Perform a hit test at the given [`DevicePoint`] and apply the [`ScrollLocation`]
    /// scrolling to the applicable scroll node under that point. If a scroll was
    /// performed, returns the hit test result contains [`PipelineId`] of the node
    /// scrolled, the id, and the final scroll delta.
    fn scroll_node_at_device_point(
        &mut self,
        cursor: DevicePoint,
        scroll_location: ScrollLocation,
    ) -> Option<ScrollResult> {
        let scroll_location = match scroll_location {
            ScrollLocation::Delta(delta) => {
                let device_pixels_per_page = self.device_pixels_per_page_pixel();
                let scaled_delta = (Vector2D::from_untyped(delta.to_untyped())
                    / device_pixels_per_page)
                    .to_untyped();
                let calculated_delta = LayoutVector2D::from_untyped(scaled_delta);
                ScrollLocation::Delta(calculated_delta)
            }
            // Leave ScrollLocation unchanged if it is Start or End location.
            ScrollLocation::Start | ScrollLocation::End => scroll_location,
        };

        let hit_test_results = self.hit_test_at_point_with_flags(cursor, HitTestFlags::FIND_ALL);

        // Iterate through all hit test results, processing only the first node of each pipeline.
        // This is needed to propagate the scroll events from a pipeline representing an iframe to
        // its ancestor pipelines.
        let mut previous_pipeline_id = None;
        for hit_test_result in hit_test_results.iter() {
            let pipeline_details = self
                .pipeline_details
                .get_mut(&hit_test_result.pipeline_id)?;
            if previous_pipeline_id.replace(&hit_test_result.pipeline_id)
                != Some(&hit_test_result.pipeline_id)
            {
                let scroll_result = pipeline_details.scroll_tree.scroll_node_or_ancestor(
                    hit_test_result.external_scroll_id,
                    scroll_location,
                    ScrollType::InputEvents,
                );
                if let Some((external_scroll_id, offset)) = scroll_result {
                    return Some(ScrollResult {
                        hit_test_result: hit_test_result.clone(),
                        external_scroll_id,
                        offset,
                    });
                }
            }
        }
        None
    }

    /// If there are any animations running, dispatches appropriate messages to the constellation.
    fn process_animations(&mut self, force: bool) {
        // When running animations in order to dump a screenshot (not after a full composite), don't send
        // animation ticks faster than about 60Hz.
        //
        // TODO: This should be based on the refresh rate of the screen and also apply to all
        // animation ticks, not just ones sent while waiting to dump screenshots. This requires
        // something like a refresh driver concept though.
        if !force && (Instant::now() - self.last_animation_tick) < Duration::from_millis(16) {
            return;
        }
        self.last_animation_tick = Instant::now();

        let animating_webviews: Vec<_> = self
            .pipeline_details
            .iter()
            .filter_map(|(_, pipline_detail)| {
                if pipline_detail.animating() {
                    pipline_detail
                        .pipeline
                        .as_ref()
                        .map(|pipline| pipline.webview_id)
                } else {
                    None
                }
            })
            .collect();
        if !animating_webviews.is_empty() {
            if let Err(error) =
                self.constellation_sender
                    .send(EmbedderToConstellationMessage::TickAnimation(
                        animating_webviews,
                    ))
            {
                warn!("Sending tick to constellation failed ({error:?}).");
            }
        }
    }

    fn device_pixels_per_page_pixel(&self) -> Scale<f32, CSSPixel, DevicePixel> {
        self.device_pixels_per_page_pixel_not_including_page_zoom()
    }

    fn device_pixels_per_page_pixel_not_including_page_zoom(
        &self,
    ) -> Scale<f32, CSSPixel, DevicePixel> {
        Scale::new(self.scale_factor.get())
    }

    /// Handle zoom reset event
    pub fn on_zoom_reset_window_event(&mut self, window: &Window) {
        if self.shutdown_state != ShutdownState::NotShuttingDown {
            return;
        }

        self.update_after_zoom_or_hidpi_change(window);
    }

    /// Handle zoom event in the window
    pub fn on_zoom_window_event(&mut self, _magnification: f32, window: &Window) {
        if self.shutdown_state != ShutdownState::NotShuttingDown {
            return;
        }

        self.update_after_zoom_or_hidpi_change(window);
    }

    fn update_after_zoom_or_hidpi_change(&mut self, window: &Window) {
        for webview in window.painting_order() {
            self.send_window_size_message_for_top_level_browser_context(
                webview.rect,
                webview.webview_id,
            );
        }

        // Update the root transform in WebRender to reflect the new zoom.
        self.send_root_pipeline_display_list(window);
    }

    /// Simulate a pinch zoom
    pub fn on_pinch_zoom_window_event(&mut self, magnification: f32) {
        if self.shutdown_state != ShutdownState::NotShuttingDown {
            return;
        }

        // TODO: Scroll to keep the center in view?
        self.pending_scroll_zoom_events
            .push(ScrollZoomEvent::PinchZoom(magnification));
    }

    fn send_scroll_positions_to_layout_for_pipeline(&self, pipeline_id: PipelineId) {
        let Some(details) = self.pipeline_details.get(&pipeline_id) else {
            return;
        };

        let scroll_offsets = details.scroll_tree.scroll_offsets();

        // This might be true if we have not received a display list from the layout
        // associated with this pipeline yet. In that case, the layout is not ready to
        // receive scroll offsets anyway, so just save time and prevent other issues by
        // not sending them.
        if scroll_offsets.is_empty() {
            return;
        }

        let _ = self
            .constellation_sender
            .send(EmbedderToConstellationMessage::SetScrollStates(
                pipeline_id,
                scroll_offsets,
            ));
    }

    /// Returns true if any animation callbacks (ie `requestAnimationFrame`) are waiting for a response.
    fn animation_callbacks_running(&self) -> bool {
        self.pipeline_details
            .values()
            .any(PipelineDetails::animation_callbacks_running)
    }

    /// Composite to the given target if any, or the current target otherwise.
    pub fn composite(&mut self, window: &Window) {
        self.composite_specific_target(window);

        // We've painted the default target, which means that from the embedder's perspective,
        // the scene no longer needs to be repainted.
        self.needs_repaint.set(RepaintReason::empty());

        // Queue up any subsequent paints for animations.
        self.process_animations(true);
    }

    /// Composite to the given target if any, or the current target otherwise.
    fn composite_specific_target(&mut self, window: &Window) {
        if let Err(err) = self
            .rendering_context
            .make_gl_context_current(&window.surface)
        {
            warn!("Failed to make GL context current: {:?}", err);
        }
        self.assert_no_gl_error();

        if let Some(webrender) = self.webrender.as_mut() {
            webrender.update();
        }

        time_profile!(
            ProfilerCategory::Compositing,
            None,
            self.time_profiler_chan.clone(),
            || {
                trace!("Compositing");
                // Paint the scene.
                // TODO(gw): Take notice of any errors the renderer returns!
                if let Some(webrender) = self.webrender.as_mut() {
                    webrender
                        .render(self.viewport.to_i32(), 0 /* buffer_age */)
                        .ok();
                }
            },
        );

        self.send_pending_paint_metrics_messages_after_composite();
        self.screenshot_taker.maybe_take_screenshots(self);
    }

    #[track_caller]
    fn assert_no_gl_error(&self) {
        debug_assert_eq!(self.webrender_gl.get_error(), gl::NO_ERROR);
    }

    #[track_caller]
    fn assert_gl_framebuffer_complete(&self) {
        debug_assert_eq!(
            (
                self.webrender_gl.get_error(),
                self.webrender_gl.check_frame_buffer_status(gl::FRAMEBUFFER)
            ),
            (gl::NO_ERROR, gl::FRAMEBUFFER_COMPLETE)
        );
    }

    /// Receive and handle compositor messages.
    pub fn handle_messages(
        &mut self,
        windows: &mut HashMap<WindowId, (Window, DocumentId)>,
        mut messages: Vec<CompositorMsg>,
    ) {
        // Pull out the `NewWebRenderFrameReady` messages from the list of messages and handle them
        // at the end of this function. This prevents overdraw when more than a single message of
        // this type of received. In addition, if any of these frames need a repaint, that reflected
        // when calling `handle_new_webrender_frame_ready`.
        let mut repaint_needed = false;
        messages.retain(|message| match message {
            CompositorMsg::NewWebRenderFrameReady(_, need_repaint) => {
                self.pending_frames -= 1;
                repaint_needed |= need_repaint;
                false
            }
            _ => true,
        });
        for msg in messages {
            self.handle_browser_message(msg, windows);

            if self.shutdown_state == ShutdownState::FinishedShuttingDown {
                return;
            }
        }

        self.handle_new_webrender_frame_ready(repaint_needed);
    }

    /// Perform composition and related actions.
    pub fn perform_updates(
        &mut self,
        windows: &mut HashMap<WindowId, (Window, DocumentId)>,
    ) -> bool {
        if self.shutdown_state == ShutdownState::FinishedShuttingDown {
            return false;
        }

        if let Some((window, _)) = windows.get(&self.current_window) {
            if !self.pending_scroll_zoom_events.is_empty() {
                self.process_pending_scroll_events(window)
            }
        }
        self.shutdown_state != ShutdownState::FinishedShuttingDown
    }

    /// Update debug option of the webrender.
    pub fn toggle_webrender_debug(&mut self, option: WebRenderDebugOption) {
        let Some(webrender) = self.webrender.as_mut() else {
            return;
        };
        let mut flags = webrender.get_debug_flags();
        let flag = match option {
            WebRenderDebugOption::Profiler => {
                webrender::DebugFlags::PROFILER_DBG
                    | webrender::DebugFlags::GPU_TIME_QUERIES
                    | webrender::DebugFlags::GPU_SAMPLE_QUERIES
            }
            WebRenderDebugOption::TextureCacheDebug => webrender::DebugFlags::TEXTURE_CACHE_DBG,
            WebRenderDebugOption::RenderTargetDebug => webrender::DebugFlags::RENDER_TARGET_DBG,
        };
        flags.toggle(flag);
        webrender.set_debug_flags(flags);

        let mut txn = Transaction::new();
        self.generate_frame(&mut txn, RenderReasons::TESTING);
        self.send_transaction(txn);
    }

    fn add_font_instance(
        &mut self,
        instance_key: FontInstanceKey,
        font_key: FontKey,
        size: f32,
        flags: FontInstanceFlags,
        variations: Vec<FontVariation>,
    ) {
        let variations = if pref!(layout_variable_fonts_enabled) {
            variations
        } else {
            vec![]
        };

        let mut transaction = Transaction::new();
        let font_instance_options = FontInstanceOptions {
            flags,
            ..Default::default()
        };
        transaction.add_font_instance(
            instance_key,
            font_key,
            size,
            Some(font_instance_options),
            None,
            variations,
        );
        self.send_transaction(transaction);
    }

    fn add_font(&mut self, font_key: FontKey, index: u32, data: Arc<IpcSharedMemory>) {
        let mut transaction = Transaction::new();
        transaction.add_raw_font(font_key, (**data).into(), index);
        self.send_transaction(transaction);
    }

    /// Send all pending paint metrics messages after a composite operation, which may advance
    /// the epoch for pipelines in the WebRender scene.
    ///
    /// If there are pending paint metrics, we check if any of the painted epochs is one
    /// of the ones that the paint metrics recorder is expecting. In that case, we get the
    /// current time, inform the constellation about it and remove the pending metric from
    /// the list.
    fn send_pending_paint_metrics_messages_after_composite(&mut self) {
        let paint_time = CrossProcessInstant::now();
        let document_id = self.webrender_document;
        for (_, pipeline_id) in self.webviews.iter_mut() {
            debug_assert!(self.pipeline_details.contains_key(pipeline_id));
            let pipeline = self.pipeline_details.get_mut(pipeline_id).unwrap();
            let Some(current_epoch) = self
                .webrender
                .as_ref()
                .and_then(|wr| wr.current_epoch(document_id, (*pipeline_id).into()))
            else {
                continue;
            };

            match pipeline.first_paint_metric {
                // We need to check whether the current epoch is later, because
                // CompositorMsg::SendInitialTransaction sends an
                // empty display list to WebRender which can happen before we receive
                // the first "real" display list.
                PaintMetricState::Seen(epoch, first_reflow) if epoch <= current_epoch => {
                    assert!(epoch <= current_epoch);
                    if let Err(error) =
                        self.constellation_sender
                            .send(EmbedderToConstellationMessage::PaintMetric(
                                *pipeline_id,
                                PaintMetricEvent::FirstPaint(paint_time, first_reflow),
                            ))
                    {
                        warn!("Sending paint metric event to constellation failed ({error:?}).");
                    }
                    pipeline.first_paint_metric = PaintMetricState::Sent;
                }
                _ => {}
            }

            match pipeline.first_contentful_paint_metric {
                PaintMetricState::Seen(epoch, first_reflow) if epoch <= current_epoch => {
                    if let Err(error) =
                        self.constellation_sender
                            .send(EmbedderToConstellationMessage::PaintMetric(
                                *pipeline_id,
                                PaintMetricEvent::FirstContentfulPaint(paint_time, first_reflow),
                            ))
                    {
                        warn!("Sending paint metric event to constellation failed ({error:?}).");
                    }
                    pipeline.first_contentful_paint_metric = PaintMetricState::Sent;
                }
                _ => {}
            }
        }
    }

    fn refresh_cursor(&self) {
        let Some(last_mouse_move_position) = self.last_mouse_move_position else {
            return;
        };

        let Some(hit_test_result) = self
            .hit_test_at_point(last_mouse_move_position)
            .first()
            .cloned()
        else {
            return;
        };

        if let Err(error) =
            self.constellation_sender
                .send(EmbedderToConstellationMessage::RefreshCursor(
                    hit_test_result.pipeline_id,
                ))
        {
            warn!("Sending event to constellation failed ({:?}).", error);
        }
    }

    fn handle_new_webrender_frame_ready(&mut self, repaint_needed: bool) {
        if repaint_needed {
            self.refresh_cursor();
        }
        if repaint_needed || self.animation_callbacks_running() {
            self.set_needs_repaint(RepaintReason::NewWebRenderFrame);
        }

        // If we received a new frame and a repaint isn't necessary, it may be that this
        // is the last frame that was pending. In that case, trigger a manual repaint so
        // that the screenshot can be taken at the end of the repaint procedure.
        if !repaint_needed {
            self.screenshot_taker
                .maybe_trigger_paint_for_screenshot(self);
        }
    }

    /// Whether or not the renderer is waiting on a frame, either because it has been sent
    /// to WebRender and is not ready yet or because the [`FrameDelayer`] is delaying a frame
    /// waiting for asynchronous (canvas) image updates to complete.
    pub(crate) fn has_pending_frames(&self) -> bool {
        self.pending_frames != 0 || self.frame_delayer.pending_frame
    }

    pub fn request_screenshot(
        &self,
        webview_id: WebViewId,
        callback: Box<dyn FnOnce(Result<RgbaImage, ScreenshotCaptureError>) + 'static>,
    ) {
        self.screenshot_taker
            .request_screenshot(webview_id, callback);
        let _ = self.constellation_sender.send(
            EmbedderToConstellationMessage::RequestScreenshotReadiness(webview_id),
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FrameTreeId(u32);

impl FrameTreeId {
    pub fn next(&mut self) {
        self.0 += 1;
    }
}

/// A struct that is reponsible for delaying frame requests until all new canvas images
/// for a particular "update the rendering" call in the `ScriptThread` have been
/// sent to WebRender.
///
/// These images may be updated in WebRender asynchronously in the canvas task. A frame
/// is then requested if:
///
///  - The renderer has received a GenerateFrame message from a `ScriptThread`.
///  - All pending image updates have finished and have been noted in the [`FrameDelayer`].
#[derive(Default)]
struct FrameDelayer {
    /// The latest [`Epoch`] of canvas images that have been sent to WebRender. Note
    /// that this only records the `Epoch`s for canvases and only ones that are involved
    /// in "update the rendering".
    image_epochs: HashMap<ImageKey, Epoch>,
    /// A map of all pending canvas images
    pending_canvas_images: HashMap<ImageKey, Epoch>,
    /// Whether or not we have a pending frame.
    pending_frame: bool,
    /// A list of pipelines that should be notified when we are no longer waiting for
    /// canvas images.
    waiting_pipelines: HashSet<PipelineId>,
}

impl FrameDelayer {
    fn delete_image(&mut self, image_key: ImageKey) {
        self.image_epochs.remove(&image_key);
        self.pending_canvas_images.remove(&image_key);
    }

    fn update_image(&mut self, image_key: ImageKey, epoch: Epoch) {
        self.image_epochs.insert(image_key, epoch);
        let Entry::Occupied(entry) = self.pending_canvas_images.entry(image_key) else {
            return;
        };
        if *entry.get() <= epoch {
            entry.remove();
        }
    }

    fn add_delay(
        &mut self,
        pipeline_id: PipelineId,
        canvas_epoch: Epoch,
        image_keys: Vec<ImageKey>,
    ) {
        for image_key in image_keys.into_iter() {
            // If we've already seen the necessary epoch for this image, do not
            // start waiting for it.
            if self
                .image_epochs
                .get(&image_key)
                .is_some_and(|epoch_seen| *epoch_seen >= canvas_epoch)
            {
                continue;
            }
            self.pending_canvas_images.insert(image_key, canvas_epoch);
        }
        self.waiting_pipelines.insert(pipeline_id);
    }

    fn needs_new_frame(&self) -> bool {
        self.pending_frame && self.pending_canvas_images.is_empty()
    }

    fn set_pending_frame(&mut self, value: bool) {
        self.pending_frame = value;
    }

    fn take_waiting_pipelines(&mut self) -> Vec<PipelineId> {
        self.waiting_pipelines.drain().collect()
    }
}
