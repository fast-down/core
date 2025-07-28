pub fn format_time(time: u64) -> String {
    let seconds = time % 60;
    let minutes = (time / 60) % 60;
    let hours = time / 3600;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_time() {
        assert_eq!(format_time(0), "00:00:00");
        assert_eq!(format_time(59), "00:00:59");
        assert_eq!(format_time(60), "00:01:00");
        assert_eq!(format_time(3599), "00:59:59");
        assert_eq!(format_time(3600), "01:00:00");
        assert_eq!(format_time(3661), "01:01:01");
        assert_eq!(format_time(86399), "23:59:59");
    }
}
