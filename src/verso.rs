use crossbeam_channel::{Receiver, Sender, unbounded};
use ipc_channel::ipc::{self, IpcSender};
use ipc_channel::router::ROUTER;
use servo::{
    EmbedderMsg, EmbedderProxy, EventLoopWaker, UserContentManager, WebResourceResponse,
    WebResourceResponseMsg,
};

use paint_api::{CrossProcessPaintApi, PaintMessage, PaintProxy};
use servo_base::{
    generic_channel::GenericCallback, generic_channel::RoutedReceiver, id::WebViewId,
};
use servo_constellation_traits::EmbedderToConstellationMessage;
use std::{cell::RefCell, collections::HashMap, fmt::Debug, rc::Rc};
//use net::resource_thread;
//use script::JSEngineSetup;
use servo::{AllowOrDenyRequest, Servo, ServoBuilder, ServoDelegate, ServoError};
use versoview_messages::{PositionType, SizeType, ToControllerMessage, ToVersoMessage};
use winit::{
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy},
    window::WindowId,
};

use crate::{
    config::{Config, parse_cli_args, to_winit_theme, to_winit_window_level},
    window::Window,
};

/// Main entry point of Verso browser.
pub struct Verso {
    pub(crate) servo: Servo,
    pub(crate) windows: RefCell<HashMap<WindowId, Window>>,
    to_controller_sender: Option<IpcSender<ToControllerMessage>>,
    // embedder_receiver: Receiver<EmbedderMsg>,
    // webdriver_receiver: Option<Receiver<WebDriverCommandMsg>>,
    config: Config,
    // bookmark_manager: BookmarkManager,
    // servo_delegate: VersoServoDelegate,
}

struct VersoServoDelegate;
impl ServoDelegate for VersoServoDelegate {
    fn notify_devtools_server_started(&self, port: u16, _token: String) {
        log::info!("Devtools Server running on port {port}");
    }

    fn request_devtools_connection(&self, request: AllowOrDenyRequest) {
        request.allow();
    }

    fn notify_error(&self, error: ServoError) {
        log::error!("Saw Servo error: {error:?}!");
    }
}

impl Verso {
    /// Create a Verso instance from Winit's window and event loop proxy.
    ///
    /// Following threads will be created while initializing Verso based on configurations:
    /// - Time Profiler: Enabled
    /// - Memory Profiler: Enabled
    /// - DevTools: `pref!(devtools_server_enabled)`
    /// - Webrender: Enabled
    /// - WebGL: Disabled
    /// - WebXR: Disabled
    /// - Bluetooth: Enabled
    /// - Resource: Enabled
    /// - Storage: Enabled
    /// - Font Cache: Enabled
    /// - Canvas: Enabled
    /// - Constellation: Enabled
    /// - Image Cache: Enabled
    pub fn new(evl: &ActiveEventLoop, proxy: EventLoopProxy<EventLoopProxyMessage>) -> Rc<Self> {
        let (config, to_controller_sender) = try_connect_ipc_and_get_config(&proxy);

        // Initialize configurations and Verso window
        let protocols = config.create_protocols();
        let initial_url = config.url.clone();
        let with_panel = config.with_panel;
        let window_settings = config.window_attributes.clone();
        let user_scripts = config.user_scripts.clone();
        let zoom_level = config.zoom_level;

        let mut window = Window::new(evl, window_settings);
        let event_loop_waker = Box::new(Waker(proxy));

        let (opts, preferences) = config.init();

        let servo_builder = ServoBuilder::default()
            .opts(opts)
            .preferences(preferences)
            .protocol_registry(protocols)
            .event_loop_waker(event_loop_waker);
        let servo: Servo = servo_builder.build();
        servo.setup_logging();
        servo.set_delegate(Rc::new(VersoServoDelegate));

        let mut user_content_manager = UserContentManager::new(&servo);
        for script in user_scripts {
            user_content_manager.add_script(Rc::new(script));
        }

        // servo.user_content_manager(user_content_manager);

        let windows = HashMap::new();

        // Create Verso instance
        let verso = Rc::new(Verso {
            servo,
            windows: windows.into(),
            to_controller_sender,
            // embedder_receiver,
            // webdriver_receiver,
            config,
            // bookmark_manager: BookmarkManager::new(),
        });

        if with_panel {
            window.create_panel(&verso, initial_url);
        } else {
            window.create_tab(&verso, initial_url);
        }

        verso.windows.borrow_mut().insert(window.id(), window);

        verso
    }

    /// Handle Winit window events. The strategy to handle event are different between platforms
    /// because the order of events might be different.
    pub fn handle_window_event(
        self: &mut Rc<Self>,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        #[cfg(linux)]
        if let WindowEvent::Resized(_) = event {
            self.handle_winit_window_event(event_loop, window_id, event);
        } else {
            self.handle_winit_window_event(event_loop, window_id, event);
            self.handle_servo_messages(event_loop);
        }

        #[cfg(apple)]
        if let WindowEvent::RedrawRequested = event {
            let resizing = self.handle_winit_window_event(event_loop, window_id, event);
            if !resizing {
                self.handle_servo_messages(event_loop);
            }
        } else {
            self.handle_winit_window_event(event_loop, window_id, event);
            self.handle_servo_messages(event_loop);
        }

        #[cfg(windows)]
        {
            self.handle_winit_window_event(event_loop, window_id, event);
            self.handle_servo_messages(event_loop);
        }

        // self.handle_webdriver_messages();
    }

    /// Handle Winit window events
    fn handle_winit_window_event(
        self: &mut Rc<Self>,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) -> bool {
        log::trace!("Verso is handling Winit event: {event:?}");

        let mut windows = self.windows.borrow_mut();
        let Some(window) = windows.get_mut(&window_id) else {
            return false;
        };

        if let WindowEvent::CloseRequested = event {
            if let Some(to_controller_sender) = &self.to_controller_sender {
                if window.event_listeners.on_close_requested {
                    if let Err(error) =
                        to_controller_sender.send(ToControllerMessage::OnCloseRequested)
                    {
                        log::error!(
                            "Verso failed to send WebResourceRequested to controller: {error}"
                        );
                    } else {
                        return false;
                    }
                }
            }
            // self.windows.remove(&window_id);
            drop(&self.servo);
            event_loop.exit();
        } else {
            window.handle_winit_window_event(&self, &event);
            return window.resizing;
        }

        false
    }

    /// Handle message came from Servo.
    pub fn handle_servo_messages(self: &mut Rc<Self>, event_loop: &ActiveEventLoop) {
        let should_shutdown = self.servo.spin_event_loop();

        // Only handle incoming embedder messages if the compositor hasn't already started shutting down.
        // while let Ok(msg) = self.embedder_receiver.try_recv() {
        //     if let Some(webview_id) = Self::get_embedder_message_webview_id(&msg) {
        //         if let Some((window, document_id)) = self
        //             .windows
        //             .values_mut()
        //             .find(|window| window.has_webview(*webview_id))
        //         {
        //             if window.handle_servo_message(
        //                 *webview_id,
        //                 msg,
        //                 &self.constellation_sender,
        //                 &self.to_controller_sender,
        //                 self.clipboard.as_mut(),
        //                 &mut self.bookmark_manager,
        //             ) {
        //                 let mut window = Window::new_with_compositor(
        //                     evl,
        //                     self.config.window_attributes.clone(),
        //                     compositor,
        //                 );
        //                 window.create_panel(&self.constellation_sender, self.config.url.clone());
        //                 let webrender_document = *document_id;
        //                 self.windows
        //                     .insert(window.id(), (window, webrender_document));
        //             }
        //         }
        //     }
        // }

        // Check if Verso need to start shutting down.
        // if self.windows.is_empty() {
        //     compositor.start_shutting_down();
        // }

        if should_shutdown {
            event_loop.exit();
        } else if self.is_animating() {
            event_loop.set_control_flow(ControlFlow::Poll);
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }

    fn get_embedder_message_webview_id(msg: &EmbedderMsg) -> Option<&WebViewId> {
        match msg {
            EmbedderMsg::Status(webview_id, ..) => Some(webview_id),
            EmbedderMsg::ChangePageTitle(webview_id, ..) => Some(webview_id),
            EmbedderMsg::MoveTo(webview_id, ..) => Some(webview_id),
            EmbedderMsg::ResizeTo(webview_id, ..) => Some(webview_id),
            EmbedderMsg::ShowSimpleDialog(webview_id, ..) => Some(webview_id),
            EmbedderMsg::AllowUnload(webview_id, ..) => Some(webview_id),
            EmbedderMsg::ClearClipboard(webview_id) => Some(webview_id),
            EmbedderMsg::GetClipboardText(webview_id, ..) => Some(webview_id),
            EmbedderMsg::SetClipboardText(webview_id, ..) => Some(webview_id),
            EmbedderMsg::AllowProtocolHandlerRequest(webview_id, ..) => Some(webview_id),
            EmbedderMsg::ShowConsoleApiMessage(webview_id, ..) => webview_id.as_ref(),
            EmbedderMsg::InputEventsHandled(webview_id, ..) => Some(webview_id),
            EmbedderMsg::AccessibilityTreeUpdate(webview_id, ..) => Some(webview_id),
            EmbedderMsg::SetCursor(webview_id, ..) => Some(webview_id),
            EmbedderMsg::NewFavicon(webview_id, ..) => Some(webview_id),
            EmbedderMsg::NotifyFullscreenStateChanged(webview_id, ..) => Some(webview_id),
            EmbedderMsg::NotifyLoadStatusChanged(webview_id, ..) => Some(webview_id),
            EmbedderMsg::GetSelectedBluetoothDevice(webview_id, ..) => Some(webview_id),
            EmbedderMsg::PromptPermission(webview_id, ..) => Some(webview_id),
            EmbedderMsg::OnDevtoolsStarted(..) => None,
            EmbedderMsg::RequestDevtoolsConnection(..) => None,
            EmbedderMsg::ShowNotification(opt_webview_id, ..) => opt_webview_id.as_ref(),
            EmbedderMsg::GetWindowRect(webview_id, ..) => Some(webview_id),
            EmbedderMsg::GetScreenMetrics(webview_id, ..) => Some(webview_id),
            EmbedderMsg::ShowEmbedderControl(..) => None,
            EmbedderMsg::HideEmbedderControl(embedder_control_id) => {
                Some(&embedder_control_id.webview_id)
            }
        }
    }

    // TODO: Implement this
    /// Handle webdriver messages
    // pub fn handle_webdriver_messages(&self) {
    //     let Some(webdriver_receiver) = &self.webdriver_receiver else {
    //         return;
    //     };

    //     while let Ok(msg) = webdriver_receiver.try_recv() {
    //         match msg {
    //             WebDriverCommandMsg::KeyboardAction(..)
    //             | WebDriverCommandMsg::MouseButtonAction(..)
    //             | WebDriverCommandMsg::MouseMoveAction(..)
    //             | WebDriverCommandMsg::WheelScrollAction(..)
    //             | WebDriverCommandMsg::ScriptCommand(..)
    //             | WebDriverCommandMsg::TakeScreenshot(..) => {
    //                 log::warn!(
    //                     "WebDriverCommand {msg:?} is still not moved from constellation to embedder"
    //                 );
    //             }
    //             _ => log::warn!("WebDriverCommand {msg:?} is not supported yet"),
    //         };
    //     }
    // }

    /// Request Verso to redraw. It will queue a redraw event on current focused window.
    pub fn request_redraw(self: &mut Rc<Self>, event_loop: &ActiveEventLoop) {
        // let Some(compositor) = &mut self.compositor else {
        //     return;
        // };

        // if let Some(window) = self.windows.get(&compositor.current_window) {
        //     // evl.set_control_flow(ControlFlow::Poll);
        //     window.request_redraw();

        //     // Wait for `request_redraw` to trigger the next event loop if the window is visible
        //     if window.window.is_visible().unwrap_or(true) {
        //         return;
        //     }
        // }

        self.handle_servo_messages(event_loop);
    }

    /// Handle message came from webview controller.
    pub fn handle_incoming_webview_message(
        self: &mut Rc<Self>,
        event_loop: &ActiveEventLoop,
        message: ToVersoMessage,
    ) {
        // We only use the first window for now
        let mut windows = self.windows.borrow_mut();
        let Some(window) = windows.values_mut().next().map(|window| window) else {
            return;
        };
        // We only use the first webview for now
        let webview = window.tab_manager.current_tab().map(|tab| tab.webview());

        match message {
            ToVersoMessage::Exit => {
                // if let Some(compositor) = &mut self.compositor {
                //     compositor.start_shutting_down();
                //     self.handle_servo_messages(event_loop);
                // }
                event_loop.exit();
            }
            ToVersoMessage::ListenToOnCloseRequested => {
                window.event_listeners.on_close_requested = true;
            }
            ToVersoMessage::NavigateTo(to_url) => {
                if let Some(webview) = webview {
                    webview.load(to_url);
                }
            }
            ToVersoMessage::Reload => {
                if let Some(webview) = webview {
                    webview.reload();
                }
            }
            ToVersoMessage::ListenToOnNavigationStarting => {
                window.event_listeners.on_navigation_starting = true;
            }
            ToVersoMessage::OnNavigationStartingResponse(id, allow) => {
                // send_to_constellation(
                //     &self.constellation_sender,
                //     EmbedderToConstellationMessage::AllowNavigationResponse(
                //         bincode::deserialize(&id).unwrap(),
                //         allow,
                //     ),
                // );
            }
            ToVersoMessage::ExecuteScript(js) => {
                if let Some(webview) = webview {
                    webview.evaluate_javascript(js, |_| {});
                }
            }
            ToVersoMessage::ListenToWebResourceRequests => {
                window
                    .event_listeners
                    .on_web_resource_requested
                    .replace(HashMap::new());
            }
            ToVersoMessage::WebResourceRequestResponse(response) => {
                if let Some((url, sender)) = window
                    .event_listeners
                    .on_web_resource_requested
                    .as_mut()
                    .and_then(|senders| senders.remove(&response.id))
                {
                    if let Some(response) = response.response {
                        let _ = sender
                            .send(WebResourceResponseMsg::Start(
                                WebResourceResponse::new(url)
                                    .headers(response.headers().clone())
                                    .status_code(response.status()),
                            ))
                            .and_then(|_| {
                                sender.send(WebResourceResponseMsg::SendBodyData(
                                    response.into_body(),
                                ))
                            })
                            .and_then(|_| sender.send(WebResourceResponseMsg::FinishLoad));
                    } else {
                        let _ = sender.send(WebResourceResponseMsg::DoNotIntercept);
                    }
                }
            }
            ToVersoMessage::SetTitle(title) => {
                window.window.set_title(&title);
            }
            ToVersoMessage::SetSize(size) => {
                let _ = window.window.request_inner_size(size);
            }
            ToVersoMessage::SetPosition(position) => {
                window.window.set_outer_position(position);
            }
            ToVersoMessage::SetMaximized(maximized) => {
                window.window.set_maximized(maximized);
            }
            ToVersoMessage::SetMinimized(minimized) => {
                window.window.set_minimized(minimized);
            }
            ToVersoMessage::SetFullscreen(fullscreen) => {
                window.window.set_fullscreen(if fullscreen {
                    Some(winit::window::Fullscreen::Borderless(None))
                } else {
                    None
                });
            }
            ToVersoMessage::SetVisible(visible) => {
                window.window.set_visible(visible);
            }
            ToVersoMessage::SetWindowLevel(window_level) => {
                window
                    .window
                    .set_window_level(to_winit_window_level(window_level));
            }
            ToVersoMessage::SetTheme(theme) => {
                window.window.set_theme(to_winit_theme(&theme));
            }
            ToVersoMessage::StartDragging => {
                let _ = window.window.drag_window();
            }
            ToVersoMessage::Focus => {
                window.window.focus_window();
            }
            ToVersoMessage::GetTitle(id) => {
                if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                    ToControllerMessage::GetTitleResponse(id, window.window.title()),
                ) {
                    log::error!("Verso failed to send GetTitleReponse to controller: {error}")
                }
            }
            ToVersoMessage::GetSize(id, size_type) => {
                if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                    ToControllerMessage::GetSizeResponse(
                        id,
                        match size_type {
                            SizeType::Inner => window.window.inner_size(),
                            SizeType::Outer => window.window.outer_size(),
                        },
                    ),
                ) {
                    log::error!("Verso failed to send GetSizeReponse to controller: {error}")
                }
            }
            ToVersoMessage::GetPosition(id, position_type) => {
                if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                    ToControllerMessage::GetPositionResponse(
                        id,
                        match position_type {
                            PositionType::Inner => window.window.inner_position(),
                            PositionType::Outer => window.window.outer_position(),
                        }
                        .ok(),
                    ),
                ) {
                    log::error!("Verso failed to send GetPositionResponse to controller: {error}")
                }
            }
            ToVersoMessage::GetMinimized(id) => {
                if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                    ToControllerMessage::GetMinimizedResponse(
                        id,
                        window.window.is_minimized().unwrap_or_default(),
                    ),
                ) {
                    log::error!("Verso failed to send GetMinimizedResponse to controller: {error}")
                }
            }
            ToVersoMessage::GetMaximized(id) => {
                if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                    ToControllerMessage::GetMaximizedResponse(id, window.window.is_maximized()),
                ) {
                    log::error!("Verso failed to send GetMaximizedResponse to controller: {error}")
                }
            }
            ToVersoMessage::GetFullscreen(id) => {
                if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                    ToControllerMessage::GetFullscreenResponse(
                        id,
                        window.window.fullscreen().is_some(),
                    ),
                ) {
                    log::error!("Verso failed to send GetFullscreenResponse to controller: {error}")
                }
            }
            ToVersoMessage::GetVisible(id) => {
                if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                    ToControllerMessage::GetVisibleResponse(
                        id,
                        window.window.is_visible().unwrap_or(true),
                    ),
                ) {
                    log::error!("Verso failed to send GetVisibleResponse to controller: {error}")
                }
            }
            ToVersoMessage::GetScaleFactor(id) => {
                if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                    ToControllerMessage::GetScaleFactorResponse(id, window.window.scale_factor()),
                ) {
                    log::error!(
                        "Verso failed to send GetScaleFactorResponse to controller: {error}"
                    )
                }
            }
            ToVersoMessage::GetTheme(id) => {
                if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                    ToControllerMessage::GetThemeResponse(
                        id,
                        match window.window.theme() {
                            Some(winit::window::Theme::Dark) => versoview_messages::Theme::Dark,
                            _ => versoview_messages::Theme::Light,
                        },
                    ),
                ) {
                    log::error!("Verso failed to send GetThemeResponse to controller: {error}")
                }
            }
            ToVersoMessage::GetCurrentUrl(id) => {
                let tab = window.tab_manager.current_tab().unwrap();
                let history = tab.history();
                if let Err(error) = self.to_controller_sender.as_ref().unwrap().send(
                    ToControllerMessage::GetCurrentUrlResponse(
                        id,
                        history.list[history.current_idx].as_url().clone(),
                    ),
                ) {
                    log::error!(
                        "Verso failed to send GetScaleFactorResponse to controller: {error}"
                    )
                }
            }
            ToVersoMessage::SetConfig(..) => {
                log::error!(
                    "`ToVersoMessage::SetConfig` should not be received after the initial setup"
                )
            }
        }
    }

    /// Return true if one of the Verso windows is animating.
    pub fn is_animating(&self) -> bool {
        self.windows
            .borrow()
            .values()
            .flat_map(|vw| vw.tab_manager.tabs())
            .any(|tab| tab.webview().clone().animating())
    }
}

/// Parse the command line arguments,
/// if `ipc_channel` is set, we try to connect to it and set up routing to the event loop proxy
/// then return the config from [`ToVersoMessage::SetConfig`] or fallback to from the command line arguments
fn try_connect_ipc_and_get_config(
    proxy: &EventLoopProxy<EventLoopProxyMessage>,
) -> (Config, Option<IpcSender<ToControllerMessage>>) {
    let cli_args = parse_cli_args().unwrap_or_default();
    let (to_controller_sender, initial_settings) = if let Some(ipc_channel) = &cli_args.ipc_channel
    {
        let sender = IpcSender::<ToControllerMessage>::connect(ipc_channel.to_string()).unwrap();
        let (to_verso_sender, receiver) = ipc::channel::<ToVersoMessage>().unwrap();
        sender
            .send(ToControllerMessage::SetToVersoSender(to_verso_sender))
            .unwrap();
        let ToVersoMessage::SetConfig(initial_settings) = receiver
            .recv()
            .expect("Failed to recieve the initial settings from controller")
        else {
            panic!("The initial message sent from versoview is not a `ToVersoMessage::SetConfig`")
        };
        let proxy_clone = EventLoopProxyDropGuard(proxy.clone());
        ROUTER.add_typed_route(
            receiver,
            Box::new(move |message| match message {
                Ok(message) => {
                    if let Err(e) = proxy_clone
                        .0
                        .send_event(EventLoopProxyMessage::IpcMessage(Box::new(message)))
                    {
                        log::error!("Failed to send controller message to Verso: {e}");
                    }
                }
                Err(e) => log::error!("Failed to receive controller message: {e}"),
            }),
        );
        (Some(sender), Some(initial_settings))
    } else {
        (None, None)
    };
    let config = if let Some(initial_settings) = initial_settings {
        Config::from_controller_config(initial_settings)
    } else {
        Config::from_cli_args(cli_args)
    };
    (config, to_controller_sender)
}

/// Signal the event loop to exit when dropped,
/// this is used for when we lose the connection with our host controller
struct EventLoopProxyDropGuard(EventLoopProxy<EventLoopProxyMessage>);

impl Drop for EventLoopProxyDropGuard {
    fn drop(&mut self) {
        if let Err(error) = self
            .0
            .send_event(EventLoopProxyMessage::IpcMessage(Box::new(
                ToVersoMessage::Exit,
            )))
        {
            log::error!("Failed to send exit message to event loop on IPC disconnect: {error}");
        }
    }
}

/// Message send to the event loop
#[derive(Debug)]
pub enum EventLoopProxyMessage {
    /// Wake
    Wake,
    /// Message coming from the webview controller
    IpcMessage(Box<ToVersoMessage>),
}

#[derive(Debug, Clone)]
struct Waker(pub EventLoopProxy<EventLoopProxyMessage>);

impl EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(self.clone())
    }

    fn wake(&self) {
        if let Err(e) = self.0.send_event(EventLoopProxyMessage::Wake) {
            log::error!("Servo failed to send wake up event to Verso: {e}");
        }
    }
}

// A logger that logs to two downstream loggers.
// This should probably be in the log crate.

pub(crate) fn send_to_constellation(
    sender: &Sender<EmbedderToConstellationMessage>,
    msg: EmbedderToConstellationMessage,
) {
    let variant_name: &str = (&msg).into();
    if let Err(e) = sender.send(msg) {
        log::warn!("Sending {variant_name} to constellation failed: {e:?}");
    }
}

fn create_embedder_channel(
    event_loop_waker: Box<dyn EventLoopWaker>,
) -> (EmbedderProxy, Receiver<EmbedderMsg>) {
    let (sender, receiver) = unbounded();
    (
        EmbedderProxy {
            sender,
            event_loop_waker,
        },
        receiver,
    )
}

fn create_paint_channel(
    event_loop_waker: Box<dyn EventLoopWaker>,
) -> (PaintProxy, RoutedReceiver<PaintMessage>) {
    let (sender, receiver) = unbounded();

    let sender_clone = sender.clone();
    let event_loop_waker_clone = event_loop_waker.clone();
    // This callback is equivalent to `PaintProxy::send`
    let result_callback =
        move |msg: Result<PaintMessage, servo_media_player::ipc_channel::IpcError>| {
            if let Err(err) = sender_clone.send(msg) {
                log::warn!("Failed to send response ({:?}).", err);
            }
            event_loop_waker_clone.wake();
        };

    let generic_callback =
        GenericCallback::new(result_callback).expect("Failed to create callback");
    let cross_process_paint_api = CrossProcessPaintApi::new(generic_callback);

    let paint_proxy = PaintProxy {
        sender,
        cross_process_paint_api,
        event_loop_waker,
    };

    (paint_proxy, receiver)
}
