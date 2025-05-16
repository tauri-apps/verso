use std::path::PathBuf;

use dpi::{PhysicalPosition, PhysicalSize, Position, Size};
use ipc_channel::ipc;
use serde::{Deserialize, Serialize};

// Note: the reason why we didn't send `IpcSender` in those messages is because it panics on MacOS,
// see https://github.com/versotile-org/verso/pull/222#discussion_r1939111585,
// the work around is let verso send back the message through the initial sender and we map them back manually

// Can't use `PipelineId` directly or else we need to pull in servo as a dependency
type SerializedPipelineId = Vec<u8>;

/// Message sent from the controller to versoview
#[derive(Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ToVersoMessage {
    /// Initial configs for versoview
    /// this will be the first message sent to Verso once we received the sender from [`ToControllerMessage::SetToVersoSender`]
    SetConfig(ConfigFromController),
    /// Exit
    Exit,
    /// Register a listener on versoview for getting notified on close requested from the OS,
    /// veroview will send a [`ToControllerMessage::OnCloseRequested`] when that happens
    ListenToOnCloseRequested,
    /// Navigate to this URL
    NavigateTo(url::Url),
    /// Reload the current webview
    Reload,
    /// Register a listener on versoview for getting notified on navigation starting,
    /// veroview will send a [`ToControllerMessage::OnNavigationStarting`] when that happens
    ListenToOnNavigationStarting,
    /// Response to a [`ToControllerMessage::OnNavigationStarting`] message from versoview
    OnNavigationStartingResponse(SerializedPipelineId, bool),
    /// Execute JavaScript
    ExecuteScript(String),
    /// Register a listener on versoview for getting notified on web resource requests
    ListenToWebResourceRequests,
    /// Response to a [`ToControllerMessage::OnWebResourceRequested`] message from versoview
    WebResourceRequestResponse(WebResourceRequestResponse),
    /// Sets the webview window's size
    SetSize(Size),
    /// Sets the webview window's position
    SetPosition(Position),
    /// Maximize or unmaximize the window
    SetMaximized(bool),
    /// Minimize or unminimize the window
    SetMinimized(bool),
    /// Sets the window to fullscreen or back
    SetFullscreen(bool),
    /// Show or hide the window
    SetVisible(bool),
    /// Show or hide the window
    SetWindowLevel(WindowLevel),
    /// Moves the window with the left mouse button until the button is released
    StartDragging,
    /// Bring the window to the front, and capture input focus
    Focus,
    /// Get the window's size, need a response with [`ToControllerMessage::GetSizeResponse`]
    GetSize(uuid::Uuid, SizeType),
    /// Get the window's position, need a response with [`ToControllerMessage::GetPositionResponse`]
    GetPosition(uuid::Uuid, PositionType),
    /// Get if the window is currently maximized or not, need a response with [`ToControllerMessage::GetMaximizedResponse`]
    GetMaximized(uuid::Uuid),
    /// Get if the window is currently minimized or not, need a response with [`ToControllerMessage::GetMinimizedResponse`]
    GetMinimized(uuid::Uuid),
    /// Get if the window is currently fullscreen or not, need a response with [`ToControllerMessage::GetFullscreenResponse`]
    GetFullscreen(uuid::Uuid),
    /// Get the visibility of the window, need a response with [`ToControllerMessage::GetVisibleResponse`]
    GetVisible(uuid::Uuid),
    /// Get the scale factor of the window, need a response with [`ToControllerMessage::GetScaleFactorResponse`]
    GetScaleFactor(uuid::Uuid),
    /// Get the current URL of the webview, need a response with [`ToControllerMessage::GetCurrentUrlResponse`]
    GetCurrentUrl(uuid::Uuid),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum PositionType {
    Inner,
    Outer,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum SizeType {
    Inner,
    Outer,
}

/// Message sent from versoview to the controller
#[derive(Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ToControllerMessage {
    /// IPC sender for the controller to send commands to versoview,
    /// this will be the first message sent to the controller once connected
    SetToVersoSender(ipc::IpcSender<ToVersoMessage>),
    /// Sent on a new navigation starting, need a response with [`ToVersoMessage::OnNavigationStartingResponse`]
    OnNavigationStarting(SerializedPipelineId, url::Url),
    /// Sent on a new web resource request, need a response with [`ToVersoMessage::WebResourceRequestResponse`]
    OnWebResourceRequested(WebResourceRequest),
    /// Response to a [`ToVersoMessage::GetSize`]
    GetSizeResponse(uuid::Uuid, PhysicalSize<u32>),
    /// Response to a [`ToVersoMessage::GetPosition`]
    GetPositionResponse(uuid::Uuid, Option<PhysicalPosition<i32>>),
    /// Response to a [`ToVersoMessage::GetMaximized`]
    GetMaximizedResponse(uuid::Uuid, bool),
    /// Response to a [`ToVersoMessage::GetMinimized`]
    GetMinimizedResponse(uuid::Uuid, bool),
    /// Response to a [`ToVersoMessage::GetFullscreen`]
    GetFullscreenResponse(uuid::Uuid, bool),
    /// Response to a [`ToVersoMessage::GetVisible`]
    GetVisibleResponse(uuid::Uuid, bool),
    /// Response to a [`ToVersoMessage::GetScaleFactor`]
    GetScaleFactorResponse(uuid::Uuid, f64),
    /// Response to a [`ToVersoMessage::GetCurrentUrl`]
    GetCurrentUrlResponse(uuid::Uuid, url::Url),
    /// Verso have recieved a close request from the OS
    OnCloseRequested,
}

/// Configuration of Verso instance.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigFromController {
    /// Window size for the initial winit window
    pub inner_size: Option<Size>,
    /// Window position for the initial winit window
    pub position: Option<Position>,
    /// Launch maximized or not for the initial winit window
    pub maximized: bool,
    /// Launch visible or not for the initial winit window
    pub visible: bool,
    /// Launch fullscreen or not for the initial winit window
    pub fullscreen: bool,
    /// Launch focused or not for the initial winit window
    pub focused: bool,
    /// Launch decorated or not for the initial winit window
    pub decorated: bool,
    /// Launch transparent or not for the initial winit window
    pub transparent: bool,
    /// Title of the initial winit window in the title bar.
    pub title: Option<String>,
    /// Window icon of the initial winit window.
    pub icon: Option<Icon>,
    /// The window level for the initial winit window
    pub window_level: WindowLevel,

    /// URL to load initially.
    pub url: Option<url::Url>,
    /// Should launch without or without control panel
    pub with_panel: bool,
    /// Port number to start a server to listen to remote Firefox devtools connections. 0 for random port.
    pub devtools_port: Option<u16>,
    /// Servo time profile settings
    pub profiler_settings: Option<ProfilerSettings>,
    /// Override the user agent
    pub user_agent: Option<String>,
    /// Script to run on document started to load
    pub user_scripts: Vec<UserScript>,
    /// Initial window's zoom level
    pub zoom_level: Option<f32>,
    /// Path to resource directory. If None, Verso will try to get default directory. And if that
    /// still doesn't exist, all resource configuration will set to default values.
    pub resources_directory: Option<PathBuf>,
    /// Register those custom protocols
    pub custom_protocols: Vec<CustomProtocol>,
}

impl Default for ConfigFromController {
    fn default() -> Self {
        Self {
            inner_size: None,
            position: None,
            maximized: false,
            visible: true,
            fullscreen: false,
            focused: true,
            decorated: false,
            transparent: true,
            title: None,
            icon: None,
            window_level: WindowLevel::Normal,

            url: None,
            with_panel: false,
            devtools_port: None,
            profiler_settings: None,
            user_agent: None,
            user_scripts: Vec::new(),
            zoom_level: None,
            resources_directory: None,
            custom_protocols: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Icon {
    /// RGBA bytes of the icon.
    pub rgba: Vec<u8>,
    /// Icon width.
    pub width: u32,
    /// Icon height.
    pub height: u32,
}

/// A window level groups windows with respect to their z-position.
///
/// The relative ordering between windows in different window levels is fixed.
/// The z-order of a window within the same window level may change dynamically on user interaction.
///
/// ## Platform-specific
///
/// - **Wayland:** Unsupported.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub enum WindowLevel {
    /// The default.
    #[default]
    Normal,
    /// The window will always be on top of normal windows.
    AlwaysOnTop,
    /// The window will always be below normal windows.
    ///
    /// This is useful for a widget-based app.
    AlwaysOnBottom,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UserScript {
    pub script: String,
    pub source_file: Option<PathBuf>,
}

impl<T: Into<String>> From<T> for UserScript {
    fn from(script: T) -> Self {
        UserScript {
            script: script.into(),
            source_file: None,
        }
    }
}

/// Servo time profile settings
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProfilerSettings {
    /// Servo time profile settings
    pub output_options: OutputOptions,
    /// When servo profiler is enabled, this is an optional path to dump a self-contained HTML file
    /// visualizing the traces as a timeline.
    pub trace_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum OutputOptions {
    /// Database connection config (hostname, name, user, pass)
    FileName(String),
    Stdout(f64),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WebResourceRequest {
    pub id: uuid::Uuid,
    #[serde(with = "http_serde_ext::request")]
    pub request: http::Request<Vec<u8>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WebResourceRequestResponse {
    pub id: uuid::Uuid,
    #[serde(with = "http_serde_ext::response::option")]
    pub response: Option<http::Response<Vec<u8>>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CustomProtocol {
    pub scheme: String,
    pub secure: bool,
    pub fetchable: bool,
}

pub struct CustomProtocolBuilder(CustomProtocol);

impl CustomProtocolBuilder {
    /// Create a new custom protocol
    pub fn new(scheme: impl Into<String>) -> Self {
        Self(CustomProtocol {
            scheme: scheme.into(),
            secure: true,
            fetchable: true,
        })
    }

    /// Set if the protocol can be used by `fetch`
    pub fn set_fetchable(mut self, fetchable: bool) -> Self {
        self.0.fetchable = fetchable;
        self
    }

    /// Set if the protocol can be used in a [secure context]
    ///
    /// [secure context]: https://developer.mozilla.org/en-US/docs/Web/Security/Secure_Contexts
    pub fn set_secure(mut self, secure: bool) -> Self {
        self.0.secure = secure;
        self
    }
}

impl From<CustomProtocolBuilder> for CustomProtocol {
    fn from(value: CustomProtocolBuilder) -> Self {
        value.0
    }
}
