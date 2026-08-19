use std::time::{SystemTime, UNIX_EPOCH};

pub fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after Unix epoch")
        .as_millis() as i64
}
