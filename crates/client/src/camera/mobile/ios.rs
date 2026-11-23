use super::{I420Buffer, frame};
use anyhow::{Result, ensure};
use livekit::webrtc::native::yuv_helper::argb_to_i420;

unsafe extern "C" {
    fn rt_camera_permission(request: bool) -> i32;
    fn rt_camera_devices() -> i32;
    fn rt_camera_start(front: bool, height: u32, fps: u32) -> bool;
    fn rt_camera_ended() -> bool;
    fn rt_camera_stop();
}

pub(super) fn permission(request: bool) -> i32 {
    unsafe { rt_camera_permission(request) }
}
pub(super) fn devices() -> Result<i32> {
    Ok(unsafe { rt_camera_devices() })
}
pub(super) fn start(front: bool, _width: u32, height: u32, fps: u32) -> Result<()> {
    ensure!(
        unsafe { rt_camera_start(front, height, fps) },
        "camera unavailable"
    );
    Ok(())
}
pub(super) fn ended() -> bool {
    unsafe { rt_camera_ended() }
}
pub(super) fn stop() {
    unsafe { rt_camera_stop() }
}

#[unsafe(no_mangle)]
extern "C" fn rt_camera_bgra(data: *const u8, w: u32, h: u32, stride: u32) {
    if data.is_null() || !(1..=8192).contains(&w) || !(1..=8192).contains(&h) || stride < w * 4 {
        return;
    }
    // The delegate holds the pixel buffer read-locked throughout this call.
    let pixels = unsafe { std::slice::from_raw_parts(data, (stride as usize) * (h as usize)) };
    let mut buffer = I420Buffer::new(w, h);
    let (ys, us, vs) = buffer.strides();
    let (y, u, v) = buffer.data_mut();
    argb_to_i420(pixels, stride, y, ys, u, us, v, vs, w as i32, h as i32);
    frame(buffer);
}
