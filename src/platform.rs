//! Platform-specific window setup.

/// Prevent macOS from stretching the last frame while the window is resized.
///
/// By default AppKit scales layer contents to the new bounds during a live resize,
/// which makes text momentarily appear stretched. Anchoring the contents at the
/// visual top-left makes it clip/reveal instead. Returns `true` once at least one
/// window was set up.
#[cfg(target_os = "macos")]
pub(crate) fn configure_live_resize() -> bool {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use objc2_quartz_core::{CALayer, kCAGravityBottomLeft, kCAGravityTopLeft};

    // `contentsGravity` is resolved in the layer's own coordinate space. Winit's
    // view is flipped, so `Top` would land at the visual bottom; pick the
    // constant that actually anchors the contents at the visual top.
    let anchor_top_left = |layer: &CALayer| {
        // SAFETY: Both are framework-provided constants.
        let gravity = unsafe {
            if layer.contentsAreFlipped() {
                kCAGravityBottomLeft
            } else {
                kCAGravityTopLeft
            }
        };
        layer.setContentsGravity(gravity);
    };

    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let app = NSApplication::sharedApplication(mtm);
    let windows = app.windows();
    let mut configured = false;
    for window in windows.iter() {
        if let Some(view) = window.contentView()
            && let Some(layer) = view.layer()
        {
            anchor_top_left(&layer);
            // The wgpu render layer is usually a sublayer, so pin it too.
            if let Some(sublayers) = unsafe { layer.sublayers() } {
                for sublayer in sublayers.iter() {
                    anchor_top_left(&sublayer);
                }
            }
            configured = true;
        }
    }
    configured
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn configure_live_resize() -> bool {
    true
}
