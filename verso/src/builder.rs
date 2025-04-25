use dpi::{Position, Size};
use std::path::{Path, PathBuf};
use versoview_messages::{ConfigFromController, CustomProtocol, ProfilerSettings, UserScript};

use crate::VersoviewController;

/// A builder for configuring and creating a [`VersoviewController`] instance.
#[derive(Debug, Clone)]
pub struct VersoBuilder(ConfigFromController);

impl VersoBuilder {
    /// Creates a new [`VersoBuilder`] with default settings.
    pub fn new() -> Self {
        Self(ConfigFromController::default())
    }

    /// Sets whether the control panel should be included.
    pub fn with_panel(mut self, with_panel: bool) -> Self {
        self.0.with_panel = with_panel;
        self
    }

    /// Sets the initial window size.
    pub fn inner_size(mut self, size: impl Into<Size>) -> Self {
        self.0.inner_size = Some(size.into());
        self
    }

    /// Sets the initial window position.
    pub fn position(mut self, position: impl Into<Position>) -> Self {
        self.0.position = Some(position.into());
        self
    }

    /// Sets whether the window should start maximized.
    pub fn maximized(mut self, maximized: bool) -> Self {
        self.0.maximized = maximized;
        self
    }

    /// Sets whether the window should be visible initially.
    pub fn visible(mut self, visible: bool) -> Self {
        self.0.visible = visible;
        self
    }

    /// Sets whether the window should start in fullscreen mode.
    pub fn fullscreen(mut self, fullscreen: bool) -> Self {
        self.0.fullscreen = fullscreen;
        self
    }

    /// Sets whether the window will be initially focused or not.
    pub fn focused(mut self, focused: bool) -> Self {
        self.0.focused = focused;
        self
    }

    /// Sets whether the window will be initially decorated or not.
    pub fn decorated(mut self, decorated: bool) -> Self {
        self.0.decorated = decorated;
        self
    }

    /// Sets whether the window will be initially transparent or not.
    pub fn transparent(mut self, transparent: bool) -> Self {
        self.0.transparent = transparent;
        self
    }

    /// Sets the initial title of the window in the title bar.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.0.title = Some(title.into());
        self
    }

    /// Sets the window icon.
    pub fn icon(mut self, icon: versoview_messages::Icon) -> Self {
        self.0.icon = Some(icon);
        self
    }

    /// Port number to start a server to listen to remote Firefox devtools connections. 0 for random port.
    pub fn devtools_port(mut self, port: u16) -> Self {
        self.0.devtools_port = Some(port);
        self
    }

    /// Sets the profiler settings.
    pub fn profiler_settings(mut self, settings: ProfilerSettings) -> Self {
        self.0.profiler_settings = Some(settings);
        self
    }

    /// Overrides the user agent.
    pub fn user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.0.user_agent = Some(user_agent.into());
        self
    }

    /// Adds an user script to run when the document starts loading.
    pub fn user_script(mut self, script: impl Into<UserScript>) -> Self {
        self.0.user_scripts.push(script.into());
        self
    }

    /// Adds multiple user scripts to run when the document starts loading.
    pub fn user_scripts<I, S>(mut self, scripts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<UserScript>,
    {
        for script in scripts {
            self = self.user_script(script)
        }
        self
    }

    /// Sets the initial zoom level of the webview.
    pub fn zoom_level(mut self, zoom: f32) -> Self {
        self.0.zoom_level = Some(zoom);
        self
    }

    /// Sets the resource directory path.
    pub fn resources_directory(mut self, path: impl Into<PathBuf>) -> Self {
        self.0.resources_directory = Some(path.into());
        self
    }

    /// Registers a custom protocol.
    ///
    /// ## Example
    ///
    /// ```
    /// let verso_builder =
    ///     VersoBuilder::new().custom_protocol(CustomProtocolBuilder::new("custom-protocol-1"));
    /// ```
    pub fn custom_protocol(mut self, custom_protocol: impl Into<CustomProtocol>) -> Self {
        self.0.custom_protocols.push(custom_protocol.into());
        self
    }

    /// Registers multiple custom protocols.
    ///
    /// ## Example
    ///
    /// ```
    /// let verso_builder = VersoBuilder::new().custom_protocols([
    ///     CustomProtocolBuilder::new("custom-protocol-1"),
    ///     CustomProtocolBuilder::new("custom-protocol-2"),
    /// ]);
    /// ```
    pub fn custom_protocols<I, C>(mut self, custom_protocols: I) -> Self
    where
        I: IntoIterator<Item = C>,
        C: Into<CustomProtocol>,
    {
        for custom_protocol in custom_protocols {
            self = self.custom_protocol(custom_protocol);
        }
        self
    }

    /// Builds the [`VersoviewController`] with the configured settings.
    pub fn build(
        self,
        versoview_path: impl AsRef<Path>,
        initial_url: url::Url,
    ) -> VersoviewController {
        VersoviewController::create(versoview_path, initial_url, self.0)
    }
}
