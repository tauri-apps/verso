use crossbeam_channel::Sender;
use dpi::{LogicalPosition, PhysicalPosition};
use embedder_traits::{NewWebViewDetails, ViewportDetails};
use euclid::Scale;
use servo::WebView;
use servo_constellation_traits::EmbedderToConstellationMessage;
use servo_url::ServoUrl;
use webrender_api::units::DeviceRect;

use crate::{verso::send_to_constellation, window::Window};

/// Trait for webview menus
pub trait WebViewMenu {
    /// Get the webview of the menu
    fn webview(&self) -> &WebView;
    /// Set the webview rect of the menu
    fn set_webview_rect(&mut self, rect: DeviceRect);
    /// Get the position of the menu
    fn position(&self) -> LogicalPosition<f64>;
    /// Set the position of the menu
    fn set_position(&mut self, position: LogicalPosition<f64>);
    /// Get the resource url of the menu which is used to present the menu
    fn resource_url(&self) -> ServoUrl;
    /// Show the menu
    fn show(
        &mut self,
        sender: &Sender<EmbedderToConstellationMessage>,
        window: &Window,
        position: PhysicalPosition<f64>,
    ) {
        self.set_position(position.to_logical(window.scale_factor()));
        self.set_webview_rect(DeviceRect::from_size(window.outer_size()));

        let hidpi_scale_factor = Scale::new(window.scale_factor() as f32);
        let size = window.outer_size().to_f32() / hidpi_scale_factor;
        send_to_constellation(
            sender,
            EmbedderToConstellationMessage::NewWebView(
                self.resource_url(),
                NewWebViewDetails {
                    webview_id: self.webview().id(),
                    viewport_details: ViewportDetails {
                        size,
                        hidpi_scale_factor,
                    },
                    // Todo: create userContentManager
                    user_content_manager_id: None,
                },
            ),
        );
    }
    /// Close the menu
    fn close(&mut self, sender: &Sender<EmbedderToConstellationMessage>) {
        send_to_constellation(
            sender,
            EmbedderToConstellationMessage::CloseWebView(self.webview().id()),
        );
    }
}
