//! AppKit owns the retained view backing. We invalidate individual rectangles
//! and draw only the actual dirty rectangles, never replace a CALayer's contents.
//! All AppKit access and pixel updates are confined to winit's main thread.
use super::{
    Rect,
    macos_damage::{Mapping, Points},
};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, rc::Retained};
use objc2_app_kit::{NSAutoresizingMaskOptions, NSGraphicsContext, NSView, NSViewLayerContentsRedrawPolicy};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGContext, CGDataProvider, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo, CGInterpolationQuality,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    ptr::{self, NonNull},
    sync::{Arc, Weak},
};
use winit::window::Window;

#[derive(Default)]
struct Frame {
    pixels: Vec<u32>,
    size: (usize, usize),
}

struct ViewState {
    frame: RefCell<Frame>,
    failed: Cell<bool>,
    window: Weak<Window>,
}

define_class!(
    // SAFETY: This opaque, flipped NSView only draws on the main thread. It
    // leaves hit testing and first-responder/input ownership with winit.
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[name = "KwilDamageView"]
    #[ivars = ViewState]
    struct DamageView;

    impl DamageView {
        #[unsafe(method(isFlipped))]
        fn flipped(&self) -> bool { true }

        #[unsafe(method(isOpaque))]
        fn opaque(&self) -> bool { true }

        // Retained object returns require objc2's ownership-aware ABI adapter.
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, _point: NSPoint) -> Option<Retained<NSView>> { None }

        #[unsafe(method(drawRect:))]
        fn draw(&self, _dirty: NSRect) {
            if self.draw_damage().is_err() {
                self.ivars().failed.set(true);
                // An expose can draw outside present(). Hand failure back to
                // the existing event loop; never install a timer or retry loop.
                if let Some(window) = self.ivars().window.upgrade() { window.request_redraw(); }
            }
        }
    }
);

fn points(rect: NSRect) -> Points {
    Points { x: rect.origin.x, y: rect.origin.y, w: rect.size.width, h: rect.size.height }
}

fn nsrect(rect: Points) -> NSRect {
    NSRect::new(NSPoint::new(rect.x, rect.y), NSSize::new(rect.w, rect.h))
}

impl DamageView {
    fn mapping(&self, size: (usize, usize)) -> Result<Mapping, String> {
        Mapping::new(size, points(self.bounds())).ok_or_else(|| "invalid AppKit view geometry".into())
    }

    fn draw_damage(&self) -> Result<(), String> {
        if self.ivars().failed.get() {
            return Err("AppKit presenter failed".into());
        }
        let frame = self.ivars().frame.try_borrow().map_err(|_| "reentrant AppKit drawing")?;
        if frame.pixels.is_empty() {
            return Ok(());
        }
        let mapping = self.mapping(frame.size)?;
        let graphics = NSGraphicsContext::currentContext().ok_or("no AppKit graphics context")?;
        let context = graphics.CGContext();
        let mut rects = ptr::null();
        let mut count = 0;
        // SAFETY: AppKit supplies this immutable list for the duration of drawRect.
        unsafe {
            self.getRectsBeingDrawn_count(&mut rects, &mut count);
        }
        if count < 0 || (count > 0 && rects.is_null()) {
            return Err("invalid AppKit dirty rectangles".into());
        }
        if count == 0 {
            return Ok(());
        }
        let rects = unsafe { std::slice::from_raw_parts(rects, count as usize) };
        for rect in rects {
            let Some(area) = mapping.pixels(points(*rect)) else {
                continue;
            };
            let destination = mapping.points(area).ok_or("invalid AppKit crop")?;
            let image = patch_image(&frame, area)?;
            // CGImages have a bottom-up drawing transform in a flipped NSView.
            // Flip each cropped image locally, keeping framebuffer y top-down.
            CGContext::save_g_state(Some(&context));
            CGContext::clip_to_rect(Some(&context), *rect);
            CGContext::translate_ctm(Some(&context), destination.x, destination.y + destination.h);
            CGContext::scale_ctm(Some(&context), 1.0, -1.0);
            CGContext::set_interpolation_quality(Some(&context), CGInterpolationQuality::None);
            CGContext::draw_image(Some(&context), nsrect(Points { x: 0.0, y: 0.0, ..destination }), Some(&image));
            CGContext::restore_g_state(Some(&context));
        }
        Ok(())
    }
}

// The provider owns each crop until CoreGraphics releases it, including any
// deferred consumption. It never points into a mutable retained framebuffer.
unsafe extern "C-unwind" fn release_patch(info: *mut c_void, _data: NonNull<c_void>, _size: usize) {
    unsafe {
        drop(Box::from_raw(info.cast::<Vec<u32>>()));
    }
}

fn patch_image(frame: &Frame, area: Rect) -> Result<objc2_core_foundation::CFRetained<CGImage>, String> {
    let len = area.w.checked_mul(area.h).ok_or("AppKit crop size overflow")?;
    let mut patch = Vec::new();
    patch.try_reserve_exact(len).map_err(|_| "AppKit crop allocation failed")?;
    for y in area.y..area.y + area.h {
        patch.extend_from_slice(&frame.pixels[y * frame.size.0 + area.x..y * frame.size.0 + area.x + area.w]);
    }
    let patch = Box::new(patch);
    let data = patch.as_ptr().cast();
    let bytes = len.checked_mul(4).ok_or("AppKit crop byte size overflow")?;
    let info = Box::into_raw(patch).cast();
    // SAFETY: The boxed allocation is immutable and released by the provider.
    let provider = unsafe { CGDataProvider::with_data(info, data, bytes, Some(release_patch)) };
    let Some(provider) = provider else {
        unsafe {
            drop(Box::from_raw(info.cast::<Vec<u32>>()));
        }
        return Err("CoreGraphics data provider creation failed".into());
    };
    let space = CGColorSpace::new_device_rgb().ok_or("CoreGraphics RGB color space creation failed")?;
    let bitmap = CGBitmapInfo(CGImageAlphaInfo::NoneSkipFirst.0 | CGImageByteOrderInfo::Order32Little.0);
    // SAFETY: All rows are packed 0x00RRGGBB, decode is null, provider owns bytes.
    unsafe {
        CGImage::new(
            area.w,
            area.h,
            8,
            32,
            area.w * 4,
            Some(&space),
            bitmap,
            Some(&provider),
            ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }
    .ok_or_else(|| "CoreGraphics image creation failed".into())
}

#[derive(Default)]
pub(super) struct Presenter {
    view: Option<Retained<DamageView>>,
    disabled: bool,
}

impl Presenter {
    /// False is sticky softbuffer fallback. An error retires the child before
    /// the host immediately repairs the whole softbuffer image on this frame.
    pub fn present(
        &mut self,
        window: &Arc<Window>,
        pixels: &[u32],
        size: (usize, usize),
        damage: &[Rect],
    ) -> Result<bool, String> {
        if self.disabled {
            return Ok(false);
        }
        match self.update(window, pixels, size, damage) {
            Ok(()) => Ok(true),
            Err(error) => {
                self.disabled = true;
                if let Some(view) = self.view.take() {
                    view.removeFromSuperview();
                }
                Err(error)
            }
        }
    }

    fn update(
        &mut self,
        window: &Arc<Window>,
        pixels: &[u32],
        size: (usize, usize),
        damage: &[Rect],
    ) -> Result<(), String> {
        let mtm = MainThreadMarker::new().ok_or("AppKit presentation requires the main thread")?;
        if size.0.checked_mul(size.1) != Some(pixels.len()) || pixels.is_empty() {
            return Err("AppKit framebuffer size mismatch".into());
        }
        if self.view.is_none() {
            let handle = window.window_handle().map_err(|error| error.to_string())?;
            let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
                return Err("no AppKit view".into());
            };
            // SAFETY: winit's borrowed handle is a valid NSView on this thread.
            let parent: &NSView = unsafe { handle.ns_view.cast().as_ref() };
            let this = DamageView::alloc(mtm).set_ivars(ViewState {
                frame: RefCell::new(Frame::default()),
                failed: Cell::new(false),
                window: Arc::downgrade(window),
            });
            let view: Option<Retained<DamageView>> = unsafe { msg_send![super(this), initWithFrame: parent.bounds()] };
            let view = view.ok_or("AppKit view initialization failed")?;
            // Retain before attachment so all error paths can retire the view.
            self.view = Some(view.clone());
            view.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
            );
            view.setWantsLayer(true);
            view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::OnSetNeedsDisplay);
            parent.addSubview(&view);
        }
        let view = self.view.as_ref().ok_or("AppKit view creation failed")?;
        if view.ivars().failed.get() {
            return Err("AppKit drawing failed".into());
        }
        let mapping = view.mapping(size)?;
        let mut frame = view.ivars().frame.try_borrow_mut().map_err(|_| "reentrant AppKit update")?;
        let resized = frame.size != size;
        if resized {
            let additional = pixels.len().saturating_sub(frame.pixels.len());
            frame.pixels.try_reserve_exact(additional).map_err(|_| "AppKit framebuffer allocation failed")?;
            frame.pixels.resize(pixels.len(), 0);
            frame.size = size;
        }
        let full = [Rect { x: 0, y: 0, w: size.0, h: size.1 }];
        let damage = if resized { &full } else { damage };
        for area in damage {
            let Some(area) = mapping.clip(*area) else {
                continue;
            };
            for y in area.y..area.y + area.h {
                let start = y * size.0 + area.x;
                frame.pixels[start..start + area.w].copy_from_slice(&pixels[start..start + area.w]);
            }
        }
        drop(frame);
        for area in damage {
            if let Some(area) = mapping.points(*area) {
                view.setNeedsDisplayInRect(nsrect(area));
            }
        }
        // No idle wake: flush only this accepted event-driven frame. AppKit may
        // also restore the retained pixels on a later expose via drawRect.
        view.displayIfNeeded();
        if view.ivars().failed.get() {
            return Err("AppKit drawing failed".into());
        }
        Ok(())
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        if let Some(view) = self.view.take() {
            view.removeFromSuperview();
        }
    }
}
