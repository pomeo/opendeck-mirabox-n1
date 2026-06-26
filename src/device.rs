use data_url::DataUrl;
use image::load_from_memory_with_format;
use mirajazz::{device::Device, error::MirajazzError, state::DeviceStateUpdate};
use openaction::{OUTBOUND_EVENT_MANAGER, SetImageEvent};
use std::time::{Duration, SystemTime};
use tokio_util::sync::CancellationToken;

use crate::{
    DEVICES, TOKENS,
    mappings::{COL_COUNT, CandidateDevice, ENCODER_COUNT, KEY_COUNT, Kind, ROW_COUNT},
};

/// Keep-alive loop period.
const KEEPALIVE_PERIOD: Duration = Duration::from_secs(2);

/// If the real-time gap between two ticks exceeds this, we assume the host was suspended (the
/// monotonic timer driving the loop is frozen during sleep while the system clock keeps real
/// time) and fully reconnect the device, which the N1 firmware needs after a resume.
const RESUME_GAP: Duration = Duration::from_secs(30);

/// Connects to a device and runs the init sequence that switches it into image mode.
async fn connect_and_init(candidate: &CandidateDevice) -> Result<Device, MirajazzError> {
    let device = connect(candidate).await?;

    // Initialize first (set_brightness triggers it), then switch the device into its image
    // mode — doing it in this order keeps the init sequence from undoing the mode switch.
    device.set_brightness(50).await?;
    if let Some(mode) = candidate.kind.mode() {
        device.set_mode(mode).await?;
    }
    device.clear_all_button_images().await?;
    device.flush().await?;

    Ok(device)
}

/// Resolves once a resume-from-suspend is detected: a wall-clock gap between ticks far larger
/// than the polling period (the monotonic timer is frozen while the host sleeps).
async fn wait_for_resume() {
    let mut last_tick = SystemTime::now();

    loop {
        tokio::time::sleep(KEEPALIVE_PERIOD).await;

        let now = SystemTime::now();
        let elapsed = now.duration_since(last_tick).unwrap_or(Duration::ZERO);
        last_tick = now;

        if elapsed >= RESUME_GAP {
            return;
        }
    }
}

/// Initializes a device and listens for events, fully reconnecting after a resume from suspend.
pub async fn device_task(candidate: CandidateDevice, token: CancellationToken) {
    log::info!("Running device task for {:?}", candidate);

    loop {
        let device = match connect_and_init(&candidate).await {
            Ok(device) => device,
            Err(err) => {
                handle_error(&candidate.id, err).await;

                log::error!(
                    "Had error during device init, finishing device task: {:?}",
                    candidate
                );

                return;
            }
        };

        log::info!("Registering device {}", candidate.id);
        if let Some(outbound) = OUTBOUND_EVENT_MANAGER.lock().await.as_mut() {
            outbound
                .register_device(
                    candidate.id.clone(),
                    candidate.kind.human_name(),
                    ROW_COUNT as u8,
                    COL_COUNT as u8,
                    ENCODER_COUNT as u8,
                    0,
                )
                .await
                .unwrap();
        }

        DEVICES.write().await.insert(candidate.id.clone(), device);

        // After a resume the N1 firmware is back in its default mode and won't deliver input over
        // the existing handle, so we tear everything down and reconnect from scratch — an in-place
        // re-init leaves the stale reader and the firmware's default screen in place.
        let resumed = tokio::select! {
            _ = device_events_task(&candidate) => false,
            _ = device_keepalive_task(&candidate.id, token.clone()) => false,
            _ = wait_for_resume() => true,
            _ = token.cancelled() => false,
        };

        log::info!("Shutting down device {:?}", candidate);

        if let Some(device) = DEVICES.write().await.remove(&candidate.id) {
            device.shutdown().await.ok();
        }

        if resumed && !token.is_cancelled() {
            log::info!("Resumed from suspend, reconnecting device {}", candidate.id);

            // Drop OpenDeck's registration so it re-registers and repaints (clearing the firmware's
            // default screen) once we reconnect on the next loop iteration.
            if let Some(outbound) = OUTBOUND_EVENT_MANAGER.lock().await.as_mut() {
                outbound.deregister_device(candidate.id.clone()).await.ok();
            }

            continue;
        }

        break;
    }

    log::info!("Device task finished for {:?}", candidate);
}

/// The N1 re-enumerates / drops off the bus if the host stops talking to it, so we send a
/// periodic keep-alive ("CONNECT") ping to keep the connection stable.
async fn device_keepalive_task(id: &String, token: CancellationToken) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(KEEPALIVE_PERIOD) => {}
            _ = token.cancelled() => return,
        }

        let result = {
            let guard = DEVICES.read().await;
            match guard.get(id) {
                Some(device) => device.keep_alive().await,
                None => return,
            }
        };

        if let Err(err) = result {
            handle_error(id, err).await;
            return;
        }
    }
}

/// Handles errors, returning true if should continue, returning false if an error is fatal
pub async fn handle_error(id: &String, err: MirajazzError) -> bool {
    log::error!("Device {} error: {}", id, err);

    // Some errors are not critical and can be ignored without sending disconnected event
    if matches!(err, MirajazzError::ImageError(_) | MirajazzError::BadData) {
        return true;
    }

    log::info!("Deregistering device {}", id);
    if let Some(outbound) = OUTBOUND_EVENT_MANAGER.lock().await.as_mut() {
        outbound.deregister_device(id.clone()).await.unwrap();
    }

    log::info!("Cancelling tasks for device {}", id);
    if let Some(token) = TOKENS.read().await.get(id) {
        token.cancel();
    }

    log::info!("Removing device {} from the list", id);
    DEVICES.write().await.remove(id);

    log::info!("Finished clean-up for {}", id);

    false
}

pub async fn connect(candidate: &CandidateDevice) -> Result<Device, MirajazzError> {
    let firmware_version = Device::read_firmware_version(&candidate.dev).await;

    let firmware_version = match firmware_version {
        Ok(fw) => fw,
        Err(e) => {
            log::error!("Failed to read firmware version from {}", &candidate.id);

            return Err(e);
        }
    };

    log::info!(
        "Connecting to {} with fw {:?}",
        &candidate.id,
        &firmware_version
    );

    let result = Device::connect(
        &candidate.dev,
        candidate.kind.protocol_version(),
        KEY_COUNT,
        ENCODER_COUNT,
    )
    .await;

    match result {
        Ok(device) => Ok(device),
        Err(e) => {
            log::error!("Error while connecting to device: {e}");

            Err(e)
        }
    }
}

/// Handles events from device to OpenDeck
async fn device_events_task(candidate: &CandidateDevice) -> Result<(), MirajazzError> {
    log::info!("Connecting to {} for incoming events", candidate.id);

    let devices_lock = DEVICES.read().await;
    let reader = match devices_lock.get(&candidate.id) {
        Some(device) => device.get_reader(crate::inputs::process_input),
        None => return Ok(()),
    };
    drop(devices_lock);

    log::info!("Connected to {} for incoming events", candidate.id);

    log::info!("Reader is ready for {}", candidate.id);

    loop {
        log::info!("Reading updates...");

        let updates = match reader.read(None).await {
            Ok(updates) => updates,
            Err(e) => {
                if !handle_error(&candidate.id, e).await {
                    break;
                }

                continue;
            }
        };

        for update in updates {
            log::info!("New update: {:#?}", update);

            let id = candidate.id.clone();

            if let Some(outbound) = OUTBOUND_EVENT_MANAGER.lock().await.as_mut() {
                match update {
                    DeviceStateUpdate::ButtonDown(key) => outbound.key_down(id, key).await.unwrap(),
                    DeviceStateUpdate::ButtonUp(key) => outbound.key_up(id, key).await.unwrap(),
                    DeviceStateUpdate::EncoderDown(encoder) => {
                        outbound.encoder_down(id, encoder).await.unwrap();
                    }
                    DeviceStateUpdate::EncoderUp(encoder) => {
                        outbound.encoder_up(id, encoder).await.unwrap();
                    }
                    DeviceStateUpdate::EncoderTwist(encoder, val) => {
                        outbound
                            .encoder_change(id, encoder, val as i16)
                            .await
                            .unwrap();
                    }
                }
            }
        }
    }

    Ok(())
}

/// Handles different combinations of "set image" event, including clearing the specific buttons and whole device
pub async fn handle_set_image(device: &Device, evt: SetImageEvent) -> Result<(), MirajazzError> {
    let kind = Kind::from_vid_pid(device.vid, device.pid).unwrap(); // Safe: device is already filtered

    // Encoder ("knob"/button) images are drawn on the screen-strip segments, which live at device
    // indices right after the keys. Keypad images use the position as-is.
    let is_encoder = evt.controller.as_deref() == Some("Encoder");
    let device_index = |position: u8| -> u8 {
        if is_encoder {
            KEY_COUNT as u8 + position
        } else {
            position
        }
    };
    let format = if is_encoder {
        kind.encoder_image_format()
    } else {
        kind.image_format()
    };

    match (evt.position, evt.image) {
        (Some(position), Some(image)) => {
            log::info!("Setting image for {} {}", if is_encoder { "encoder" } else { "button" }, position);

            // OpenDeck sends image as a data url, so parse it using a library
            let url = DataUrl::process(image.as_str()).unwrap(); // Isn't expected to fail, so unwrap it is
            let (body, _fragment) = url.decode_to_vec().unwrap(); // Same here

            // Allow only image/jpeg mime for now
            if url.mime_type().subtype != "jpeg" {
                log::error!("Incorrect mime type: {}", url.mime_type());

                return Ok(()); // Not a fatal error, enough to just log it
            }

            let image = load_from_memory_with_format(body.as_slice(), image::ImageFormat::Jpeg)?;

            device
                .set_button_image(device_index(position), format, image)
                .await?;
            device.flush().await?;
        }
        (Some(position), None) => {
            device.clear_button_image(device_index(position)).await?;
            device.flush().await?;
        }
        (None, None) => {
            device.clear_all_button_images().await?;
            device.flush().await?;
        }
        _ => {}
    }

    Ok(())
}
