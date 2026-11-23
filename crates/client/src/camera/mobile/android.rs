use super::{I420Buffer, frame};
use anyhow::{Context, Result, ensure};
use jni::{JValue, jni_sig, jni_str};
use livekit_jni::{
    JNIEnv,
    objects::{JByteBuffer, JClass},
};

fn activity<T>(
    f: impl FnOnce(&mut jni::Env, &jni::objects::JObject) -> jni::errors::Result<T>,
) -> Result<T> {
    Ok(crate::android::java(
        crate::android::app().context("no Android activity")?,
        f,
    )?)
}

pub(super) fn permission(request: bool) -> i32 {
    activity(|env, a| {
        env.call_method(
            a,
            jni_str!("cameraPermission"),
            jni_sig!("(Z)I"),
            &[JValue::Bool(request)],
        )?
        .i()
    })
    .unwrap_or(-1)
}
pub(super) fn devices() -> Result<i32> {
    activity(|env, a| {
        env.call_method(a, jni_str!("cameraDevices"), jni_sig!("()I"), &[])?
            .i()
    })
}
pub(super) fn start(front: bool, w: u32, h: u32, fps: u32) -> Result<()> {
    ensure!(
        activity(|env, a| env
            .call_method(
                a,
                jni_str!("startCamera"),
                jni_sig!("(ZIII)Z"),
                &[
                    JValue::Bool(front),
                    JValue::Int(w as i32),
                    JValue::Int(h as i32),
                    JValue::Int(fps as i32)
                ]
            )?
            .z())?,
        "camera unavailable"
    );
    Ok(())
}
pub(super) fn ended() -> bool {
    activity(|env, a| {
        env.call_method(a, jni_str!("cameraEnded"), jni_sig!("()Z"), &[])?
            .z()
    })
    .unwrap_or(true)
}
pub(super) fn stop() {
    if let Err(error) = activity(|env, a| {
        env.call_method(a, jni_str!("stopCamera"), jni_sig!("()V"), &[])?
            .v()
    }) {
        tracing::warn!(%error,"releasing Android camera failed");
    }
}

#[unsafe(no_mangle)]
extern "system" fn Java_io_github_rusttalkcrew_rusttalk_CallCamera_frame(
    env: JNIEnv,
    _: JClass,
    bytes: JByteBuffer,
    w: i32,
    h: i32,
) {
    if !(1..=8192).contains(&w) || !(1..=8192).contains(&h) {
        return;
    }
    let (Ok(ptr), Ok(len)) = (
        env.get_direct_buffer_address(&bytes),
        env.get_direct_buffer_capacity(&bytes),
    ) else {
        return;
    };
    let mut buffer = I420Buffer::new(w as u32, h as u32);
    let (y, u, v) = buffer.data_mut();
    if len < y.len() + u.len() + v.len() {
        return;
    }
    // Java owns this direct buffer until the synchronous callback returns.
    let data = unsafe { std::slice::from_raw_parts(ptr, len) };
    let (luma, chroma) = data.split_at(y.len());
    y.copy_from_slice(luma);
    u.copy_from_slice(&chroma[..u.len()]);
    v.copy_from_slice(&chroma[u.len()..u.len() + v.len()]);
    frame(buffer);
}

pub(super) fn state() -> i32 {
    activity(|env, a| {
        env.call_method(a, jni_str!("cameraState"), jni_sig!("()I"), &[])?
            .i()
    })
    .unwrap_or(0)
}
pub(super) fn select(front: bool) -> bool {
    activity(|env, a| {
        env.call_method(
            a,
            jni_str!("selectCamera"),
            jni_sig!("(Z)Z"),
            &[JValue::Bool(front)],
        )?
        .z()
    })
    .unwrap_or(false)
}
pub(super) fn torch(on: bool) -> bool {
    activity(|env, a| {
        env.call_method(
            a,
            jni_str!("cameraTorch"),
            jni_sig!("(Z)Z"),
            &[JValue::Bool(on)],
        )?
        .z()
    })
    .unwrap_or(false)
}
