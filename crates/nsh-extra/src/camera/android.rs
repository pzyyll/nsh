//! The camera on Android: the camera app (`ACTION_IMAGE_CAPTURE`) started
//! for a result (`nsh::activity`), writing its photo to an output file of
//! the app's cache through the runtime's file provider. When the app's
//! manifest declares `CAMERA`, Android refuses the intent until the
//! permission is granted, so it is asked first; an app that does not
//! declare it needs none. The photo is prepared off the UI thread as a
//! picked image (`nsh::picker::prepare_image`) and the file deleted.

use std::{future::Future, pin::Pin};

use nsh::{
    activity::{self, ActivityError, ActivityResult, OutputFile},
    jni::{
        JNIEnv,
        objects::{JObject, JValue},
    },
    permissions::{self, Permission},
};
use nsh_core::{picker::PickedFile, prelude::*};

use super::{PhotoError, PhotoRequest};

const CAMERA: &str = "android.permission.CAMERA";
const IMAGE_CAPTURE: &str = "android.media.action.IMAGE_CAPTURE";
const EXTRA_OUTPUT: &str = "output";
/// `FLAG_GRANT_READ_URI_PERMISSION | FLAG_GRANT_WRITE_URI_PERMISSION`.
const GRANT_READ_WRITE: i32 = 0x1 | 0x2;

type Started = Pin<Box<dyn Future<Output = Result<ActivityResult, ActivityError>> + Send>>;

pub(super) async fn available() -> bool {
    nsh::with_env(|env| {
        let intent = capture_intent(env)?;
        Ok(activity::can_start(env, &intent))
    })
}

pub(super) async fn take_photo(request: PhotoRequest) -> Result<Option<PickedFile>, PhotoError> {
    allow_camera().await?;
    let started = nsh::with_env(|env| Ok(Some(start(env)?)));
    let Some((output, started)) = started.transpose()? else {
        return Err(PhotoError::failed("the camera app cannot be started"));
    };
    let result = started.await;
    let path = output.path.clone();
    let answer = match result {
        Ok(result) if result.code == activity::RESULT_OK => {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.split_once('-').map(|(_, rest)| rest.to_owned()))
                .unwrap_or_else(|| "photo.jpg".to_owned());
            let pick = request.pick_request();
            let prepared = blocking(move || nsh::picker::prepare_image(&path, &name, &pick))
                .await
                .map_err(|error| {
                    PhotoError::failed(format!("cannot prepare the photo: {error}"))
                })?;
            match prepared {
                Some(photo) => Ok(Some(photo)),
                None => Err(PhotoError::failed("the camera app wrote no photo")),
            }
        }
        Ok(_) => Ok(None),
        Err(ActivityError::NotFound) => Err(PhotoError::unavailable("no camera app is installed")),
        Err(error) => Err(PhotoError::failed(error.to_string())),
    };
    let _ = std::fs::remove_file(&output.path);
    answer
}

/// Asks for `CAMERA` when the manifest declares it and the user has not
/// granted it yet.
async fn allow_camera() -> Result<(), PhotoError> {
    let denied = || PhotoError::denied("the user does not let the app use the camera");
    match permissions::state(CAMERA) {
        Ok(Permission::Granted | Permission::Undeclared) => Ok(()),
        Ok(Permission::Denied) => Err(denied()),
        Ok(_) => match permissions::request(&[CAMERA]).await {
            Ok(states) if states.first() == Some(&Permission::Granted) => Ok(()),
            Ok(_) => Err(denied()),
            Err(error) => Err(PhotoError::failed(error.to_string())),
        },
        Err(error) => Err(PhotoError::failed(error.to_string())),
    }
}

/// `new Intent(ACTION_IMAGE_CAPTURE)`.
fn capture_intent<'local>(env: &mut JNIEnv<'local>) -> nsh::jni::errors::Result<JObject<'local>> {
    let action = env.new_string(IMAGE_CAPTURE)?;
    env.new_object(
        "android/content/Intent",
        "(Ljava/lang/String;)V",
        &[JValue::Object(&action)],
    )
}

/// Starts the camera app writing to a new output file: the file, and the
/// camera app's answer to come; or why it cannot start.
fn start(
    env: &mut JNIEnv<'_>,
) -> nsh::jni::errors::Result<Result<(OutputFile, Started), PhotoError>> {
    let intent = capture_intent(env)?;
    if !activity::can_start(env, &intent) {
        return Ok(Err(PhotoError::unavailable("no camera app is installed")));
    }
    let output = match activity::output_file(env, "photo.jpg") {
        Ok(output) => output,
        Err(error) => return Ok(Err(PhotoError::failed(error.to_string()))),
    };
    let uri = output.uri.as_obj();
    let extra = env.new_string(EXTRA_OUTPUT)?;
    env.call_method(
        &intent,
        "putExtra",
        "(Ljava/lang/String;Landroid/os/Parcelable;)Landroid/content/Intent;",
        &[JValue::Object(&extra), JValue::Object(uri)],
    )?;
    // The grant reaches the camera app through the intent's clip data.
    let label = env.new_string("photo")?;
    let clip = env
        .call_static_method(
            "android/content/ClipData",
            "newRawUri",
            "(Ljava/lang/CharSequence;Landroid/net/Uri;)Landroid/content/ClipData;",
            &[JValue::Object(&label), JValue::Object(uri)],
        )?
        .l()?;
    env.call_method(
        &intent,
        "setClipData",
        "(Landroid/content/ClipData;)V",
        &[JValue::Object(&clip)],
    )?;
    env.call_method(
        &intent,
        "addFlags",
        "(I)Landroid/content/Intent;",
        &[JValue::Int(GRANT_READ_WRITE)],
    )?;
    let started: Started = Box::pin(activity::start_for_result(env, &intent));
    Ok(Ok((output, started)))
}
