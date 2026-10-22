//! Damage-preserving GPU backing store. wgpu 30 exposes no Present1 dirty/scroll
//! rectangles, so this is deliberately NOT an onscreen takeover. CPU presentation
//! remains authoritative until an admitted platform damage-present seam exists.
use super::Rect;
pub(crate) struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    texture: Option<wgpu::Texture>,
    scratch: Option<wgpu::Texture>,
    size: (usize, usize),
}
impl Gpu {
    pub fn invalidate(&mut self) {
        self.size = (0, 0);
    }
    #[cfg(feature = "frame-harness")]
    pub fn matches(&self, pixels: &[u32]) -> Result<bool, String> {
        let texture = self.texture.as_ref().ok_or("backing not initialized")?;
        let (w, h) = self.size;
        let stride = (w * 4).div_ceil(256) * 256;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("kwil harness readback"),
            size: (stride * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            copy(texture, 0, 0),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride as u32),
                    rows_per_image: Some(h as u32),
                },
            },
            extent(w, h),
        );
        self.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device
            .poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(std::time::Duration::from_secs(5)) })
            .map_err(|e| e.to_string())?;
        rx.recv_timeout(std::time::Duration::from_secs(5)).map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
        let bytes = buffer.slice(..).get_mapped_range().map_err(|e| e.to_string())?;
        let equal = (0..h).all(|y| {
            (0..w).all(|x| {
                let p = pixels[y * w + x];
                let i = y * stride + x * 4;
                bytes[i..i + 4] == [(p >> 16) as u8, (p >> 8) as u8, p as u8, 255]
            })
        });
        drop(bytes);
        buffer.unmap();
        Ok(equal)
    }
    pub fn init() -> Result<Self, String> {
        #[cfg(windows)]
        let backends = wgpu::Backends::DX12;
        #[cfg(target_os = "macos")]
        let backends = wgpu::Backends::METAL;
        #[cfg(not(any(windows, target_os = "macos")))]
        let backends = wgpu::Backends::VULKAN | wgpu::Backends::GL;
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = pollster::block_on(instance.request_adapter(&Default::default())).map_err(|e| e.to_string())?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .map_err(|e| e.to_string())?;
        Ok(Self { device, queue, texture: None, scratch: None, size: (0, 0) })
    }
    /// Upload only new damage; preserve scroll pixels via two texture copies
    /// because overlapping self-copies are not valid WebGPU operations.
    pub fn update(
        &mut self,
        pixels: &[u32],
        size: (usize, usize),
        rects: &[Rect],
        scroll: i64,
        cell_height: usize,
        rows: usize,
    ) -> bool {
        let (w, h) = size;
        if w == 0
            || h == 0
            || w > self.device.limits().max_texture_dimension_2d as usize
            || h > self.device.limits().max_texture_dimension_2d as usize
        {
            return false;
        }
        let fresh = self.size != size;
        if fresh {
            let descriptor = wgpu::TextureDescriptor {
                label: Some("kwil damage backing"),
                size: extent(w, h),
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            };
            self.texture = Some(self.device.create_texture(&descriptor));
            self.scratch = Some(self.device.create_texture(&descriptor));
            self.size = size;
        }
        let texture = self.texture.as_ref().unwrap();
        let mut encoder = self.device.create_command_encoder(&Default::default());
        if !fresh && scroll != 0 && (scroll.unsigned_abs() as usize) < rows {
            let dy = scroll.unsigned_abs() as usize * cell_height;
            let keep = rows * cell_height - dy;
            let (from, to) = if scroll > 0 { (dy, 0) } else { (0, dy) };
            let scratch = self.scratch.as_ref().unwrap();
            encoder.copy_texture_to_texture(copy(texture, 0, from), copy(scratch, 0, 0), extent(w, keep));
            encoder.copy_texture_to_texture(copy(scratch, 0, 0), copy(texture, 0, to), extent(w, keep));
        }
        self.queue.submit([encoder.finish()]);
        let full = [Rect { x: 0, y: 0, w, h }];
        for r in if fresh { &full[..] } else { rects } {
            if r.w == 0 || r.h == 0 {
                continue;
            }
            let mut data = Vec::with_capacity(r.w * r.h * 4);
            for y in r.y..r.y + r.h {
                for &color in &pixels[y * w + r.x..y * w + r.x + r.w] {
                    data.extend_from_slice(&[(color >> 16) as u8, (color >> 8) as u8, color as u8, 255]);
                }
            }
            self.queue.write_texture(
                copy(texture, r.x, r.y),
                &data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(r.w as u32 * 4),
                    rows_per_image: Some(r.h as u32),
                },
                extent(r.w, r.h),
            );
        }
        self.queue.submit([]);
        true
    }
}
fn extent(w: usize, h: usize) -> wgpu::Extent3d {
    wgpu::Extent3d { width: w as u32, height: h as u32, depth_or_array_layers: 1 }
}
fn copy(texture: &wgpu::Texture, x: usize, y: usize) -> wgpu::TexelCopyTextureInfo<'_> {
    wgpu::TexelCopyTextureInfo {
        texture,
        mip_level: 0,
        origin: wgpu::Origin3d { x: x as u32, y: y as u32, z: 0 },
        aspect: wgpu::TextureAspect::All,
    }
}
