//! Process-shared GPU runtime for the GTK backend.
//!
//! `GpuSurface` widgets and `AppliedFilter` hosts used to build a wgpu
//! device per widget on top of that widget's GL context, which made GTK pay
//! `eglCreateContext` once per GPU widget inside the frame snapshot. Both
//! now share one [`GpuRuntime`]; this module holds the lazy bring-up and the
//! GPU→CPU readback both presentation paths pay.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use waterui_graphics::{GpuRuntime, SharedContextError};

type RuntimeCallback = Box<dyn FnOnce(Result<GpuRuntime, SharedContextError>)>;

/// Where the one-per-process runtime bring-up stands.
enum SharedRuntime {
    /// Nobody asked yet.
    NotStarted,
    /// `GpuRuntime::new()` is in flight; each entry resolves when it lands.
    Pending(Vec<RuntimeCallback>),
    /// Ready — cloned out to every caller.
    Ready(GpuRuntime),
    /// Bring-up already failed once with this error; it is reported to every
    /// later caller rather than silently retrying a dead device.
    Failed(SharedContextError),
}

thread_local! {
    static SHARED_RUNTIME: RefCell<SharedRuntime> = const { RefCell::new(SharedRuntime::NotStarted) };
}

/// Resolves the process-shared [`GpuRuntime`], invoking `callback` on the
/// main context. Concurrent callers queue behind the first request, and a
/// completed or failed bring-up resolves immediately.
pub(crate) fn ensure_shared_runtime(
    callback: impl FnOnce(Result<GpuRuntime, SharedContextError>) + 'static,
) {
    let spawn = SHARED_RUNTIME.with(|slot| {
        let mut slot = slot.borrow_mut();
        match &mut *slot {
            SharedRuntime::Ready(runtime) => {
                callback(Ok(runtime.clone()));
                false
            }
            SharedRuntime::Failed(error) => {
                callback(Err(error.clone()));
                false
            }
            SharedRuntime::Pending(waiters) => {
                waiters.push(Box::new(callback));
                false
            }
            SharedRuntime::NotStarted => {
                *slot = SharedRuntime::Pending(vec![Box::new(callback)]);
                true
            }
        }
    });
    if !spawn {
        return;
    }
    gtk4::glib::MainContext::default().spawn_local(async move {
        let result = GpuRuntime::new().await;
        let waiters = SHARED_RUNTIME.with(|slot| {
            let mut slot = slot.borrow_mut();
            match &mut *slot {
                SharedRuntime::Pending(waiters) => {
                    *slot = match &result {
                        Ok(runtime) => SharedRuntime::Ready(runtime.clone()),
                        Err(error) => SharedRuntime::Failed(error.clone()),
                    };
                    std::mem::take(waiters)
                }
                _ => unreachable!("a pending runtime request cannot be superseded"),
            }
        });
        for waiter in waiters {
            waiter(result.clone());
        }
    });
}

/// Reads an `Rgba8Unorm` texture back into tightly packed RGBA8 bytes.
///
/// One `map_async` round-trip per frame; callers present the returned bytes
/// as a `gdk::MemoryTexture`. The texture must have been created with
/// `COPY_SRC`.
///
/// Synchronous on purpose: `map_async` only completes during `device.poll`,
/// so an async shape would just hide the wait. Callers invoke this from a
/// spawned task so the poll does not stall the snapshot it was kicked from.
///
/// # Errors
///
/// Returns a description of the first failed step (device poll or buffer
/// map). The texture is left unchanged on failure.
pub(crate) fn readback_texture_rgba8(
    runtime: &GpuRuntime,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    let shared = runtime.context();
    let bytes_per_row = width.saturating_mul(4);
    // `wgpu` copy rows must align to `COPY_BYTES_PER_ROW_ALIGNMENT` (256);
    // pad the staging layout and strip the padding after download.
    let padded_bpr = bytes_per_row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer_size = u64::from(padded_bpr) * u64::from(height);
    let buffer = shared.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("waterui_gtk_readback"),
        size: buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = shared
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("waterui_gtk_readback"),
        });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_bpr),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    shared.queue.submit([encoder.finish()]);

    // `map_async`'s callback must be `Send`, so the result crosses through an
    // `Arc<Mutex>`; `poll(Wait)` below guarantees it has run by the time the
    // lock is read.
    let map_result = Arc::new(Mutex::new(None));
    buffer.slice(..).map_async(wgpu::MapMode::Read, {
        let map_result = Arc::clone(&map_result);
        move |result| {
            *map_result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(result.map_err(|error| error.to_string()));
        }
    });
    shared
        .device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .map_err(|error| error.to_string())?;
    map_result
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .ok_or_else(|| "map callback did not run during device poll".to_string())??;

    let mapped = buffer.slice(..).get_mapped_range();
    let row = usize::try_from(bytes_per_row).expect("row stride fits usize");
    let padded = usize::try_from(padded_bpr).expect("row stride fits usize");
    let mut pixels = vec![0_u8; row * usize::try_from(height).expect("height fits usize")];
    if padded == row {
        pixels.copy_from_slice(&mapped[..pixels.len()]);
    } else {
        for (index, chunk) in pixels.chunks_mut(row).enumerate() {
            let start = index * padded;
            chunk.copy_from_slice(&mapped[start..start + chunk.len()]);
        }
    }
    drop(mapped);
    buffer.unmap();
    Ok(pixels)
}
