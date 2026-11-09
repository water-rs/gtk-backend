//! DMA-BUF presentation plumbing for the GPU surface.
//!
//! Cherenkov's `DmabufFrame` contract is explicit-fence: `acquire` is a
//! sync file that signals when the engine's writes retire on the GPU, and
//! the host returns a release sync file through `DmabufFrame::release`.
//! GTK's public `GdkDmabufTextureBuilder` API has no sync-file entry
//! point — dma-buf consumers synchronize through the buffer's own
//! implicit fences, which is what GDK's private
//! `gdk_dmabuf_import_sync_file` / `gdk_dmabuf_export_sync_file` wrap
//! internally. The crate issues the same
//! `DMA_BUF_IOCTL_{IMPORT,EXPORT}_SYNC_FILE` ioctls directly: the
//! acquire fence is attached to the exported image before GTK sees it,
//! and the release fence is exported back out of the image's reservation
//! once GTK's texture is destroyed. Nothing waits on the CPU anywhere.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use gdk4::prelude::DisplayExt;

use waterui_graphics::cherenkov_gpu::interop::dmabuf::{
    DRM_FORMAT_ABGR8888, DRM_FORMAT_ABGR16161616F, DRM_FORMAT_ARGB8888, DRM_FORMAT_XBGR8888,
    DRM_FORMAT_XRGB8888, DmabufFormat, DmabufFrame,
};
use waterui_graphics::cherenkov_gpu::interop::{OutputAlpha, OutputColor, RgbAlpha};

/// `linux/dma-buf.h`: the exported sync file waits on the buffer's
/// writers (a reader's `POLLIN` equivalent).
const DMA_BUF_SYNC_READ: u32 = 1;
/// `linux/dma-buf.h`: imported as a write fence, every subsequent
/// implicit-sync access waits on it; exported, the sync file waits on
/// any users at all (a writer's `POLLOUT` equivalent).
const DMA_BUF_SYNC_WRITE: u32 = 2;
/// `linux/dma-buf.h`: `DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE` —
/// export-side shorthand for waiting on every user.
const DMA_BUF_SYNC_RW: u32 = DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE;

#[repr(C)]
struct DmaBufExportSyncFile {
    flags: u32,
    fd: i32,
}

#[repr(C)]
struct DmaBufImportSyncFile {
    flags: u32,
    fd: i32,
}

/// `_IOWR('b', 2, 8)` — `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` (kernel 6.2+).
const EXPORT_SYNC_FILE: u64 = (3_u64 << 30) | (8 << 16) | ((b'b' as u64) << 8) | 2;
/// `_IOW('b', 3, 8)` — `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` (kernel 6.4+).
const IMPORT_SYNC_FILE: u64 = (1_u64 << 30) | (8 << 16) | ((b'b' as u64) << 8) | 3;

/// Inserts `sync` as a write fence on `dmabuf`: every implicit-sync
/// access any consumer submits afterwards waits on it — the acquire
/// side of the engine's fence contract.
fn import_sync_file(dmabuf: &impl AsRawFd, sync: RawFd) -> io::Result<()> {
    let mut payload = DmaBufImportSyncFile {
        flags: DMA_BUF_SYNC_WRITE,
        fd: sync,
    };
    // SAFETY: `dmabuf` is a live dma-buf fd and `payload` outlives the call.
    if unsafe { libc::ioctl(dmabuf.as_raw_fd(), IMPORT_SYNC_FILE as _, &raw mut payload) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Snapshots every fence on `dmabuf`'s reservation as a new sync file —
/// the release side of the contract: the fd signals once all users the
/// image accumulated (GTK's reads included) are done.
fn export_sync_file(dmabuf: &impl AsRawFd) -> io::Result<OwnedFd> {
    let mut payload = DmaBufExportSyncFile {
        flags: DMA_BUF_SYNC_RW,
        fd: -1,
    };
    // SAFETY: `dmabuf` is a live dma-buf fd and `payload` outlives the call;
    // a nonnegative returned `fd` is an owned descriptor the kernel handed us.
    if unsafe { libc::ioctl(dmabuf.as_raw_fd(), EXPORT_SYNC_FILE as _, &raw mut payload) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a nonnegative `fd` out of the ioctl is a descriptor the kernel
    // opened for this call; ownership moves into the `OwnedFd`.
    Ok(unsafe { OwnedFd::from_raw_fd(payload.fd) })
}

/// The single-plane RGB(A) fourccs cherenkov's dma-buf target exports
/// (`export_format` in `dmabuf_export.rs`). The display's declared list
/// is intersected against what an engine could actually produce, in the
/// display's own preference order, so the negotiation never offers a
/// combination the engine has no renderable `VkFormat` for.
///
/// `pub` so a negotiation failure can name the engine's side.
pub const EXPORTABLE_FOURCCS: [u32; 5] = [
    DRM_FORMAT_ABGR8888,
    DRM_FORMAT_XBGR8888,
    DRM_FORMAT_ARGB8888,
    DRM_FORMAT_XRGB8888,
    DRM_FORMAT_ABGR16161616F,
];

/// The `(fourcc, modifiers)` combinations `display` can import, folded
/// into one [`DmabufFormat`] per fourcc, in the display's preference
/// order.
///
/// Color and alpha are fixed: `premultiplied` is the alpha convention
/// GTK composites with, and `Srgb` is what GTK's default texture color
/// state decodes — `GdkDmabufTextureBuilder::set_color_state`, needed
/// for any wider encoding, is a GTK 4.16 entry point while this crate's
/// floor is 4.14.
///
/// Panics when the intersection is empty: a display that cannot import
/// any of the engine's fourccs cannot host a GPU surface — the failure
/// must surface at the first frame, not degrade to an empty widget.
pub fn display_formats(display: &gdk4::Display) -> Vec<DmabufFormat> {
    let accepted = display.dmabuf_formats();
    let mut declared: Vec<(u32, u64)> = Vec::new();
    let mut negotiated: Vec<DmabufFormat> = Vec::new();
    for index in 0..accepted.n_formats() {
        let (fourcc, modifier) = accepted.format(index);
        declared.push((fourcc, modifier));
        if !EXPORTABLE_FOURCCS.contains(&fourcc) {
            continue;
        }
        match negotiated.iter_mut().find(|format| format.fourcc == fourcc) {
            Some(format) => {
                if !format.modifiers.contains(&modifier) {
                    format.modifiers.push(modifier);
                }
            }
            None => negotiated.push(DmabufFormat::new(
                fourcc,
                vec![modifier],
                OutputColor::Srgb,
                OutputAlpha::Premultiplied,
            )),
        }
    }
    assert!(
        !negotiated.is_empty(),
        "the GdkDisplay offers no dma-buf formats the engine can export; \
         engine fourccs {EXPORTABLE_FOURCCS:?}, display declared (fourcc, modifier) pairs {declared:?}"
    );
    negotiated
}

/// Returns a frame the host never imported to the pool: nothing read
/// its planes, so its own acquire fence is an honest release — the
/// image becomes reusable once the engine's writes retire, exactly as
/// if a consumer had finished instantly.
pub fn release_unread(frame: DmabufFrame) {
    match frame.acquire.try_clone() {
        Ok(acquire) => frame.release(acquire),
        Err(error) => {
            tracing::error!(
                "[gtk-gpu] acquire fd could not be duplicated ({error}); a pool image is retired"
            );
        }
    }
}

/// Wraps `frame` in a `GdkDmabufTexture`.
///
/// The acquire sync file is attached to every plane's reservation first,
/// so the implicit-sync access GTK submits waits on the engine's writes
/// on the GPU; the texture's release callback then exports the image's
/// accumulated fences back out as the frame's release. The frame itself
/// travels inside the release closure — the plane fds must stay valid
/// for the texture's whole life, and GTK runs the callback from the
/// texture's `dispose`.
///
/// A build failure is traced. GTK never runs the release callback on a
/// failed build, so the closure (and the frame inside it) is retired —
/// one pool image permanently lost, loud rather than silent. It can only
/// fire on a `(fourcc, modifier)` pair the display itself declared
/// importable.
pub fn texture(display: &gdk4::Display, frame: DmabufFrame) -> Option<gdk4::Texture> {
    for plane in &frame.planes {
        if let Err(error) = import_sync_file(&plane.fd, frame.acquire.as_raw_fd()) {
            tracing::error!("[gtk-gpu] acquire fence import failed: {error}");
            release_unread(frame);
            return None;
        }
    }

    if frame.planes.is_empty() {
        release_unread(frame);
        return None;
    }

    let mut builder = gdk4::DmabufTextureBuilder::new()
        .set_display(display)
        .set_width(frame.size.0)
        .set_height(frame.size.1)
        .set_fourcc(frame.fourcc)
        .set_modifier(frame.modifier)
        .set_n_planes(u32::try_from(frame.planes.len()).expect("a dmabuf plane count fits u32"))
        .set_premultiplied(!matches!(frame.alpha, RgbAlpha::Straight));
    for (index, plane) in frame.planes.iter().enumerate() {
        let index = u32::try_from(index).expect("a dmabuf plane index fits u32");
        // SAFETY: `frame` moves into the release closure below, keeping
        // every plane fd valid for the texture's whole life.
        builder = unsafe { builder.set_fd(index, plane.fd.as_raw_fd()) }
            .set_stride(index, plane.stride)
            .set_offset(index, plane.offset);
    }

    // SAFETY: `build_with_release_func` runs the closure exactly once
    // when the texture's last reference drops; on the GTK error path the
    // closure is never run and the frame inside is reported.
    match unsafe {
        builder.build_with_release_func(move || match export_sync_file(&frame.planes[0].fd) {
            Ok(release) => frame.release(release),
            Err(error) => {
                tracing::error!(
                    "[gtk-gpu] release fence export failed ({error}); a pool image is retired"
                );
            }
        })
    } {
        Ok(texture) => Some(texture),
        Err(error) => {
            tracing::error!(
                "[gtk-gpu] dmabuf texture build failed ({error}); the frame inside the release closure is retired"
            );
            None
        }
    }
}
