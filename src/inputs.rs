use mirajazz::{error::MirajazzError, types::DeviceInput};

use crate::mappings::{ENCODER_COUNT, KEY_COUNT};

// Device input codes for the Mirabox N1, determined from hardware:
//   0x01..=0x0f  15 LCD keys, row-major (top-left -> bottom-right). state 1=down, 0=up
//   0x32 / 0x33  knob rotation, one event per detent (left / right), state always 0
//   0x23         knob press (state 1/0). NOTE: also appears as part of the boot handshake
//                with state=2, which we ignore
//   0x1e         extra button A (state 1/0)
//   0x1f         extra button B (state 1/0)
//   0xcc / 0xaa  boot handshake noise, ignored
//
// Encoders exposed to OpenDeck, ordered left-to-right to match the physical layout
// (two buttons on the left, knob on the right):
//   0 = button A, 1 = button B, 2 = knob (twist + press).
const ENC_BUTTON_A: usize = 0;
const ENC_BUTTON_B: usize = 1;
const ENC_KNOB: usize = 2;

pub fn process_input(input: u8, state: u8) -> Result<DeviceInput, MirajazzError> {
    log::debug!("Processing input: code=0x{:02x} state={}", input, state);

    match input {
        0x01..=0x0f => read_button_press(input, state),
        0x32 | 0x33 => read_encoder_twist(input),
        0x23 => read_encoder_press(ENC_KNOB, state),     // knob press
        0x1e => read_encoder_press(ENC_BUTTON_A, state), // button A
        0x1f => read_encoder_press(ENC_BUTTON_B, state), // button B
        // Boot handshake (0xcc/0xaa) and anything unexpected: non-fatal, just ignore.
        _ => Err(MirajazzError::BadData),
    }
}

/// LCD key press. Device codes 0x01..0x0f already match OpenDeck's row-major order,
/// so the logical index is simply `code - 1`.
fn read_button_press(input: u8, state: u8) -> Result<DeviceInput, MirajazzError> {
    let key = (input as usize) - 1;

    if key >= KEY_COUNT {
        return Err(MirajazzError::BadData);
    }

    let mut states = vec![false; KEY_COUNT];
    states[key] = state != 0;

    Ok(DeviceInput::ButtonStateChange(states))
}

/// Knob rotation: 0x32 = left (-1), 0x33 = right (+1). One detent per event.
fn read_encoder_twist(input: u8) -> Result<DeviceInput, MirajazzError> {
    let mut values = vec![0i8; ENCODER_COUNT];

    values[ENC_KNOB] = match input {
        0x32 => -1,
        0x33 => 1,
        _ => return Err(MirajazzError::BadData),
    };

    Ok(DeviceInput::EncoderTwist(values))
}

/// Press of the knob (encoder 0) or one of the extra buttons (encoders 1/2).
fn read_encoder_press(encoder: usize, state: u8) -> Result<DeviceInput, MirajazzError> {
    // Real presses report state 0/1. The boot handshake emits 0x23 with state=2, ignore it.
    if state > 1 {
        return Err(MirajazzError::BadData);
    }

    let mut states = vec![false; ENCODER_COUNT];
    states[encoder] = state != 0;

    Ok(DeviceInput::EncoderStateChange(states))
}
