use std::{cell::Cell, collections::HashMap};

use base::id::WebViewId;
use constellation_traits::EmbedderToConstellationMessage;
use crossbeam_channel::Sender;
use embedder_traits::{
    AlertResponse, AllowOrDeny, ConfirmResponse, Cursor, EmbedderMsg, FocusId, ImeEvent,
    InputEvent, MouseButton, MouseButtonAction, MouseButtonEvent, MouseLeaveEvent, MouseMoveEvent,
    Notification, PromptResponse, ScreenMetrics, TouchEventType, ViewportDetails,
    WebResourceResponseMsg, WheelMode,
};
use euclid::{Point2D, Scale, Size2D};
use glutin::{
    config::{ConfigTemplateBuilder, GlConfig},
    surface::{Surface, WindowSurface},
};
use glutin_winit::DisplayBuilder;
use ipc_channel::ipc::IpcSender;
use keyboard_types::{
    Code, CompositionEvent, CompositionState, KeyState, KeyboardEvent, Modifiers,
};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use muda::{MenuEvent, MenuEventReceiver};
#[cfg(linux)]
use notify_rust::Image;
#[cfg(target_os = "macos")]
use raw_window_handle::HasWindowHandle;
use servo_geometry::{convert_rect_to_css_pixel, convert_size_to_css_pixel};
use servo_url::ServoUrl;
use versoview_messages::ToControllerMessage;
use webrender_api::{
    ScrollLocation,
    units::{
        DeviceIntPoint, DeviceIntRect, DevicePixel, DevicePoint, DeviceRect, DeviceSize,
        LayoutVector2D,
    },
};
#[cfg(any(linux, target_os = "windows"))]
use winit::window::ResizeDirection;
use winit::{
    dpi::{LogicalPosition, LogicalSize, PhysicalPosition},
    event::{ElementState, Ime, TouchPhase, WindowEvent},
    event_loop::ActiveEventLoop,
    keyboard::ModifiersState,
    window::{CursorIcon, Window as WinitWindow, WindowAttributes, WindowId},
};

use crate::{
    bookmark::BookmarkManager,
    compositor::IOCompositor,
    javascript_evaluator::JavaScriptEvaluator,
    keyboard::keyboard_event_from_winit,
    rendering::{RenderingContext, gl_config_picker},
    tab::TabManager,
    verso::send_to_constellation,
    webview::{Panel, WebView, prompt::PromptSender, webview_menu::WebViewMenu},
};

use arboard::Clipboard;

const PANEL_HEIGHT: f64 = 50.0;
const TAB_HEIGHT: f64 = 30.0;
const BOOKMARK_HEIGHT: f64 = 30.0;
const PANEL_PADDING: f64 = 4.0;

#[derive(Default)]
pub(crate) struct EventListeners {
    /// This is `true` if the controller wants to get and handle OnNavigationStarting/AllowNavigationRequest
    pub(crate) on_navigation_starting: bool,
    /// An id to request response sender map if the controller wants to get and handle web resource requests
    pub(crate) on_web_resource_requested:
        Option<HashMap<uuid::Uuid, (url::Url, IpcSender<WebResourceResponseMsg>)>>,
    /// This is `true` if the controller wants to get and handle WindowEvent::CloseRequested
    pub(crate) on_close_requested: bool,
}

#[derive(Debug, Default)]
struct CursorState {
    current_cursor: CursorIcon,
    #[cfg(any(linux, target_os = "windows"))]
    cursor_resizing: bool,
}

/// A Verso window is a Winit window containing several web views.
pub struct Window {
    /// Access to Winit window
    pub(crate) window: WinitWindow,
    cursor_state: CursorState,
    /// GL surface of the window
    pub(crate) surface: Surface<WindowSurface>,
    /// The main panel of this window.
    pub(crate) panel: Option<Panel>,
    /// The WebView of this window.
    // pub(crate) webview: Option<WebView>,
    /// Event listeners registered from the webview controller
    pub(crate) event_listeners: EventListeners,
    /// The mouse physical position in the web view.
    pub(crate) mouse_position: Cell<Option<PhysicalPosition<f64>>>,
    /// The webview that the mouse is currenly hovering on.
    hovering_webview: Option<WebViewId>,
    /// Modifiers state of the keyboard.
    modifiers_state: Cell<ModifiersState>,
    /// State to indicate if the window is resizing.
    pub(crate) resizing: bool,
    /// Global menu event receiver for muda crate
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub(crate) menu_event_receiver: MenuEventReceiver,
    /// Window tabs manager
    pub(crate) tab_manager: TabManager,
    pub(crate) focused_webview_id: Option<WebViewId>,
    /// Window-wide menu. e.g. context menu(Wayland) and browsing history menu.
    pub(crate) webview_menu: Option<Box<dyn WebViewMenu>>,
    /// Show the bookmark bar or not
    pub show_bookmark: bool,
}

impl Window {
    /// Create a Verso window from Winit window and return the rendering context.
    pub fn new(
        evl: &ActiveEventLoop,
        window_attributes: WindowAttributes,
    ) -> (Self, RenderingContext) {
        let template = ConfigTemplateBuilder::new()
            .with_alpha_size(8)
            .with_transparency(cfg!(macos));

        let (window, gl_config) = DisplayBuilder::new()
            .with_window_attributes(Some(window_attributes))
            .build(evl, template, gl_config_picker)
            .expect("Failed to create window and gl config");

        let window = window.ok_or("Failed to create window").unwrap();

        log::debug!("Picked a config with {} samples", gl_config.num_samples());

        #[cfg(macos)]
        unsafe {
            let rwh = window.window_handle().expect("Failed to get window handle");
            if let RawWindowHandle::AppKit(AppKitWindowHandle { ns_view, .. }) = rwh.as_ref() {
                decorate_window(
                    ns_view.as_ptr() as *mut AnyObject,
                    LogicalPosition::new(8.0, 40.0),
                );
            }
        }
        let (rendering_context, surface) =
            RenderingContext::create(&window, &gl_config, window.inner_size())
                .expect("Failed to create rendering context");
        log::trace!("Created rendering context for window {:?}", window);

        (
            Self {
                window,
                cursor_state: CursorState::default(),
                surface,
                panel: None,
                event_listeners: Default::default(),
                mouse_position: Default::default(),
                hovering_webview: None,
                modifiers_state: Cell::new(ModifiersState::default()),
                resizing: false,
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                menu_event_receiver: MenuEvent::receiver().clone(),
                tab_manager: TabManager::new(),
                focused_webview_id: None,
                webview_menu: None,
                show_bookmark: false,
            },
            rendering_context,
        )
    }

    /// Create a Verso window with the rendering context.
    pub fn new_with_compositor(
        evl: &ActiveEventLoop,
        window_attributes: WindowAttributes,
        compositor: &mut IOCompositor,
    ) -> Self {
        let window = evl
            .create_window(window_attributes)
            .expect("Failed to create window.");

        #[cfg(macos)]
        unsafe {
            let rwh = window.window_handle().expect("Failed to get window handle");
            if let RawWindowHandle::AppKit(AppKitWindowHandle { ns_view, .. }) = rwh.as_ref() {
                decorate_window(
                    ns_view.as_ptr() as *mut AnyObject,
                    LogicalPosition::new(8.0, 40.0),
                );
            }
        }
        let surface = compositor
            .rendering_context
            .create_surface(&window)
            .unwrap();

        let mut window = Self {
            window,
            cursor_state: CursorState::default(),
            surface,
            panel: None,
            // webview: None,
            event_listeners: Default::default(),
            mouse_position: Default::default(),
            hovering_webview: None,
            modifiers_state: Cell::new(ModifiersState::default()),
            resizing: false,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            menu_event_receiver: MenuEvent::receiver().clone(),
            tab_manager: TabManager::new(),
            focused_webview_id: None,
            webview_menu: None,
            show_bookmark: false,
        };
        compositor.swap_current_window(&mut window);
        window
    }

    /// Get the content area size for the webview to draw on
    pub fn get_content_size(
        &self,
        mut size: DeviceRect,
        include_tab: bool,
        include_bookmark: bool,
    ) -> DeviceRect {
        if self.panel.is_some() {
            let mut height: f64 = PANEL_HEIGHT + PANEL_PADDING;
            if include_tab {
                height += TAB_HEIGHT;
            }
            if include_bookmark {
                height += BOOKMARK_HEIGHT;
            }
            height *= self.scale_factor();
            size.min.y = size.max.y.min(height as f32);
            size.min.x += 10.0;
            size.max.y -= 10.0;
            size.max.x -= 10.0;
        }
        size
    }

    /// Send the constellation message to start Panel UI
    pub fn create_panel(
        &mut self,
        constellation_sender: &Sender<EmbedderToConstellationMessage>,
        initial_url: url::Url,
    ) {
        let hidpi_scale_factor = Scale::new(self.scale_factor() as f32);
        let size = self.window.inner_size();
        let size = Size2D::new(size.width as f32, size.height as f32);
        let size = size.to_f32() / hidpi_scale_factor;
        let viewport_details = ViewportDetails {
            size,
            hidpi_scale_factor,
        };

        let panel_id = WebViewId::new();
        self.panel = Some(Panel {
            webview: WebView::new(panel_id, viewport_details),
            initial_url: ServoUrl::from_url(initial_url),
        });

        let url = ServoUrl::parse("verso://resources/components/panel.html").unwrap();

        send_to_constellation(
            constellation_sender,
            EmbedderToConstellationMessage::NewWebView(url, panel_id, viewport_details),
        );
    }

    /// Create a new webview and send the constellation message to load the initial URL
    pub fn create_tab(
        &mut self,
        constellation_sender: &Sender<EmbedderToConstellationMessage>,
        initial_url: ServoUrl,
        javascript_evaluator: &mut JavaScriptEvaluator,
    ) {
        let webview_id = WebViewId::new();
        let size = self.size().to_f32();
        let rect = DeviceRect::from_size(size);

        let show_tab = self.tab_manager.count() >= 1;
        let content_size = self.get_content_size(rect, show_tab, self.show_bookmark);

        let hidpi_scale_factor = Scale::new(self.scale_factor() as f32);
        let size = content_size.size().to_f32() / hidpi_scale_factor;
        let viewport_details = ViewportDetails {
            size,
            hidpi_scale_factor,
        };

        let mut webview = WebView::new(webview_id, viewport_details);
        webview.set_size(content_size);

        if let Some(panel) = &self.panel {
            let cmd: String = format!(
                "window.navbar.addTab('{}', {})",
                serde_json::to_string(&webview.webview_id).unwrap(),
                true,
            );

            javascript_evaluator.evaluate_ignore_result(
                constellation_sender,
                &panel.webview.webview_id,
                cmd,
            );
        }

        self.tab_manager.append_tab(webview, true);

        send_to_constellation(
            constellation_sender,
            EmbedderToConstellationMessage::NewWebView(initial_url, webview_id, viewport_details),
        );
        log::debug!("Verso Window {:?} adds webview {}", self.id(), webview_id);
    }

    /// Close a tab
    pub fn close_tab(
        &mut self,
        compositor: &mut IOCompositor,
        tab_id: WebViewId,
        javascript_evaluator: &mut JavaScriptEvaluator,
    ) {
        // if there are more than 2 tabs, we need to ask for the new active tab after tab is closed
        if self.tab_manager.count() > 1 {
            if let Some(panel) = &self.panel {
                let cmd: String = format!(
                    "const nextTab = window.navbar.closeTab('{}');",
                    serde_json::to_string(&tab_id).unwrap()
                );
                let activate_next_tab = r#"if (nextTab) window.prompt(`ACTIVATE_TAB:${JSON.stringify({ id: nextTab })}`)"#;

                javascript_evaluator.evaluate_ignore_result(
                    &compositor.constellation_chan,
                    &panel.webview.webview_id,
                    format!("{cmd}{activate_next_tab}"),
                );
            }
        }
        send_to_constellation(
            &compositor.constellation_chan,
            EmbedderToConstellationMessage::CloseWebView(tab_id),
        );
    }

    /// Activate a tab
    pub fn activate_tab(
        &mut self,
        compositor: &mut IOCompositor,
        tab_id: WebViewId,
        show_tab: bool,
        javascript_evaluator: &mut JavaScriptEvaluator,
    ) {
        let size = self.size().to_f32();
        let rect = DeviceRect::from_size(size);
        let content_size = self.get_content_size(rect, show_tab, self.show_bookmark);
        let (tab_id, prompt_id) = self.tab_manager.set_size(tab_id, content_size);

        if let Some(prompt_id) = prompt_id {
            compositor.on_resize_webview_event(prompt_id, content_size);
        }
        if let Some(tab_id) = tab_id {
            compositor.on_resize_webview_event(tab_id, content_size);

            let old_tab_id = self.tab_manager.current_tab_id();
            if self.tab_manager.activate_tab(tab_id).is_some() {
                // throttle the old tab to avoid unnecessary animation caclulations
                if let Some(old_tab_id) = old_tab_id {
                    let _ = compositor.constellation_chan.send(
                        EmbedderToConstellationMessage::SetWebViewThrottled(old_tab_id, true),
                    );
                }
                let _ = compositor.constellation_chan.send(
                    EmbedderToConstellationMessage::SetWebViewThrottled(tab_id, false),
                );

                self.focused_webview_id = Some(tab_id);
                let _ = compositor.constellation_chan.send(
                    EmbedderToConstellationMessage::FocusWebView(tab_id, FocusId::new()),
                );

                // Set navigation button enabled state
                let history = self.tab_manager.history(tab_id).unwrap();
                let prev_btn_enabled = history.current_idx > 0;
                let next_btn_enabled = history.current_idx < history.list.len() - 1;
                javascript_evaluator.evaluate_ignore_result(
                    &compositor.constellation_chan,
                    &self.panel.as_ref().unwrap().webview.webview_id,
                    format!(
                        "window.navbar.setNavBtnEnabled({}, {})",
                        prev_btn_enabled, next_btn_enabled
                    ),
                );

                // update painting order immediately to draw the active tab
                compositor.send_root_pipeline_display_list(self);
            }
        }
    }

    /// Handle Winit window event and return a boolean to indicate if the compositor should repaint immediately.
    pub fn handle_winit_window_event(
        &mut self,
        sender: &Sender<EmbedderToConstellationMessage>,
        compositor: &mut IOCompositor,
        event: &winit::event::WindowEvent,
        javascript_evaluator: &mut JavaScriptEvaluator,
    ) {
        match event {
            WindowEvent::RedrawRequested => {
                if compositor.needs_repaint() {
                    self.window.pre_present_notify();
                    compositor.composite(self);
                    if let Err(err) = compositor.rendering_context.present(&self.surface) {
                        log::warn!("Failed to present surface: {:?}", err);
                    }
                }
            }
            WindowEvent::Focused(focused) => {
                if *focused {
                    compositor.swap_current_window(self);
                }
            }
            WindowEvent::Resized(size) => {
                if self.window.has_focus() {
                    self.resizing = true;
                }
                let size = Size2D::new(size.width, size.height);
                compositor.resize(size.to_f32(), self);
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                compositor.on_scale_factor_event(*scale_factor as f32, self);
            }
            WindowEvent::CursorEntered { .. } => {
                compositor.swap_current_window(self);
            }
            WindowEvent::CursorLeft { .. } => {
                let position = self.mouse_position.take();
                let hovering_webview = self.hovering_webview.take();
                if let (Some(position), Some(hovering_webview)) = (position, hovering_webview) {
                    let point: DevicePoint = DevicePoint::new(position.x as f32, position.y as f32);
                    forward_input_event(
                        compositor,
                        hovering_webview,
                        sender,
                        InputEvent::MouseLeave(MouseLeaveEvent::new(point)),
                    );
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let last_position = self.mouse_position.get().unwrap_or(*position).cast();
                self.mouse_position.set(Some(*position));

                let point = DevicePoint::new(position.x as f32, position.y as f32);
                let target_webview = self.forward_mouse_move(compositor, sender, point);
                let last_hovering_webview = self.hovering_webview.take();
                self.hovering_webview = target_webview;

                if last_hovering_webview != target_webview {
                    if let Some(last_hovering_webview) = last_hovering_webview {
                        let last_point = DevicePoint::new(last_position.x, last_position.y);
                        forward_input_event(
                            compositor,
                            last_hovering_webview,
                            sender,
                            InputEvent::MouseLeave(MouseLeaveEvent::new(last_point)),
                        );
                    }
                }

                // handle Windows and Linux non-decoration window resize cursor
                #[cfg(any(linux, target_os = "windows"))]
                {
                    if self.should_use_client_region_drag() {
                        let direction = self.get_drag_resize_direction();
                        self.set_drag_resize_cursor(direction);
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let point = match self.mouse_position.get() {
                    Some(point) => Point2D::new(point.x as f32, point.y as f32),
                    None => {
                        log::trace!("Mouse position is None, skipping MouseInput event.");
                        return;
                    }
                };

                /* handle context menu */
                if let (ElementState::Pressed, winit::event::MouseButton::Right) = (state, button) {
                    let prompt = self.tab_manager.current_prompt();
                    if prompt.is_some() {
                        return;
                    }
                }

                /* handle Windows and Linux non-decoration window resize */
                #[cfg(any(linux, target_os = "windows"))]
                {
                    if *state == ElementState::Pressed
                        && *button == winit::event::MouseButton::Left
                        && self.should_use_client_region_drag()
                    {
                        self.drag_resize_window();
                    }
                }

                /* handle mouse events */

                let webview_id = match self.focused_webview_id {
                    Some(webview_id) => webview_id,
                    None => {
                        log::trace!("No focused webview, skipping MouseInput event.");
                        return;
                    }
                };

                let button: MouseButton = match button {
                    winit::event::MouseButton::Left => MouseButton::Left,
                    winit::event::MouseButton::Right => MouseButton::Right,
                    winit::event::MouseButton::Middle => MouseButton::Middle,
                    winit::event::MouseButton::Back => MouseButton::Back,
                    winit::event::MouseButton::Forward => MouseButton::Forward,
                    winit::event::MouseButton::Other(value) => MouseButton::Other(*value),
                };

                let action = match state {
                    ElementState::Pressed => MouseButtonAction::Down,
                    ElementState::Released => MouseButtonAction::Up,
                };

                forward_input_event(
                    compositor,
                    webview_id,
                    sender,
                    InputEvent::MouseButton(MouseButtonEvent::new(action, button, point)),
                );
            }
            WindowEvent::PinchGesture { delta, .. } => {
                compositor.on_zoom_window_event(1.0 + *delta as f32, self);
            }
            WindowEvent::MouseWheel { delta, phase, .. } => {
                let point = match self.mouse_position.get() {
                    Some(point) => point,
                    None => {
                        log::trace!("Mouse position is None, skipping MouseWheel event.");
                        return;
                    }
                };

                // FIXME: Pixels per line, should be configurable (from browser setting?) and vary by zoom level.
                const LINE_HEIGHT: f32 = 38.0;

                let (mut x, mut y, _mode) = match delta {
                    winit::event::MouseScrollDelta::LineDelta(x, y) => {
                        (*x as f64, (*y * LINE_HEIGHT) as f64, WheelMode::DeltaLine)
                    }
                    winit::event::MouseScrollDelta::PixelDelta(position) => {
                        let position = position.to_logical::<f64>(self.window.scale_factor());
                        (position.x, position.y, WheelMode::DeltaPixel)
                    }
                };

                // Scroll Event
                // Do one axis at a time.
                if y.abs() >= x.abs() {
                    x = 0.0;
                } else {
                    y = 0.0;
                }

                let phase: TouchEventType = match phase {
                    TouchPhase::Started => TouchEventType::Down,
                    TouchPhase::Moved => TouchEventType::Move,
                    TouchPhase::Ended => TouchEventType::Up,
                    TouchPhase::Cancelled => TouchEventType::Cancel,
                };

                compositor.on_scroll_event(
                    ScrollLocation::Delta(LayoutVector2D::new(x as f32, y as f32)),
                    DeviceIntPoint::new(point.x as i32, point.y as i32),
                    phase,
                );
            }
            WindowEvent::ModifiersChanged(modifier) => self.modifiers_state.set(modifier.state()),
            WindowEvent::Ime(event) => {
                let webview_id = match self.focused_webview_id {
                    Some(webview_id) => webview_id,
                    None => {
                        log::trace!("No focused webview, skipping Ime event.");
                        return;
                    }
                };
                if !self.has_webview(webview_id) {
                    log::trace!(
                        "Webview {:?} doesn't exist, skipping Ime event.",
                        webview_id
                    );
                    return;
                }

                match event {
                    Ime::Commit(text) => {
                        let text = text.clone();
                        forward_input_event(
                            compositor,
                            webview_id,
                            sender,
                            InputEvent::Ime(ImeEvent::Composition(CompositionEvent {
                                state: CompositionState::End,
                                data: text,
                            })),
                        );
                    }
                    Ime::Enabled => {
                        forward_input_event(
                            compositor,
                            webview_id,
                            sender,
                            InputEvent::Ime(ImeEvent::Composition(CompositionEvent {
                                state: CompositionState::Start,
                                data: String::new(),
                            })),
                        );
                    }
                    Ime::Preedit(text, _) => {
                        forward_input_event(
                            compositor,
                            webview_id,
                            sender,
                            InputEvent::Ime(ImeEvent::Composition(CompositionEvent {
                                state: CompositionState::Update,
                                data: text.to_string(),
                            })),
                        );
                    }
                    Ime::Disabled => {
                        forward_input_event(
                            compositor,
                            webview_id,
                            sender,
                            InputEvent::Ime(ImeEvent::Composition(CompositionEvent {
                                state: CompositionState::End,
                                data: String::new(),
                            })),
                        );
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let webview_id = match self.focused_webview_id {
                    Some(webview_id) => webview_id,
                    None => {
                        log::trace!("No focused webview, skipping KeyboardInput event.");
                        return;
                    }
                };
                if !self.has_webview(webview_id) {
                    log::trace!(
                        "Webview {:?} doesn't exist, skipping KeyboardInput event.",
                        webview_id
                    );
                    return;
                }
                let event = keyboard_event_from_winit(event, self.modifiers_state.get());
                log::trace!("Verso is handling {:?}", event);

                /* Window operation keyboard shortcut */
                if self.handle_keyboard_shortcut(compositor, &event, javascript_evaluator) {
                    return;
                }
                forward_input_event(
                    compositor,
                    webview_id,
                    sender,
                    InputEvent::Keyboard(embedder_traits::KeyboardEvent::new(event)),
                );
            }
            WindowEvent::ThemeChanged(theme) => {
                let theme = to_servo_theme(Some(theme));
                for webview_id in self.tab_manager.tab_ids() {
                    let _ = sender.send(EmbedderToConstellationMessage::ThemeChange(
                        webview_id, theme,
                    ));
                }
                if let Some(panel) = &self.panel {
                    let _ = sender.send(EmbedderToConstellationMessage::ThemeChange(
                        panel.webview.webview_id,
                        theme,
                    ));
                }
            }
            e => log::trace!("Verso Window isn't supporting this window event yet: {e:?}"),
        }
    }

    /// Handle Window keyboard shortcut
    ///
    /// - Returns `true` if the event is handled, then we should skip sending it to constellation
    fn handle_keyboard_shortcut(
        &mut self,
        compositor: &mut IOCompositor,
        event: &KeyboardEvent,
        javascript_evaluator: &mut JavaScriptEvaluator,
    ) -> bool {
        let is_macos = cfg!(target_os = "macos");
        let control_or_meta = if is_macos {
            Modifiers::META
        } else {
            Modifiers::CONTROL
        };

        if event.state == KeyState::Down {
            // TODO: New Window, Close Browser
            match (event.modifiers, event.code) {
                (modifiers, Code::KeyT) if modifiers == control_or_meta => {
                    (*self).create_tab(
                        &compositor.constellation_chan,
                        ServoUrl::parse("https://example.com").unwrap(),
                        javascript_evaluator,
                    );
                    return true;
                }
                (modifiers, Code::KeyW) if modifiers == control_or_meta => {
                    if let Some(tab_id) = self.tab_manager.current_tab_id() {
                        (*self).close_tab(compositor, tab_id, javascript_evaluator);
                    }
                    return true;
                }
                _ => (),
            }
        }

        false
    }

    /// Handle servo messages. Return true if it requests a new window
    pub fn handle_servo_message(
        &mut self,
        webview_id: WebViewId,
        message: EmbedderMsg,
        sender: &Sender<EmbedderToConstellationMessage>,
        to_controller_sender: &Option<IpcSender<ToControllerMessage>>,
        clipboard: Option<&mut Clipboard>,
        compositor: &mut IOCompositor,
        bookmark_manager: &mut BookmarkManager,
        javascript_evaluator: &mut JavaScriptEvaluator,
    ) -> bool {
        match message {
            EmbedderMsg::SetCursor(_, cursor) => {
                self.set_cursor_icon(cursor);
                return false;
            }
            EmbedderMsg::GetWindowRect(_, response_sender) => {
                let scale_factor = self.window.scale_factor() as f32;
                let position = self.window.outer_position().unwrap_or_default();
                let size = self.window.outer_size();
                let window_rect = DeviceIntRect::from_origin_and_size(
                    Point2D::new(position.x, position.y),
                    Size2D::new(size.width, size.height).to_i32(),
                );
                if let Err(error) = response_sender.send(convert_rect_to_css_pixel(
                    window_rect,
                    Scale::new(scale_factor),
                )) {
                    log::error!("Failed to respond to GetWindowRect: {error}");
                }
                return false;
            }
            EmbedderMsg::GetScreenMetrics(_, response_sender) => {
                let monitor = self.window.current_monitor();
                let screen_size = monitor
                    .as_ref()
                    .map(|monitor| monitor.size().cast())
                    .unwrap_or_default();
                let scale_factor = monitor
                    .as_ref()
                    .map(|monitor| monitor.scale_factor() as f32)
                    .unwrap_or_default();
                // let toolbar_size = Size2D::new(
                //     0.0,
                //     (self.toolbar_height.get() * self.hidpi_scale_factor()).0,
                // );
                // let screen_size = self.screen_size.to_f32() * hidpi_factor;

                // // FIXME: In reality, this should subtract screen space used by the system interface
                // // elements, but it is difficult to get this value with `winit` currently. See:
                // // See https://github.com/rust-windowing/winit/issues/2494
                // let available_screen_size = screen_size - toolbar_size;

                let screen_metrics = ScreenMetrics {
                    screen_size: convert_size_to_css_pixel(
                        Size2D::new(screen_size.width, screen_size.height),
                        Scale::new(scale_factor),
                    ),
                    available_size: convert_size_to_css_pixel(
                        Size2D::new(screen_size.width, screen_size.height),
                        Scale::new(scale_factor),
                    ),
                };
                if let Err(error) = response_sender.send(screen_metrics) {
                    log::error!("Failed to respond to GetScreenMetrics: {error}");
                }
                return false;
            }
            _ => {}
        }

        // Handle message in Verso Panel
        if let Some(panel) = &self.panel {
            if panel.webview.webview_id == webview_id {
                return self.handle_servo_messages_with_panel(
                    webview_id,
                    message,
                    sender,
                    clipboard,
                    compositor,
                    bookmark_manager,
                    javascript_evaluator,
                );
            }
        }
        if let Some(webview_menu) = &self.webview_menu {
            if webview_menu.webview().webview_id == webview_id {
                self.handle_servo_messages_with_webview_menu(
                    webview_id, message, sender, clipboard, compositor,
                );
                return false;
            }
        }
        if self.tab_manager.has_prompt(webview_id) {
            self.handle_servo_messages_with_prompt(
                webview_id, message, sender, clipboard, compositor,
            );
            return false;
        }

        // Handle message in Verso WebView
        self.handle_servo_messages_with_webview(
            webview_id,
            message,
            sender,
            to_controller_sender,
            clipboard,
            compositor,
            javascript_evaluator,
        );
        false
    }

    /// Queues a Winit `WindowEvent::RedrawRequested` event to be emitted that aligns with the windowing system drawing loop.
    pub fn request_redraw(&self) {
        self.window.request_redraw()
    }

    /// Size of the window that's used by webrender.
    pub fn size(&self) -> DeviceSize {
        let size = self.window.inner_size();
        Size2D::new(size.width as f32, size.height as f32)
    }

    /// Size of the window, including the window decorations.
    pub fn outer_size(&self) -> DeviceSize {
        let size = self.window.outer_size();
        Size2D::new(size.width as f32, size.height as f32)
    }

    /// Get Winit window ID of the window.
    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    /// Scale factor of the window. This is also known as HIDPI.
    pub fn scale_factor(&self) -> f64 {
        self.window.scale_factor()
    }

    /// Check if the window has such webview.
    pub fn has_webview(&self, id: WebViewId) -> bool {
        if self
            .webview_menu
            .as_ref()
            .is_some_and(|w| w.webview().webview_id == id)
        {
            return true;
        }

        if self.tab_manager.has_prompt(id) {
            return true;
        }

        if let Some(panel) = &self.panel {
            if panel.webview.webview_id == id {
                return true;
            }
        }

        if self.tab_manager.tab(id).is_some() {
            return true;
        }

        false
    }

    /// Remove the webview in this window by provided webview ID.
    /// If provided ID is the panel, it will shut down the compositor and then close whole application.
    pub fn remove_webview(
        &mut self,
        id: WebViewId,
        compositor: &mut IOCompositor,
    ) -> (Option<WebView>, bool) {
        if self
            .webview_menu
            .as_ref()
            .filter(|menu| menu.webview().webview_id == id)
            .is_some()
        {
            let webview_menu = self.webview_menu.take().expect("Context menu should exist");
            return (Some(webview_menu.webview().clone()), false);
        }

        if let Some(prompt) = self.tab_manager.remove_prompt_by_prompt_id(id) {
            return (Some(prompt.webview().clone()), false);
        }

        if self
            .panel
            .as_ref()
            .filter(|w| w.webview.webview_id == id)
            .is_some()
        {
            // Removing panel, remove all webviews and shut down the compositor
            let tab_ids = self.tab_manager.tab_ids();
            for tab_id in tab_ids {
                send_to_constellation(
                    &compositor.constellation_chan,
                    EmbedderToConstellationMessage::CloseWebView(tab_id),
                );
            }
            (self.panel.take().map(|panel| panel.webview), false)
        } else if let Ok(tab) = self.tab_manager.close_tab(id) {
            let close_window = self.tab_manager.count() == 0 || self.panel.is_none();
            if self.focused_webview_id == Some(id) {
                self.focused_webview_id = None;
            }
            (Some(tab.webview().clone()), close_window)
        } else {
            (None, false)
        }
    }

    /// Get the painting order of this window.
    pub fn painting_order(&self) -> Vec<&WebView> {
        let mut order = vec![];
        if let Some(panel) = &self.panel {
            order.push(&panel.webview);
        }

        if let Some(tab) = self.tab_manager.current_tab() {
            order.push(tab.webview());
        }

        if let Some(webview_menu) = &self.webview_menu {
            order.push(webview_menu.webview());
        }

        if let Some(prompt) = self.tab_manager.current_prompt() {
            order.push(prompt.webview());
        }

        order
    }

    /// Set cursor icon of the window.
    pub fn set_cursor_icon(&mut self, cursor: Cursor) {
        let winit_cursor = match cursor {
            Cursor::Default => CursorIcon::Default,
            Cursor::Pointer => CursorIcon::Pointer,
            Cursor::ContextMenu => CursorIcon::ContextMenu,
            Cursor::Help => CursorIcon::Help,
            Cursor::Progress => CursorIcon::Progress,
            Cursor::Wait => CursorIcon::Wait,
            Cursor::Cell => CursorIcon::Cell,
            Cursor::Crosshair => CursorIcon::Crosshair,
            Cursor::Text => CursorIcon::Text,
            Cursor::VerticalText => CursorIcon::VerticalText,
            Cursor::Alias => CursorIcon::Alias,
            Cursor::Copy => CursorIcon::Copy,
            Cursor::Move => CursorIcon::Move,
            Cursor::NoDrop => CursorIcon::NoDrop,
            Cursor::NotAllowed => CursorIcon::NotAllowed,
            Cursor::Grab => CursorIcon::Grab,
            Cursor::Grabbing => CursorIcon::Grabbing,
            Cursor::EResize => CursorIcon::EResize,
            Cursor::NResize => CursorIcon::NResize,
            Cursor::NeResize => CursorIcon::NeResize,
            Cursor::NwResize => CursorIcon::NwResize,
            Cursor::SResize => CursorIcon::SResize,
            Cursor::SeResize => CursorIcon::SeResize,
            Cursor::SwResize => CursorIcon::SwResize,
            Cursor::WResize => CursorIcon::WResize,
            Cursor::EwResize => CursorIcon::EwResize,
            Cursor::NsResize => CursorIcon::NsResize,
            Cursor::NeswResize => CursorIcon::NeswResize,
            Cursor::NwseResize => CursorIcon::NwseResize,
            Cursor::ColResize => CursorIcon::ColResize,
            Cursor::RowResize => CursorIcon::RowResize,
            Cursor::AllScroll => CursorIcon::AllScroll,
            Cursor::ZoomIn => CursorIcon::ZoomIn,
            Cursor::ZoomOut => CursorIcon::ZoomOut,
            Cursor::None => {
                self.window.set_cursor_visible(false);
                return;
            }
        };
        self.cursor_state.current_cursor = winit_cursor;
        self.window.set_cursor(winit_cursor);
        self.window.set_cursor_visible(true);
    }

    /// This method enables IME and set the IME cursor area of the window.
    /// The position is in logical position so it'll scale according to screen scaling factor.
    pub fn show_ime(
        &self,
        _input_method_typee: embedder_traits::InputMethodType,
        _text: Option<(String, i32)>,
        _multilinee: bool,
        position: euclid::Box2D<i32, webrender_api::units::DevicePixel>,
        show_bookmark: bool,
    ) {
        self.window.set_ime_allowed(true);
        let mut height: f64 = PANEL_HEIGHT + PANEL_PADDING;
        if self.tab_manager.count() > 1 {
            height += TAB_HEIGHT;
        }
        if show_bookmark {
            height += BOOKMARK_HEIGHT;
        }
        self.window.set_ime_cursor_area(
            LogicalPosition::new(position.min.x, position.min.y + height as i32),
            LogicalSize::new(0, position.max.y - position.min.y),
        );
    }

    /// This method disables IME of the window.
    pub fn hide_ime(&self) {
        self.window.set_ime_allowed(false);
    }

    /// Show notification
    pub fn show_notification(&self, notification: &Notification) {
        let mut display_notification = notify_rust::Notification::new();

        display_notification
            .summary(&notification.title)
            .body(&notification.body);

        #[cfg(linux)]
        {
            if let Some(icon_image) = notification.icon_resource.as_ref().and_then(|icon| {
                Image::from_rgba(
                    icon.metadata.width as i32,
                    icon.metadata.height as i32,
                    icon.bytes.to_vec(),
                )
                .ok()
            }) {
                display_notification.image_data(icon_image);
            }
        }

        #[cfg(linux)]
        std::thread::spawn(move || {
            if let Ok(handle) = display_notification.show() {
                // prevent handler dropped immediately which will close the notification as well
                handle.on_close(|| {});
            }
        });
        #[cfg(not(linux))]
        let _ = display_notification.show();
    }

    /// Close window's webview menu
    pub(crate) fn close_webview_menu(&mut self, sender: &Sender<EmbedderToConstellationMessage>) {
        if let Some(menu) = self.webview_menu.as_mut() {
            menu.close(sender);
        }
    }

    pub(crate) fn notify_theme_change(
        &self,
        constellation_sender: &Sender<EmbedderToConstellationMessage>,
        webview_id: WebViewId,
    ) {
        send_to_constellation(
            constellation_sender,
            EmbedderToConstellationMessage::ThemeChange(
                webview_id,
                to_servo_theme(self.window.theme().as_ref()),
            ),
        );
    }

    /// Forward mouse move event to the first webview under the position,
    /// returns the [`WebViewId`] of that webview
    fn forward_mouse_move(
        &self,
        compositor: &mut IOCompositor,
        sender: &Sender<EmbedderToConstellationMessage>,
        point: Point2D<f32, DevicePixel>,
    ) -> Option<WebViewId> {
        for webview in [
            &self.tab_manager.current_tab().map(|tab| tab.webview()),
            &self.panel.as_ref().map(|panel| &panel.webview),
        ]
        .into_iter()
        .filter_map(|webview| *webview)
        {
            if !webview.rect.contains(point) {
                continue;
            }

            forward_input_event(
                compositor,
                webview.webview_id,
                sender,
                InputEvent::MouseMove(MouseMoveEvent::new(point)),
            );

            return Some(webview.webview_id);
        }

        None
    }
}

// Prompt methods
impl Window {
    /// Close window's prompt dialog
    pub(crate) fn close_prompt_dialog(&mut self, tab_id: WebViewId) {
        if let Some(sender) = self
            .tab_manager
            .remove_prompt_by_tab_id(tab_id)
            .and_then(|prompt| prompt.sender())
        {
            match sender {
                PromptSender::AlertSender(sender) => {
                    let _ = sender.send(AlertResponse::default());
                }
                PromptSender::ConfirmSender(sender) => {
                    let _ = sender.send(ConfirmResponse::default());
                }
                PromptSender::InputSender(sender) => {
                    let _ = sender.send(PromptResponse::default());
                }
                PromptSender::AllowDenySender(sender) => {
                    let _ = sender.send(AllowOrDeny::Deny);
                }
                PromptSender::HttpBasicAuthSender(sender) => {
                    let _ = sender.send(None);
                }
            }
        }
    }
}

// Non-decorated window resizing for Windows and Linux.
#[cfg(any(linux, target_os = "windows"))]
impl Window {
    /// Check current window state is allowed to apply drag-resize in client region.
    fn should_use_client_region_drag(&self) -> bool {
        !self.window.is_decorated()
            && !self.window.is_maximized()
            && self.window.is_resizable()
            && self.window.fullscreen().is_none()
    }

    /// Drag resize the window.
    fn drag_resize_window(&self) {
        if let Some(direction) = self.get_drag_resize_direction() {
            if let Err(err) = self.window.drag_resize_window(direction) {
                log::error!("Failed to drag-resize window: {:?}", err);
            }
        }
    }

    /// Get drag-resize direction.
    fn get_drag_resize_direction(&self) -> Option<ResizeDirection> {
        let mouse_position = match self.mouse_position.get() {
            Some(position) => position,
            None => {
                return None;
            }
        };

        let window_size = self.window.outer_size();
        let border_size = 5.0 * self.window.scale_factor();

        let x_direction = if mouse_position.x < border_size {
            Some(ResizeDirection::West)
        } else if mouse_position.x > (window_size.width as f64 - border_size) {
            Some(ResizeDirection::East)
        } else {
            None
        };

        let y_direction = if mouse_position.y < border_size {
            Some(ResizeDirection::North)
        } else if mouse_position.y > (window_size.height as f64 - border_size) {
            Some(ResizeDirection::South)
        } else {
            None
        };

        let direction = match (x_direction, y_direction) {
            (Some(ResizeDirection::East), None) => ResizeDirection::East,
            (Some(ResizeDirection::West), None) => ResizeDirection::West,
            (None, Some(ResizeDirection::South)) => ResizeDirection::South,
            (None, Some(ResizeDirection::North)) => ResizeDirection::North,
            (Some(ResizeDirection::East), Some(ResizeDirection::North)) => {
                ResizeDirection::NorthEast
            }
            (Some(ResizeDirection::West), Some(ResizeDirection::North)) => {
                ResizeDirection::NorthWest
            }
            (Some(ResizeDirection::East), Some(ResizeDirection::South)) => {
                ResizeDirection::SouthEast
            }
            (Some(ResizeDirection::West), Some(ResizeDirection::South)) => {
                ResizeDirection::SouthWest
            }
            _ => return None,
        };

        Some(direction)
    }

    /// Set drag-resize cursor when mouse is hover on the border of the window.
    fn set_drag_resize_cursor(&mut self, direction: Option<ResizeDirection>) {
        if let Some(direction) = direction {
            let cursor = match direction {
                ResizeDirection::East => CursorIcon::EResize,
                ResizeDirection::West => CursorIcon::WResize,
                ResizeDirection::South => CursorIcon::SResize,
                ResizeDirection::North => CursorIcon::NResize,
                ResizeDirection::NorthEast => CursorIcon::NeResize,
                ResizeDirection::NorthWest => CursorIcon::NwResize,
                ResizeDirection::SouthEast => CursorIcon::SeResize,
                ResizeDirection::SouthWest => CursorIcon::SwResize,
            };
            self.cursor_state.cursor_resizing = true;
            self.window.set_cursor(cursor);
        } else if self.cursor_state.cursor_resizing {
            self.cursor_state.cursor_resizing = false;
            self.window.set_cursor(self.cursor_state.current_cursor);
        }
    }
}

/* window decoration */
#[cfg(macos)]
use objc2::runtime::AnyObject;
#[cfg(macos)]
use raw_window_handle::{AppKitWindowHandle, RawWindowHandle};

/// Window decoration for macOS.
#[cfg(macos)]
pub unsafe fn decorate_window(view: *mut AnyObject, _position: LogicalPosition<f64>) {
    use objc2::rc::Id;
    use objc2_app_kit::{NSView, NSWindowStyleMask, NSWindowTitleVisibility};

    let ns_view: Id<NSView> = unsafe { Id::retain(view.cast()) }.unwrap();
    let window = ns_view
        .window()
        .expect("view was not installed in a window");
    window.setMovable(false); // let panel UI handle window moving
    window.setTitlebarAppearsTransparent(true);
    window.setTitleVisibility(NSWindowTitleVisibility::NSWindowTitleHidden);
    window.setStyleMask(
        NSWindowStyleMask::Titled
            | NSWindowStyleMask::FullSizeContentView
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Resizable
            | NSWindowStyleMask::Miniaturizable,
    );
}

/// Forward input event to compositor or constellation.
fn forward_input_event(
    compositor: &mut IOCompositor,
    webview_id: WebViewId,
    constellation_proxy: &Sender<EmbedderToConstellationMessage>,
    event: InputEvent,
) {
    // Events with a `point` first go to the compositor for hit testing.
    if event.point().is_some() {
        compositor.on_input_event(webview_id, event);
        return;
    }

    let _ = constellation_proxy.send(EmbedderToConstellationMessage::ForwardInputEvent(
        webview_id, event, None, /* hit_test */
    ));
}

fn to_servo_theme(theme: Option<&winit::window::Theme>) -> embedder_traits::Theme {
    match theme {
        Some(winit::window::Theme::Dark) => embedder_traits::Theme::Dark,
        Some(winit::window::Theme::Light) | None => embedder_traits::Theme::Light,
    }
}
