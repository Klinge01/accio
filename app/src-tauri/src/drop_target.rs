use crate::tray;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Manager};

#[derive(Clone, Copy)]
pub enum Origin {
    Panel,
    Tray,
    Picker,
}

pub fn drop_paths(app: &AppHandle, paths: Vec<PathBuf>, origin: Origin) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let errors: Vec<_> = paths
            .into_iter()
            .filter_map(|path| {
                tray::add_path(&app, &path, false).err().map(|error| {
                    format!(
                        "{}: {error:#}",
                        path.file_name()
                            .unwrap_or(path.as_os_str())
                            .to_string_lossy()
                    )
                })
            })
            .collect();
        if errors.is_empty() {
            return;
        }
        let message = errors.join("\n");
        match origin {
            Origin::Panel => {
                if let Some(window) = app.get_webview_window("panel") {
                    let _ = window.emit("drop://error", message);
                }
            }
            Origin::Tray => {
                if let Some(hud) = app.get_webview_window("drop-hud") {
                    let _ = app.run_on_main_thread({
                        let app = app.clone();
                        move || {
                            mark_hud(&app);
                            let _ = hud.emit("drop://message", &message);
                            let _ = hud.show();
                            hide_hud_after(&app, std::time::Duration::from_secs(4));
                        }
                    });
                }
            }
            Origin::Picker => tray::show_error(&app, message),
        }
    });
}

pub fn mark_hud(app: &AppHandle) -> u64 {
    let state = app.state::<crate::state::AppState>();
    let mut tray = state.tray.lock().unwrap();
    tray.drop_generation += 1;
    tray.drop_generation
}

pub fn hide_hud_after(app: &AppHandle, delay: std::time::Duration) {
    let generation = mark_hud(app);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(delay).await;
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || {
            let state = handle.state::<crate::state::AppState>();
            if state.tray.lock().unwrap().drop_generation == generation
                && let Some(hud) = handle.get_webview_window("drop-hud")
            {
                let _ = hud.hide();
            }
        });
    });
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use objc2::{
        DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained,
        runtime::ProtocolObject,
    };
    use objc2_app_kit::{
        NSAutoresizingMaskOptions, NSDragOperation, NSDraggingDestination, NSDraggingInfo, NSEvent,
        NSPasteboardTypeFileURL, NSView,
    };
    use objc2_foundation::{NSArray, NSObjectProtocol, NSRect};
    use tauri::{PhysicalPosition, tray::TrayIcon};

    define_class!(
        #[unsafe(super(NSView))]
        #[name = "AccioTrayDropView"]
        #[thread_kind = MainThreadOnly]
        #[ivars = AppHandle]
        struct DropView;

        unsafe impl NSObjectProtocol for DropView {}

        // The drop view covers tray-icon's own `TaoTrayTarget` view, which opens the menu and
        // emits tray events from its mouse handlers. Forward every click to it.
        impl DropView {
            #[unsafe(method(mouseDown:))]
            fn mouse_down(&self, event: &NSEvent) {
                if let Some(target) = self.tray_target() {
                    target.mouseDown(event);
                }
            }

            #[unsafe(method(mouseUp:))]
            fn mouse_up(&self, event: &NSEvent) {
                if let Some(target) = self.tray_target() {
                    target.mouseUp(event);
                }
            }

            #[unsafe(method(rightMouseDown:))]
            fn right_mouse_down(&self, event: &NSEvent) {
                if let Some(target) = self.tray_target() {
                    target.rightMouseDown(event);
                }
            }

            #[unsafe(method(rightMouseUp:))]
            fn right_mouse_up(&self, event: &NSEvent) {
                if let Some(target) = self.tray_target() {
                    target.rightMouseUp(event);
                }
            }

            #[unsafe(method(otherMouseDown:))]
            fn other_mouse_down(&self, event: &NSEvent) {
                if let Some(target) = self.tray_target() {
                    target.otherMouseDown(event);
                }
            }

            #[unsafe(method(otherMouseUp:))]
            fn other_mouse_up(&self, event: &NSEvent) {
                if let Some(target) = self.tray_target() {
                    target.otherMouseUp(event);
                }
            }
        }

        unsafe impl NSDraggingDestination for DropView {
            #[unsafe(method(draggingEntered:))]
            fn dragging_entered(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
                self.show_hud();
                sender.draggingSourceOperationMask() & NSDragOperation::Copy
            }

            #[unsafe(method(draggingExited:))]
            fn dragging_exited(&self, _sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) {
                hide_hud_after(self.ivars(), std::time::Duration::from_millis(380));
            }

            #[unsafe(method(prepareForDragOperation:))]
            fn prepare_drop(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
                sender.draggingSourceOperationMask().contains(NSDragOperation::Copy)
            }

            #[unsafe(method(performDragOperation:))]
            fn perform_drop(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
                hide_hud_after(self.ivars(), std::time::Duration::ZERO);
                let paths: Vec<_> = sender.draggingPasteboard().pasteboardItems()
                    .iter().flat_map(|items| items.iter())
                    .filter_map(|item| item.stringForType(unsafe { NSPasteboardTypeFileURL }))
                    .filter_map(|url| url::Url::parse(&url.to_string()).ok()?.to_file_path().ok())
                    .collect();
                if paths.is_empty() {
                    false
                } else {
                    drop_paths(self.ivars(), paths, Origin::Tray);
                    true
                }
            }
        }
    );

    impl DropView {
        /// tray-icon's click-handling sibling view (class `TaoTrayTarget`).
        fn tray_target(&self) -> Option<Retained<NSView>> {
            let button = unsafe { self.superview() }?;
            let siblings = button.subviews();
            siblings
                .iter()
                .find(|view| view.class().name().to_bytes() == b"TaoTrayTarget")
        }

        fn new(app: AppHandle, frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(app);
            unsafe { msg_send![super(this), initWithFrame: frame] }
        }

        fn show_hud(&self) {
            let app = self.ivars();
            let Some(hud) = app.get_webview_window("drop-hud") else {
                return;
            };
            let Some(icon) = app.tray_by_id("accio") else {
                return;
            };
            if let (Ok(Some(rect)), Ok(size), Ok(scale)) =
                (icon.rect(), hud.outer_size(), hud.scale_factor())
            {
                let origin = rect.position.to_physical::<i32>(scale);
                let icon_size = rect.size.to_physical::<i32>(scale);
                let _ = hud.set_position(PhysicalPosition::new(
                    origin.x + (icon_size.width - size.width as i32) / 2,
                    origin.y + icon_size.height + 4,
                ));
            }
            mark_hud(app);
            let _ = hud.emit("drop://message", "Drop to ingest");
            let _ = hud.show();
        }
    }

    pub fn install(app: &AppHandle, icon: &TrayIcon) -> anyhow::Result<()> {
        let app = app.clone();
        icon.with_inner_tray_icon(move |inner| {
            let mtm = MainThreadMarker::new().expect("tray runs on the main thread");
            let item = inner
                .ns_status_item()
                .ok_or_else(|| anyhow::anyhow!("missing status item"))?;
            let button = item
                .button(mtm)
                .ok_or_else(|| anyhow::anyhow!("missing status button"))?;
            let destination = DropView::new(app, button.bounds(), mtm);
            destination.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewWidthSizable
                    | NSAutoresizingMaskOptions::ViewHeightSizable,
            );
            destination.registerForDraggedTypes(&NSArray::from_slice(&[unsafe {
                NSPasteboardTypeFileURL
            }]));
            button.addSubview(&destination);
            Ok(())
        })?
    }
}
#[cfg(target_os = "macos")]
pub use macos::install;
