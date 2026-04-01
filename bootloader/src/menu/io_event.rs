use core::time::Duration;

use uefi::proto::console::{
    serial::Serial,
    text::{Input, Key, ScanCode},
};

pub enum WaitEvent {
    Keypress(Option<Key>), // None for any key
    Timeout(u64),
    KeypressAndTimeout { keypress: Option<Key>, timeout: u64 },
    KeypressOrTimeout { keypress: Option<Key>, timeout: u64 },
}

impl WaitEvent {
    pub fn try_read_serial() -> Option<Key> {
        if let Ok(handle) = uefi::boot::get_handle_for_protocol::<Serial>() {
            if let Ok(mut serial) = uefi::boot::open_protocol_exclusive::<Serial>(handle) {
                let mut buf = [0u8; 1];

                if serial.read(&mut buf).is_err() {
                    return None;
                }
                let b = buf[0];

                if b == 0x1B {
                    for _ in 0..3 {
                        uefi::boot::stall(core::time::Duration::from_micros(100));
                        if serial.read(&mut buf).is_ok() {
                            let b2 = buf[0];
                            if b2 == b'[' {
                                for _ in 0..5 {
                                    uefi::boot::stall(core::time::Duration::from_micros(100));
                                    if serial.read(&mut buf).is_ok() {
                                        match buf[0] {
                                            b'A' => return Some(Key::Special(ScanCode::UP)),
                                            b'B' => return Some(Key::Special(ScanCode::DOWN)),
                                            b'C' => {
                                                return Some(Key::Special(ScanCode::ESCAPE));
                                            }
                                            b'D' => {
                                                return Some(Key::Special(ScanCode::ESCAPE));
                                            }
                                            _ => return Some(Key::Special(ScanCode::ESCAPE)),
                                        }
                                    }
                                }
                                return Some(Key::Special(ScanCode::ESCAPE));
                            } else {
                                return Some(Key::Special(ScanCode::ESCAPE));
                            }
                        }
                    }
                    return Some(Key::Special(ScanCode::ESCAPE));
                }

                if b == b'\r' || b == b'\n' {
                    return Some(Key::Printable('\r'.try_into().unwrap()));
                }

                if (0x20..=0x7E).contains(&b) {
                    let c = b as char;
                    return Some(Key::Printable(c.try_into().unwrap()));
                }
            }
        }
        None
    }

    pub fn try_read_uefi() -> Option<Key> {
        if let Ok(key_handle) = uefi::boot::get_handle_for_protocol::<Input>() {
            if let Ok(mut input) = uefi::boot::open_protocol_exclusive::<Input>(key_handle) {
                return input.read_key().ok().flatten();
            }
        }
        None
    }

    pub fn poll(&self) -> Option<Key> {
        let target = match self {
            WaitEvent::Keypress(k) => k,
            WaitEvent::KeypressAndTimeout { keypress, .. } => keypress,
            WaitEvent::KeypressOrTimeout { keypress, .. } => keypress,
            _ => return None,
        };

        if let Some(k) = Self::try_read_uefi() {
            if target.is_none() || target.as_ref() == Some(&k) {
                return Some(k);
            }
        }

        if let Some(k) = Self::try_read_serial() {
            if target.is_none() || target.as_ref() == Some(&k) {
                return Some(k);
            }
        }
        None
    }

    pub fn wait(self) -> Option<Key> {
        const POLL_INTERVAL_MS: u64 = 5;

        let (target_key, timeout_ms, wait_both) = match self {
            WaitEvent::Keypress(k) => (Some(k), None, false),
            WaitEvent::Timeout(ms) => (None, Some(ms), false),
            WaitEvent::KeypressAndTimeout { keypress, timeout } => {
                (Some(keypress), Some(timeout), true)
            }
            WaitEvent::KeypressOrTimeout { keypress, timeout } => {
                (Some(keypress), Some(timeout), false)
            }
        };

        let mut key_received = false;
        let mut timeout_expired = false;
        let mut received_key_value = None;
        let mut elapsed_ms: u64 = 0;

        loop {
            // Check Key
            if target_key.is_some() && !key_received {
                // Poll Input
                if let Some(k) = Self::try_read_uefi() {
                    let required_key = target_key.unwrap();
                    let matches = match required_key {
                        Some(tk) => k == tk,
                        None => true,
                    };
                    if matches {
                        key_received = true;
                        received_key_value = Some(k);
                    }
                }
                // Poll Serial
                if !key_received {
                    if let Some(k) = Self::try_read_serial() {
                        let required_key = target_key.unwrap();
                        let matches = match required_key {
                            Some(tk) => k == tk,
                            None => true,
                        };
                        if matches {
                            key_received = true;
                            received_key_value = Some(k);
                        }
                    }
                }
            }

            // Check Timeout
            if let Some(limit) = timeout_ms {
                if elapsed_ms >= limit {
                    timeout_expired = true;
                }
            }

            // Check Exit Conditions
            if wait_both {
                if key_received && timeout_expired {
                    return received_key_value;
                }
            } else {
                if key_received {
                    return received_key_value;
                }
                if timeout_expired {
                    return None;
                }
            }

            uefi::boot::stall(Duration::from_millis(POLL_INTERVAL_MS));
            elapsed_ms += POLL_INTERVAL_MS;
        }
    }
}
